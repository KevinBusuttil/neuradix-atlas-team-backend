use std::sync::Arc;

use atlas_team_backend::store::{MemStore, PgStore, Store};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mem_mode = std::env::args().any(|arg| arg == "--mem");
    let store: Arc<dyn Store> = if mem_mode {
        tracing::warn!("running with the in-memory store; all state is lost on exit");
        Arc::new(MemStore::new())
    } else if let Ok(database_url) = std::env::var("DATABASE_URL") {
        tracing::info!("connecting to PostgreSQL and applying migrations");
        Arc::new(PgStore::connect(&database_url).await?)
    } else {
        anyhow::bail!("set DATABASE_URL, or pass --mem for the in-memory dev mode");
    };

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("atlas-team-backend listening on port {port}");
    // ConnectInfo gives the rate limiter the socket peer address to key on
    // when ATLAS_TRUST_PROXY=0 (no proxy in front appending X-Forwarded-For).
    axum::serve(
        listener,
        atlas_team_backend::router(store)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
