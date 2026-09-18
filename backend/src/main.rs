#![forbid(unsafe_code)]
//! DocCraft Agent — turns local source code into purpose-built Markdown.
//!
//! A run moves through the modules in this order, each stage checkpointed so an
//! interrupted run resumes where it stopped:
//!
//! ```text
//! source      snapshot and index the selected roots
//! understanding  read every chunk, synthesize it hierarchically
//! purpose     narrow that reading to what the requested document needs
//! planning    turn it into an outline, then review the outline
//! runner      write each section, review the document, repair what it flags
//! publish     validate diagrams and save atomically
//! ```
//!
//! `runner` owns the lifecycle and drives that sequence; `api` exposes it over
//! HTTP; `llm`, `budget`, `db` and `graph` are the shared services beneath.
mod api;
mod budget;
mod code_graph;
mod composition;
mod config;
mod context;
mod db;
mod editorial;
mod findings;
mod graph;
mod llm;
mod maintenance;
mod markdown;
mod model;
mod parser;
mod planning;
mod publish;
mod purpose;
mod runner;
mod section_output;
mod source;
#[cfg(test)]
mod test_support;
mod understanding;

use anyhow::{Context, Result};
use fs2::FileExt;
use std::{path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "--openapi") {
        println!("{}", api::openapi().to_pretty_json()?);
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "--parse-worker") {
        let path = args.get(2).context("Missing worker input")?;
        let language = args.get(3).context("Missing worker language")?;
        println!(
            "{}",
            serde_json::to_string(&parser::parse_file(std::path::Path::new(path), language)?)?
        );
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "doccraft_agent=info,tower_http=warn".into()),
        )
        .init();
    let dir = PathBuf::from(std::env::var("DOCCRAFT_DATA_DIR").unwrap_or_else(|_| ".local".into()));
    let vault = config::Vault::open(dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(vault.dir.join("instance.lock"))?;
    lock.try_lock_exclusive()
        .context("Another DocCraftAgent instance uses this data directory")?;
    let settings = vault.load()?;
    config::validate(&settings)?;
    let pool = match db::connect(&settings.db, true).await {
        Ok(pool) => Some(pool),
        Err(_) => {
            tracing::warn!(
                "Database unavailable; open Settings to configure the local MariaDB connection"
            );
            None
        }
    };
    let state = Arc::new(context::AppState::new(vault, settings, pool));
    runner::recover(state.clone()).await?;
    maintenance::start(state.clone());
    let app = api::router(state.clone());
    let port = std::env::var("DOCCRAFT_PORT")
        .unwrap_or_else(|_| "8765".into())
        .parse::<u16>()?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    tracing::info!("DocCraftAgent listening at http://127.0.0.1:{port}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            state.shutdown.cancel();
            state.stop_all().await;
        })
        .await?;
    Ok(())
}
