use crate::{
    config::atomic_private,
    runner::{AppState, RunContext},
    source::{hash, target_path},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::MySqlPool;
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
#[derive(Serialize, Deserialize)]
struct Publication {
    id: String,
    run_id: String,
    task_id: String,
    path: String,
    hash: String,
    markdown: String,
    warnings: Vec<String>,
    status: String,
}
pub async fn save(ctx: &RunContext, markdown: &str, warnings: &[String]) -> Result<()> {
    ctx.check()?;
    validate_mermaid(ctx, markdown).await?;
    let target = target_path(
        &ctx.snapshot.task.target,
        &ctx.snapshot.settings.output_roots,
    )?;
    let existing = if target.exists() {
        Some(hash(&tokio::fs::read(&target).await?))
    } else {
        None
    };
    let mut warnings = warnings.to_vec();
    let path = if existing != ctx.snapshot.original_hash {
        warnings.push("Target changed externally; result saved as a conflict copy".into());
        target.with_file_name(format!(
            "{}.conflict-{}.md",
            target
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("document"),
            ctx.id
        ))
    } else {
        target.clone()
    };
    let p = Publication {
        id: uuid::Uuid::new_v4().to_string(),
        run_id: ctx.id.clone(),
        task_id: ctx.snapshot.task.id.clone(),
        path: path.to_string_lossy().into(),
        hash: hash(markdown.as_bytes()),
        markdown: markdown.into(),
        warnings: warnings.clone(),
        status: if warnings.is_empty() {
            "completed".into()
        } else {
            "completed_with_warnings".into()
        },
    };
    let journal = ctx
        .state
        .vault
        .dir
        .join("publications")
        .join(format!("{}.json", ctx.id));
    atomic_private(&journal, serde_json::to_string(&p)?.as_bytes())?;
    ctx.check()?;
    {
        let _gate = ctx.gate.lock.lock().unwrap_or_else(|p| p.into_inner());
        // No await between final cancellation check and atomic replacement; cancellation API
        // waits for this short synchronous commit boundary before acknowledging completion.
        if path == target {
            let now = if path.exists() {
                Some(hash(&std::fs::read(&path)?))
            } else {
                None
            };
            if now != existing {
                bail!("Target changed during publish; publication journal retained");
            }
        }
        if ctx.cancel.is_cancelled() {
            bail!("CANCELLED");
        }
        atomic_private(&path, markdown.as_bytes())?;
        ctx.gate
            .published
            .store(true, std::sync::atomic::Ordering::Release);
    }
    commit(&ctx.pool, &p).await?;
    tokio::fs::remove_file(journal).await?;
    Ok(())
}
async fn commit(pool: &MySqlPool, p: &Publication) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT IGNORE INTO artifacts(id,run_id,task_id,path,hash,markdown,warnings) VALUES(?,?,?,?,?,?,?)")
        .bind(&p.id).bind(&p.run_id).bind(&p.task_id).bind(&p.path).bind(&p.hash).bind(&p.markdown).bind(serde_json::to_string(&p.warnings)?).execute(&mut *tx).await?;
    let progress = json!({"stage":"finished","path":p.path,"warnings":p.warnings});
    sqlx::query("UPDATE runs SET status=?,progress=?,error=NULL WHERE id=?")
        .bind(&p.status)
        .bind(progress.to_string())
        .bind(&p.run_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO events(run_id,kind,data) VALUES(?,'terminal',?)")
        .bind(&p.run_id)
        .bind(json!({"status":p.status,"path":p.path,"warnings":p.warnings}).to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
pub async fn recover(state: &Arc<AppState>, pool: &MySqlPool) -> Result<()> {
    let dir = state.vault.dir.join("publications");
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let p: Publication = serde_json::from_slice(&std::fs::read(&path)?)?;
        let target = PathBuf::from(&p.path);
        if target.exists() && hash(&std::fs::read(target)?) == p.hash {
            commit(pool, &p).await?;
            std::fs::remove_file(path)?;
        } else {
            sqlx::query("UPDATE runs SET status='failed',error='Publication interrupted before commit; resume to regenerate' WHERE id=? AND status NOT IN ('completed','completed_with_warnings')").bind(&p.run_id).execute(pool).await?;
        }
    }
    Ok(())
}
pub async fn validate_mermaid(ctx: &RunContext, markdown: &str) -> Result<()> {
    if !markdown.contains("```mermaid") {
        return Ok(());
    }
    let script = std::env::var("DOCCRAFT_MERMAID_SCRIPT")
        .unwrap_or_else(|_| "frontend/scripts/mermaid-check.mjs".into());
    let mut child = tokio::process::Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Node.js is required to validate Mermaid diagrams")?;
    let mut stdin = child.stdin.take().context("Missing Mermaid worker stdin")?;
    use tokio::io::AsyncWriteExt;
    tokio::select! {
        _=ctx.cancel.cancelled()=>{let _=child.kill().await;bail!("CANCELLED");},
        result=tokio::time::timeout(Duration::from_secs(15),async{stdin.write_all(markdown.as_bytes()).await?;drop(stdin);child.wait().await})=>{
            if !result.context("Mermaid validation timed out")??.success(){bail!("MERMAID_INVALID: diagram failed syntax validation");}
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_replacement_preserves_complete_content() -> Result<()> {
        let d = tempfile::tempdir()?;
        let p = d.path().join("x.md");
        atomic_private(&p, b"old")?;
        atomic_private(&p, b"new content")?;
        assert_eq!(std::fs::read(&p)?, b"new content");
        Ok(())
    }
}
