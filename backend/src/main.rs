use muninn::{AppState, Config, jobs, router};
use sqlx::postgres::PgPoolOptions;
use tokio::signal::unix::{SignalKind, signal};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muninn=info,tower_http=info".into()),
        )
        .init();

    // Migrations run as the owner role; the app itself never holds it (ADR 0003).
    if let Ok(url) = std::env::var("MIGRATE_DATABASE_URL") {
        let owner = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("owner db");
        sqlx::migrate!().run(&owner).await.expect("migrations");
        owner.close().await;
    }

    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let db = PgPoolOptions::new()
        .max_connections(20)
        .connect(&url)
        .await
        .expect("db");
    let state = AppState::new(db, Config::from_env());

    let (stop, stopping) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(jobs::worker(state.clone(), stopping.clone()));
    tokio::spawn(jobs::housekeeping(state.clone()));
    tokio::spawn(async move {
        shutdown_signal().await;
        stop.send_replace(true);
    });

    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    tracing::info!("listening on {bind}");
    let mut http_stopping = stopping;
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            let _ = http_stopping.wait_for(|s| *s).await;
        })
        .await
        .expect("serve");
    worker.await.expect("job worker");
    state.db.close().await;
    tracing::info!("stopped");
}

/// `docker compose` sends SIGTERM and kills after `stop_grace_period`. As PID 1
/// the process has no default handler, so without this it would ignore the
/// signal and be killed mid-job.
async fn shutdown_signal() {
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("shutting down: finishing requests and running jobs");
}
