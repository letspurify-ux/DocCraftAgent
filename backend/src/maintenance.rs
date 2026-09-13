use crate::{db, model::RunSnapshot, runner::AppState};
use anyhow::Result;
use sqlx::Row;
use std::{sync::Arc, time::Duration};

pub fn start(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut cycle = 0u32;
        loop {
            tokio::select! {_=state.shutdown.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(10))=>{}}
            if let Ok(pool) = state.db().await {
                let _ = reconnect_runs(&state, &pool).await;
                if cycle.is_multiple_of(360) && state.controls.lock().await.is_empty() {
                    let _ = cleanup(&state, &pool).await;
                }
            } else {
                let _ = reconnect_database(&state).await;
            }
            cycle = cycle.wrapping_add(1);
        }
    });
}
async fn reconnect_database(state: &Arc<AppState>) -> Result<()> {
    // Settings changes, manual starts and recovery all mutate the pool/run
    // lifecycle. Serializing the complete reconnect prevents an old snapshot
    // from being enqueued while a new database configuration is being saved.
    let _guard = match state.lifecycle.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return Ok(()),
    };
    if state.pool.read().await.is_some() {
        return Ok(());
    }
    let config = state.settings.read().await.db.clone();
    let pool = db::connect(&config, true).await?;
    *state.pool.write().await = Some(pool);
    crate::runner::recover(state.clone()).await
}
async fn reconnect_runs(state: &Arc<AppState>, pool: &sqlx::MySqlPool) -> Result<()> {
    let _guard = match state.lifecycle.try_acquire() {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    sqlx::query("SELECT 1").execute(pool).await?;
    crate::runner::replay_terminal(state, pool).await?;
    if state.controls.lock().await.is_empty() {
        crate::runner::replay_journal(state, pool).await?;
        crate::publish::recover(state, pool).await?;
    }
    let rows=sqlx::query("SELECT id,snapshot FROM runs WHERE status='failed' AND error LIKE 'DB_UNAVAILABLE:%' AND cancel_requested=FALSE LIMIT 10").fetch_all(pool).await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        let n = db::load_checkpoint(pool, &id, "db_recoveries")
            .await?
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if n >= 3 {
            continue;
        }
        let snapshot: RunSnapshot = serde_json::from_str(
            &state
                .vault
                .decrypt(&row.try_get::<String, _>("snapshot")?)?,
        )?;
        db::checkpoint(pool, &id, "db_recoveries", &serde_json::json!(n + 1)).await?;
        let _ = crate::runner::enqueue(state.clone(), snapshot, Some(id)).await;
    }
    Ok(())
}
pub async fn cleanup(state: &Arc<AppState>, pool: &sqlx::MySqlPool) -> Result<()> {
    let _guard = match state.lifecycle.try_acquire() {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    if !state.controls.lock().await.is_empty() {
        return Ok(());
    }
    let settings = state.settings.read().await.clone();
    let rows=sqlx::query("SELECT id FROM runs WHERE status IN ('completed','completed_with_warnings','cancelled','failed') AND updated_at < DATE_SUB(NOW(), INTERVAL ? DAY) LIMIT 100")
        .bind(settings.retention_days).fetch_all(pool).await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        let mut tx = pool.begin().await?;
        for table in ["events", "checkpoints", "chunks", "files", "artifacts"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE run_id=?"))
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM runs WHERE id=?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let _ = tokio::fs::remove_dir_all(state.vault.dir.join("snapshots").join(id)).await;
    }
    sqlx::query(
        "DELETE b FROM chunk_blobs b LEFT JOIN chunks c ON c.blob_hash=b.hash WHERE c.id IS NULL",
    )
    .execute(pool)
    .await?;
    let limit = settings.cache_max_mb.saturating_mul(1024 * 1024) / 3;
    for (table, date) in [
        ("parse_cache", "updated_at"),
        ("llm_cache", "created_at"),
        ("retrieval_cache", "created_at"),
    ] {
        for _ in 0..20 {
            let bytes: String = sqlx::query_scalar(&format!(
                "SELECT CAST(COALESCE(SUM(OCTET_LENGTH(data)),0) AS CHAR) FROM {table}"
            ))
            .fetch_one(pool)
            .await?;
            if bytes.parse::<u64>().unwrap_or(0) <= limit {
                break;
            }
            sqlx::query(&format!("DELETE FROM {table} ORDER BY {date} LIMIT 100"))
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Vault, model::Settings};

    #[tokio::test]
    async fn database_reconnect_respects_the_lifecycle_gate() -> Result<()> {
        let data = tempfile::tempdir()?;
        let mut settings = Settings::default();
        settings.db.port = 1;
        let state = Arc::new(AppState::new(
            Vault::open(data.path().to_path_buf())?,
            settings,
            None,
        ));
        let _held = state.lifecycle.acquire().await?;

        tokio::time::timeout(Duration::from_millis(100), reconnect_database(&state)).await??;
        assert!(state.pool.read().await.is_none());
        Ok(())
    }
}
