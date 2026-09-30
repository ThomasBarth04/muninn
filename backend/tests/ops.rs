//! Running it in production: the SPA's caching, /healthz, housekeeping and a
//! job worker that stops cleanly.

mod common;

use std::time::Duration;

use uuid::Uuid;

#[tokio::test]
async fn spa_assets_are_immutable_and_index_is_revalidated() {
    let dir = std::env::temp_dir().join(format!("muninn-spa-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html>app").unwrap();
    std::fs::write(dir.join("assets/index-abc123.js"), "console.log(1)").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let spa = muninn::spa(dir.to_str().unwrap());
    tokio::spawn(async move { axum::serve(listener, spa).await.unwrap() });

    let get = |path: &str| reqwest::get(format!("{url}{path}"));
    let cache = |r: &reqwest::Response| r.headers()["cache-control"].to_str().unwrap().to_string();

    let asset = get("/assets/index-abc123.js").await.unwrap();
    assert_eq!(asset.status(), 200);
    assert_eq!(cache(&asset), "public, max-age=31536000, immutable");

    // An asset from the previous deploy: 404, never index.html cached forever.
    let gone = get("/assets/index-old999.js").await.unwrap();
    assert_eq!(gone.status(), 404);
    assert_eq!(cache(&gone), "no-cache");

    for path in ["/", "/inbox/unassigned"] {
        let page = get(path).await.unwrap();
        assert_eq!(page.status(), 200, "{path}");
        assert_eq!(cache(&page), "no-cache", "{path}");
        assert_eq!(page.text().await.unwrap(), "<!doctype html>app");
    }
}

/// A workspace with one agent, arranged as the owner role.
async fn workspace(app: &common::TestApp) -> (Uuid, Uuid) {
    let ws = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, language, trial_ends_at)
         VALUES ($1, 'Acme', 'acme', 'english', now() + interval '14 days')",
    )
    .bind(ws)
    .execute(&app.owner)
    .await
    .unwrap();
    let agent: Uuid = sqlx::query_scalar(
        "INSERT INTO agents (workspace_id, email, name, role) VALUES ($1, 'frank@acme.com', 'Frank', 'owner') RETURNING id",
    )
    .bind(ws)
    .fetch_one(&app.owner)
    .await
    .unwrap();
    (ws, agent)
}

#[tokio::test]
async fn healthz_reports_failed_jobs() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let mut c = app.client();
    let ok = c.get("/healthz").await;
    assert_eq!((ok.status, ok.body.as_str()), (200, Some("ok\n")));

    let (ws, _) = workspace(&app).await;
    sqlx::query(
        "INSERT INTO jobs (workspace_id, kind, failed_at, last_error) VALUES ($1, 'seats', now(), 'boom')",
    )
    .bind(ws)
    .execute(&app.owner)
    .await
    .unwrap();
    let bad = c.get("/healthz").await;
    assert_eq!(bad.status, 503);
    assert!(bad.body.as_str().unwrap().contains("a job failed"));

    // An hour later it is history, not an outage.
    sqlx::query("UPDATE jobs SET failed_at = now() - interval '2 hours'")
        .execute(&app.owner)
        .await
        .unwrap();
    assert_eq!(c.get("/healthz").await.status, 200);
}

#[tokio::test]
async fn housekeeping_deletes_expired_links_and_sessions() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, agent) = workspace(&app).await;
    for (hash, expires) in [
        ("old", "now() - interval '2 days'"),
        ("fresh", "now() - interval '1 hour'"),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO auth_links (token_hash, purpose, email, workspace_id, workspace_name, expires_at)
             VALUES ('{hash}'::bytea, 'setup', 'frank@acme.com', '{ws}', 'Acme', {expires})"
        )))
        .execute(&app.owner)
        .await
        .unwrap();
    }
    for (hash, used) in [
        ("old", "now() - interval '31 days'"),
        ("fresh", "now() - interval '29 days'"),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO sessions (token_hash, workspace_id, agent_id, last_used_at)
             VALUES ('{hash}'::bytea, '{ws}', '{agent}', {used})"
        )))
        .execute(&app.owner)
        .await
        .unwrap();
    }

    // As the app role, the way the worker runs it.
    muninn::jobs::clean(&app.st.db).await.unwrap();

    for table in ["auth_links", "sessions"] {
        let left: Vec<Vec<u8>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT token_hash FROM {table}"
        )))
        .fetch_all(&app.owner)
        .await
        .unwrap();
        assert_eq!(left, vec![b"fresh".to_vec()], "{table}");
    }
}

#[tokio::test]
async fn worker_finishes_its_jobs_then_stops() {
    let Some(app) = common::spawn().await else {
        return;
    };
    let (ws, _) = workspace(&app).await;
    let jobs = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(&app.owner)
            .await
            .unwrap()
    };

    // A seats job for a trial workspace is a no-op that succeeds.
    sqlx::query("INSERT INTO jobs (workspace_id, kind) VALUES ($1, 'seats')")
        .bind(ws)
        .execute(&app.owner)
        .await
        .unwrap();
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(muninn::jobs::worker(app.st.clone(), stopping));
    for _ in 0..100 {
        if jobs().await == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(jobs().await, 0);

    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("worker stops")
        .unwrap();

    // Stopped means stopped: nothing new is claimed.
    sqlx::query("INSERT INTO jobs (workspace_id, kind) VALUES ($1, 'seats')")
        .bind(ws)
        .execute(&app.owner)
        .await
        .unwrap();
    let (_stop, stopped) = tokio::sync::watch::channel(true);
    tokio::time::timeout(
        Duration::from_secs(5),
        muninn::jobs::worker(app.st.clone(), stopped),
    )
    .await
    .expect("a stopped worker returns at once");
    let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM jobs")
        .fetch_one(&app.owner)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
}
