mod agent;
mod api;
mod controller;
mod fingerprint;
mod llm;
mod model;
mod store;

use std::sync::Arc;
use store::GraphStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,phalaenopsis=debug".into()))
        .init();

    let store: Arc<dyn GraphStore> = match std::env::var("PHALAENOPSIS_STORE").as_deref() {
        Ok("memory") => {
            tracing::warn!("using in-memory store: nothing will persist across restarts");
            Arc::new(store::memory::MemoryStore::new())
        }
        _ => {
            let cfg = store::typedb::TypeDbConfig::from_env();
            tracing::info!("connecting to TypeDB at {} (database {})", cfg.addr, cfg.database);
            Arc::new(store::typedb::TypeDbStore::connect(&cfg).await?)
        }
    };

    let llm = llm::LlmClient::from_env();
    match &llm {
        Some(l) => tracing::info!("{}: interaction {} · background {}", l.provider.name(), l.fast_model, l.model),
        None => tracing::warn!("no OPENROUTER_KEY or OPENAI_KEY set: the agent and cold-start scripts are disabled"),
    }

    let controller = Arc::new(controller::MemoryController::new(store, llm));
    let app = api::router(api::AppState { controller });
    let addr = std::env::var("PHALAENOPSIS_ADDR").unwrap_or_else(|_| "127.0.0.1:7777".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
