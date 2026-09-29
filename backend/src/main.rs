use muninn::{AppState, Config, jobs, router};
use sqlx::postgres::PgPoolOptions;

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

    tokio::spawn(jobs::worker(state.clone()));

    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:3000".into());
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    tracing::info!("listening on {bind}");
    axum::serve(listener, router(state)).await.expect("serve");
}
