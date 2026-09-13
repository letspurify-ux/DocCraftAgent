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
        let _gate = ctx.gate.lock.lock().await;
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
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        recover_publication(pool, &path).await?;
    }
    Ok(())
}
pub async fn recover_run(state: &AppState, pool: &MySqlPool, run_id: &str) -> Result<()> {
    // A worker only owns its own publication. Scanning other journals here can
    // mark a concurrent publisher failed before it replaces its target, or race
    // with that publisher's journal removal after a successful commit.
    recover_publication(
        pool,
        &state
            .vault
            .dir
            .join("publications")
            .join(format!("{run_id}.json")),
    )
    .await
}
async fn recover_publication(pool: &MySqlPool, path: &std::path::Path) -> Result<()> {
    let p: Publication = serde_json::from_slice(&std::fs::read(path)?)?;
    let target = PathBuf::from(&p.path);
    if target.exists() && hash(&std::fs::read(target)?) == p.hash {
        commit(pool, &p).await?;
        std::fs::remove_file(path)?;
    } else {
        sqlx::query("UPDATE runs SET status='failed',error='Publication interrupted before commit; resume to regenerate' WHERE id=? AND status NOT IN ('completed','completed_with_warnings')").bind(&p.run_id).execute(pool).await?;
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
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Node.js is required to validate Mermaid diagrams")?;
    let stdin = child.stdin.take().context("Missing Mermaid worker stdin")?;
    let stderr = child
        .stderr
        .take()
        .context("Missing Mermaid worker stderr")?;
    tokio::select! {
        _=ctx.cancel.cancelled()=>{let _=child.kill().await;bail!("CANCELLED");},
        result=tokio::time::timeout(Duration::from_secs(15),async{
            let diagnostic = exchange_mermaid(stdin, stderr, markdown).await?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, diagnostic))
        })=>{
            let (status, diagnostic) = result.context("Mermaid validation timed out")??;
            if !status.success(){
                let detail = String::from_utf8_lossy(&diagnostic);
                if detail.trim().is_empty() { bail!("MERMAID_INVALID: diagram failed syntax validation"); }
                bail!("MERMAID_INVALID: {}", detail.trim());
            }
        }
    }
    Ok(())
}
async fn exchange_mermaid(
    mut stdin: impl tokio::io::AsyncWrite + Unpin,
    stderr: impl tokio::io::AsyncRead + Unpin,
    markdown: &str,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const DIAGNOSTIC_LIMIT: u64 = 2048;
    // A worker can fill stderr before reading stdin (for example on startup
    // failure). Drive both pipes so backpressure cannot block the diagnostic.
    let (_, diagnostic) = tokio::try_join!(
        async {
            stdin.write_all(markdown.as_bytes()).await?;
            drop(stdin);
            Ok::<_, std::io::Error>(())
        },
        async {
            let mut diagnostic = Vec::new();
            stderr
                .take(DIAGNOSTIC_LIMIT + 1)
                .read_to_end(&mut diagnostic)
                .await?;
            if diagnostic.len() as u64 > DIAGNOSTIC_LIMIT {
                return Err(std::io::Error::other(
                    "Mermaid validator diagnostic exceeded 2 KiB",
                ));
            }
            Ok(diagnostic)
        }
    )?;
    Ok(diagnostic)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn worker_recovery_leaves_other_publications_untouched() -> Result<()> {
        let pool = crate::test_support::pool(1).await?;
        let run = crate::test_support::TestRun::new(pool.clone())?;
        let dir = run.ctx.state.vault.dir.join("publications");
        for id in [&run.ctx.id, &"other-run".to_string()] {
            sqlx::query("INSERT INTO runs(id,task_id,status,snapshot,progress) VALUES(?,?,'running','','{}')")
                .bind(id).bind(&run.ctx.snapshot.task.id).execute(&pool).await?;
            let p = Publication {
                id: uuid::Uuid::new_v4().to_string(),
                run_id: id.clone(),
                task_id: run.ctx.snapshot.task.id.clone(),
                path: if id == &run.ctx.id {
                    run.ctx.snapshot.task.target.clone()
                } else {
                    format!("{}.other.md", run.ctx.snapshot.task.target)
                },
                hash: hash(b"published"),
                markdown: "published".into(),
                warnings: vec![],
                status: "completed".into(),
            };
            atomic_private(&dir.join(format!("{id}.json")), &serde_json::to_vec(&p)?)?;
        }
        std::fs::write(&run.ctx.snapshot.task.target, "published")?;
        let result: Result<()> = async {
            recover_run(&run.ctx.state, &pool, &run.ctx.id).await?;
            let status: String = sqlx::query_scalar("SELECT status FROM runs WHERE id=?")
                .bind(&run.ctx.id)
                .fetch_one(&pool)
                .await?;
            assert_eq!(status, "completed");
            let other: String = sqlx::query_scalar("SELECT status FROM runs WHERE id='other-run'")
                .fetch_one(&pool)
                .await?;
            assert_eq!(other, "running");
            assert!(dir.join("other-run.json").exists());
            assert!(!dir.join(format!("{}.json", run.ctx.id)).exists());
            Ok(())
        }
        .await;
        crate::test_support::close(pool).await?;
        result
    }

    #[tokio::test]
    async fn mermaid_exchange_closes_input_and_preserves_diagnostics() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (stdin, mut worker_stdin) = tokio::io::duplex(64);
        let (stderr, mut worker_stderr) = tokio::io::duplex(64);
        let markdown = "한글 Mermaid 입력".repeat(500);
        let (result, input) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(exchange_mermaid(stdin, stderr, &markdown), async {
                let mut input = String::new();
                worker_stderr.write_all(b"syntax diagnostic").await?;
                worker_stdin.read_to_string(&mut input).await?;
                drop(worker_stderr);
                Ok::<_, std::io::Error>(input)
            })
        })
        .await?;
        assert_eq!(result?, b"syntax diagnostic");
        assert_eq!(input?, markdown);
        Ok(())
    }
    #[tokio::test]
    async fn mermaid_drains_stderr_while_sending_input() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let (stdin, _worker_stdin) = tokio::io::duplex(64);
        let (stderr, mut worker_stderr) = tokio::io::duplex(64);
        let markdown = "x".repeat(4096);
        let (result, _) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(exchange_mermaid(stdin, stderr, &markdown), async {
                worker_stderr.write_all(&vec![b'e'; 2049]).await
            })
        })
        .await
        .context("stdin and stderr blocked each other")?;
        assert!(
            result
                .err()
                .is_some_and(|e| e.to_string().contains("exceeded 2 KiB"))
        );
        Ok(())
    }
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
