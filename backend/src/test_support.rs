use crate::{config::Vault, db, llm, model::*, runner::*};
use anyhow::{Context, Result};
use sqlx::MySqlPool;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64},
    },
    time::Instant,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// Ignored database tests require an explicitly selected, disposable local server.
pub async fn pool(max_connections: u32) -> Result<MySqlPool> {
    let admin = db::connect(
        &DbConfig {
            database: "doccraft_agent_test".into(),
            port: std::env::var("DOCCRAFT_TEST_DB_PORT")
                .context("Set DOCCRAFT_TEST_DB_PORT to a disposable local MariaDB instance")?
                .parse()?,
            password: std::env::var("DOCCRAFT_TEST_DB_PASSWORD").unwrap_or_default(),
            max_connections,
            ..Default::default()
        },
        true,
    )
    .await?;
    let name = format!("doccraft_agent_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE `{name}` CHARACTER SET utf8mb4"))
        .execute(&admin)
        .await?;
    let pool = admin
        .options()
        .clone()
        .connect_with((*admin.connect_options()).clone().database(&name))
        .await?;
    sqlx::migrate!().run(&pool).await?;
    admin.close().await;
    Ok(pool)
}

pub async fn close(pool: MySqlPool) -> Result<()> {
    let options = pool.connect_options();
    let name = options.get_database().context("Missing test database")?;
    anyhow::ensure!(
        name.starts_with("doccraft_agent_test_")
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "Not an isolated test database"
    );
    sqlx::query(&format!("DROP DATABASE `{name}`"))
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

pub struct TestRun {
    pub ctx: RunContext,
    _data: tempfile::TempDir,
}

impl TestRun {
    pub fn new(pool: MySqlPool) -> Result<Self> {
        let data = tempfile::tempdir()?;
        let root = data.path().canonicalize()?;
        let source = root.join("source.rs");
        std::fs::write(&source, "fn main() {}")?;
        let settings = Settings {
            source_roots: vec![root.to_string_lossy().into()],
            output_roots: vec![root.to_string_lossy().into()],
            ..Default::default()
        };
        let snapshot = RunSnapshot {
            task: TaskConfig {
                id: uuid::Uuid::new_v4().to_string(),
                name: "Concurrency regression".into(),
                direction: "Document the source".into(),
                sources: vec![source.to_string_lossy().into()],
                target: root.join("out.md").to_string_lossy().into(),
                ..Default::default()
            },
            settings: settings.clone(),
            original_hash: None,
        };
        let state = Arc::new(AppState::new(
            Vault::open(root.join("vault"))?,
            settings,
            Some(pool.clone()),
        ));
        Ok(Self {
            ctx: RunContext {
                client: llm::client(&snapshot.settings.llm)?,
                state,
                pool,
                snapshot,
                id: uuid::Uuid::new_v4().to_string(),
                cancel: CancellationToken::new(),
                gate: Arc::new(CommitGate {
                    lock: Mutex::new(()),
                    published: AtomicBool::new(false),
                }),
                started: Instant::now(),
                elapsed_before: 0,
                finalizing: AtomicBool::new(false),
                reserved_tokens: AtomicU64::new(0),
                reserved_cost: AtomicU64::new(0),
                extra_margin: AtomicU32::new(0),
                graph_index: tokio::sync::OnceCell::new(),
                cross_links: tokio::sync::OnceCell::new(),
            },
            _data: data,
        })
    }
}
