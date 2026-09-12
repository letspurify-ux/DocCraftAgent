use crate::model::{DbConfig, EventView, RunView};
use anyhow::{Result, bail};
use serde_json::Value;
use sqlx::{
    MySqlPool, Row,
    mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode},
};
use std::time::Duration;

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
                sqlx::query("SET SESSION max_statement_time=10, innodb_lock_wait_timeout=5")
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
