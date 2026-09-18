//! The state a run carries while it executes.
//!
//! `AppState` is the process: the settings vault, the database pool, and the
//! table of runs currently executing. `RunContext` is one run inside it, and it
//! is what every stage of the pipeline is handed.
//!
//! A `RunContext` is more than a bag of handles. It holds the run's budget, so
//! `reserve` and `check` are how a stage discovers it must stop; it holds the
//! measured byte-to-token density, so `packing_limit` answers how much input
//! actually fits; and `event` is how a stage reports progress to the screen.
//! Stages therefore take `&RunContext` rather than separate arguments, and
//! cancellation and budget exhaustion reach them all the same way.

use crate::{config::Vault, model::*};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OnceCell, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

pub struct CommitGate {
    pub lock: Mutex<()>,
    pub published: std::sync::atomic::AtomicBool,
}
pub struct Control {
    pub token: CancellationToken,
    pub target: String,
    pub gate: Arc<CommitGate>,
}
pub struct AppState {
    pub vault: Vault,
    pub settings: RwLock<Settings>,
    pub pool: RwLock<Option<MySqlPool>>,
    pub controls: Mutex<HashMap<String, Control>>,
    pub jobs: Semaphore,
    pub llm_slots: Semaphore,
    pub lifecycle: Semaphore,
    pub rate: Mutex<VecDeque<(Instant, u64)>>,
    pub shutdown: CancellationToken,
    pub session: String,
    pub node_version: OnceCell<Option<String>>,
}
impl AppState {
    pub fn new(vault: Vault, settings: Settings, pool: Option<MySqlPool>) -> Self {
        let jobs = Semaphore::new(settings.max_jobs);
        let llm_slots = Semaphore::new(settings.llm.concurrency);
        Self {
            vault,
            settings: RwLock::new(settings),
            pool: RwLock::new(pool),
            controls: Mutex::new(HashMap::new()),
            jobs,
            llm_slots,
            lifecycle: Semaphore::new(1),
            rate: Mutex::new(VecDeque::new()),
            shutdown: CancellationToken::new(),
            session: uuid::Uuid::new_v4().to_string(),
            node_version: OnceCell::new(),
        }
    }
    pub async fn db(&self) -> Result<MySqlPool> {
        self.pool
            .read()
            .await
            .clone()
            .context("Database unavailable; configure and test MariaDB in Settings")
    }
    pub async fn stop_all(&self) {
        for c in self.controls.lock().await.values() {
            c.token.cancel();
        }
    }
}
pub struct RunContext {
    pub state: Arc<AppState>,
    pub pool: MySqlPool,
    pub id: String,
    pub snapshot: RunSnapshot,
    pub cancel: CancellationToken,
    pub gate: Arc<CommitGate>,
    pub client: reqwest::Client,
    pub started: Instant,
    pub elapsed_before: u64,
    pub finalizing: std::sync::atomic::AtomicBool,
    pub reserved_tokens: AtomicU64,
    pub reserved_cost: AtomicU64,
    pub extra_margin: AtomicU32,
    /// Every file's call graph, built once. Name resolution needs a project-wide
    /// view and a run asks for one per section and per retrieval, so rebuilding
    /// it each time turns linear work into linear work repeated a hundred times.
    pub graph_index: tokio::sync::OnceCell<std::sync::Arc<Vec<crate::graph::FileGraph>>>,
    /// Call targets resolved to their declaring file, folded across call sites
    /// and built once. A reduction reads summaries rather than source, so the
    /// links between its children are the one thing it cannot recover from the
    /// request, and every node of the reduction tree asks for them.
    pub cross_links: tokio::sync::OnceCell<std::sync::Arc<crate::graph::LinkIndex>>,
    /// The reading tree without its source text, built once the reading is done.
    pub tree_index: tokio::sync::OnceCell<std::sync::Arc<crate::understanding::TreeIndex>>,
    /// Set once the provider has rejected a request carrying `response_format`.
    /// Not every OpenAI-compatible server implements JSON mode, and asking a
    /// server that does not on every later call would fail the whole run.
    pub json_mode_off: std::sync::atomic::AtomicBool,
    /// Usage reports that measured how many request bytes make one token, or
    /// `u32::MAX` once the provider has rejected a request the calibration
    /// packed, after which this run stays at one byte a token.
    pub density_samples: AtomicU32,
    /// The densest of those samples, in hundredths of a byte per token.
    pub density_floor: AtomicU32,
}
impl RunContext {
    /// Bytes per token, in hundredths, this run packs and estimates at.
    pub fn density(&self) -> u32 {
        let samples = self.density_samples.load(Ordering::Relaxed);
        if samples == u32::MAX {
            return crate::budget::UNCALIBRATED_DENSITY;
        }
        crate::budget::calibrated_density(samples, self.density_floor.load(Ordering::Relaxed))
    }
    /// Raw bytes a request may pack alongside `overhead` at this run's density.
    pub fn packing_limit(&self, overhead: usize) -> usize {
        crate::budget::packing_limit_at(
            &self.snapshot.settings.llm,
            self.extra_margin.load(Ordering::Relaxed),
            self.density(),
            overhead,
        )
    }
    /// Learn from one usage report; returns whether it was a usable sample.
    pub fn record_density(&self, request_bytes: u64, prompt_tokens: u64) -> bool {
        if self.density_samples.load(Ordering::Relaxed) == u32::MAX {
            return false;
        }
        let Some(sample) = crate::budget::density_sample(request_bytes, prompt_tokens) else {
            return false;
        };
        let _ = self
            .density_floor
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |floor| {
                Some(if floor == 0 {
                    sample
                } else {
                    floor.min(sample)
                })
            });
        let _ = self
            .density_samples
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < u32::MAX - 1).then_some(n + 1)
            });
        true
    }
    /// Stop trusting calibration after the provider rejected a request for size.
    pub fn distrust_density(&self) {
        self.density_samples.store(u32::MAX, Ordering::Relaxed);
    }
    pub fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled() || self.state.shutdown.is_cancelled() {
            bail!("CANCELLED");
        }
        if !self.finalizing.load(Ordering::Relaxed)
            && self
                .elapsed_before
                .saturating_add(self.started.elapsed().as_secs())
                >= self.snapshot.task.max_seconds
        {
            bail!("TIME_BUDGET: run deadline reached");
        }
        Ok(())
    }
    pub fn reserve(&self, tokens: u64, cost: f64) -> Result<()> {
        let current = self.reserved_tokens.load(Ordering::Relaxed);
        if current
            .checked_add(tokens)
            .is_none_or(|total| total > self.snapshot.task.max_tokens)
        {
            bail!("TOKEN_BUDGET: run token budget exhausted");
        }
        let micro = (cost * 1_000_000.0).ceil() as u64;
        let old = self.reserved_cost.load(Ordering::Relaxed);
        if self.snapshot.task.max_cost > 0.0
            && old.saturating_add(micro) as f64 / 1_000_000.0 > self.snapshot.task.max_cost
        {
            bail!("COST_BUDGET: run cost budget exhausted");
        }
        self.reserved_tokens.fetch_add(tokens, Ordering::Relaxed);
        let _ = self
            .reserved_cost
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(micro))
            });
        Ok(())
    }
    pub fn reconcile_tokens(&self, reservation: u64, used: u64) -> u64 {
        let released = reservation.saturating_sub(used);
        let _ = self
            .reserved_tokens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(reservation).saturating_add(used))
            });
        released
    }
    pub async fn event(&self, kind: &str, data: Value) -> Result<()> {
        self.check()?;
        let journal = json!({"run_id":self.id,"kind":kind,"data":data,"reserved_tokens":self.reserved_tokens.load(Ordering::Relaxed),"reserved_cost":self.reserved_cost.load(Ordering::Relaxed),"elapsed":self.elapsed_before+self.started.elapsed().as_secs()});
        let path = self
            .state
            .vault
            .dir
            .join("journal")
            .join(format!("{}.json", self.id));
        crate::config::atomic_private(&path, serde_json::to_string(&journal)?.as_bytes())?;
        // A bounded latest-event journal preserves the last state while DB reconnects.
        for attempt in 0..6 {
            self.check()?;
            let result=async {
                let mut tx=self.pool.begin().await?;
                sqlx::query("INSERT INTO events(run_id,kind,data) VALUES(?,?,?)").bind(&self.id).bind(kind).bind(data.to_string()).execute(&mut *tx).await?;
                sqlx::query("UPDATE runs SET progress=JSON_MERGE_PATCH(JSON_OBJECT('title',JSON_EXTRACT(progress,'$.title'),'section',JSON_EXTRACT(progress,'$.section'),'total_sections',JSON_EXTRACT(progress,'$.total_sections'),'iteration',JSON_EXTRACT(progress,'$.iteration'),'max_iterations',JSON_EXTRACT(progress,'$.max_iterations')),?) WHERE id=?").bind(data.to_string()).bind(&self.id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,'budget',?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&self.id).bind(json!({"tokens":self.reserved_tokens.load(Ordering::Relaxed),"cost":self.reserved_cost.load(Ordering::Relaxed),"elapsed":self.elapsed_before+self.started.elapsed().as_secs(),"extra_margin":self.extra_margin.load(Ordering::Relaxed),"density_samples":self.density_samples.load(Ordering::Relaxed),"density_floor":self.density_floor.load(Ordering::Relaxed)}).to_string()).execute(&mut *tx).await?;
                tx.commit().await
            }.await;
            if result.is_ok() {
                let _ = tokio::fs::remove_file(&path).await;
                return Ok(());
            }
            tokio::select! {_=self.cancel.cancelled()=>bail!("CANCELLED"),_=tokio::time::sleep(Duration::from_secs((1<<attempt).min(10)))=>{}}
        }
        bail!("DB_UNAVAILABLE: checkpoint journal retained for resume")
    }
    pub async fn usage(&self, tokens: u64, cost: f64) -> Result<()> {
        sqlx::query("UPDATE runs SET tokens=tokens+?,cost=cost+? WHERE id=?")
            .bind(tokens)
            .bind(cost)
            .bind(&self.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
