//! MariaDB pool, migrations, and the checkpoint/event store the pipeline
//! resumes from.
//!
//! A checkpoint is one JSON value under a (run, step) key. Steps are named by
//! the stage that writes them, and a stage that finds its own step already
//! written returns it instead of repeating the work.
use crate::model::{DbConfig, EventView, RunView};
use anyhow::{Result, bail};
use serde_json::Value;
use sqlx::{
    MySqlPool, Row,
    mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode},
};
use std::time::Duration;

fn mysql_error_number(error: &sqlx::Error) -> Option<u16> {
    error
        .as_database_error()?
        .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
        .map(sqlx::mysql::MySqlDatabaseError::number)
}

pub fn transaction_conflict(error: &sqlx::Error) -> bool {
    matches!(mysql_error_number(error), Some(1205 | 1213))
}

pub fn temporarily_unavailable(error: &sqlx::Error) -> bool {
    // DatabaseError::code() is SQLSTATE (e.g. 40001), not the MySQL error number.
    matches!(
        error,
        sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed
            | sqlx::Error::Protocol(_)
    ) || matches!(
        mysql_error_number(error),
        Some(
            1040 | 1042
                | 1152
                | 1153
                | 1158
                | 1159
                | 1160
                | 1161
                | 1205
                | 1213
                | 2002
                | 2003
                | 2006
                | 2013
        )
    )
}

pub async fn connect(c: &DbConfig, migrate: bool) -> Result<MySqlPool> {
    if !["doccraft_agent", "doccraft_agent_test"].contains(&c.database.as_str()) {
        bail!("Only dedicated application databases are allowed");
    }
    let options = MySqlConnectOptions::new()
        .host(&c.host)
        .port(c.port)
        .username(&c.user)
        .password(&c.password)
        .ssl_mode(if c.tls {
            MySqlSslMode::Required
        } else {
            MySqlSslMode::Disabled
        })
        .charset("utf8mb4");
    if migrate {
        let admin = MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        sqlx::query(&format!(
            "CREATE DATABASE IF NOT EXISTS `{}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
            c.database
        ))
        .execute(&admin)
        .await?;
        admin.close().await;
    }
    let pool = MySqlPoolOptions::new()
        .max_connections(c.max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query(
                    "SET SESSION time_zone='+00:00', max_statement_time=10, innodb_lock_wait_timeout=5",
                )
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options.database(&c.database))
        .await?;
    if migrate {
        sqlx::migrate!().run(&pool).await?;
    }
    Ok(pool)
}
pub async fn checkpoint(pool: &MySqlPool, run: &str, step: &str, data: &Value) -> Result<()> {
    sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)")
        .bind(run).bind(step).bind(data.to_string()).execute(pool).await?;
    Ok(())
}
pub async fn load_checkpoint(pool: &MySqlPool, run: &str, step: &str) -> Result<Option<Value>> {
    let row = sqlx::query("SELECT data FROM checkpoints WHERE run_id=? AND step=?")
        .bind(run)
        .bind(step)
        .fetch_optional(pool)
        .await?;
    row.map(|r| Ok(serde_json::from_str(&r.try_get::<String, _>("data")?)?))
        .transpose()
}
pub async fn runs(pool: &MySqlPool) -> Result<Vec<RunView>> {
    let rows = sqlx::query("SELECT id,task_id,status,progress,error,tokens,cost,CAST(created_at AS CHAR) created_at,CAST(updated_at AS CHAR) updated_at FROM runs ORDER BY created_at DESC LIMIT 200").fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok(RunView {
                id: r.try_get("id")?,
                task_id: r.try_get("task_id")?,
                status: r.try_get("status")?,
                progress: serde_json::from_str(&r.try_get::<String, _>("progress")?)?,
                error: r.try_get("error")?,
                tokens: r.try_get("tokens")?,
                cost: r.try_get("cost")?,
                created_at: r.try_get("created_at")?,
                updated_at: r.try_get("updated_at")?,
            })
        })
        .collect()
}
pub async fn events(pool: &MySqlPool, run: &str, after: u64) -> Result<Vec<EventView>> {
    let rows = sqlx::query(
        "SELECT id,run_id,kind,data FROM events WHERE run_id=? AND id>? ORDER BY id LIMIT 200",
    )
    .bind(run)
    .bind(after)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(EventView {
                id: r.try_get("id")?,
                run_id: r.try_get("run_id")?,
                kind: r.try_get("kind")?,
                data: serde_json::from_str(&r.try_get::<String, _>("data")?)?,
            })
        })
        .collect()
}

/// Commit repaired content and its completion marker together so recovery cannot
/// skip an unfinished repair or redo an already committed one.
pub async fn checkpoint_repair(
    pool: &MySqlPool,
    run: &str,
    section: &str,
    repair: &str,
    data: &Value,
) -> Result<()> {
    sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?),(?,?,'true') ON DUPLICATE KEY UPDATE data=VALUES(data)")
        .bind(run).bind(section).bind(serde_json::to_string(data)?)
        .bind(run).bind(repair).execute(pool).await?;
    Ok(())
}

/// One indexed chunk of a run, described without its text.
pub struct ChunkRow {
    pub id: u64,
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub bytes: u64,
}

/// One page of a run's chunks, in id order, starting after `after` (0 to begin).
///
/// Paged because a large source has more chunks than one result set should
/// carry, and sized without their text because the caller is planning how to
/// read them, not reading them yet.
pub async fn chunk_page(pool: &MySqlPool, run: &str, after: u64) -> Result<Vec<ChunkRow>> {
    let rows = sqlx::query(
        "SELECT c.id,c.path,c.start_line,c.end_line,COALESCE(LENGTH(COALESCE(b.content,c.content)),0) bytes \
         FROM chunks c LEFT JOIN chunk_blobs b ON b.hash=c.blob_hash \
         WHERE c.run_id=? AND c.id>? ORDER BY c.id LIMIT 256",
    )
    .bind(run)
    .bind(after)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(ChunkRow {
                id: row.try_get("id")?,
                path: row.try_get("path")?,
                start: row.try_get("start_line")?,
                end: row.try_get("end_line")?,
                bytes: row.try_get::<i64, _>("bytes")?.max(0) as u64,
            })
        })
        .collect()
}
