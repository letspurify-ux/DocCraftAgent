use crate::{db, model::Evidence, parser, runner::RunContext};
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;

pub fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn matches_root(path: &Path, roots: &[String]) -> bool {
    roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| path.starts_with(r))
}
pub fn target_path(path: &str, roots: &[String]) -> Result<PathBuf> {
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        bail!("Target path must be absolute");
    }
    if p.extension().and_then(|s| s.to_str()) != Some("md") {
        bail!("Target must have .md extension");
    }
    let parent = std::fs::canonicalize(p.parent().context("Target needs a parent directory")?)
        .context("Create the output directory before registering a task")?;
    if !matches_root(&parent, roots) {
        bail!("Target directory is outside allowed output roots");
    }
    let result = parent.join(p.file_name().context("Invalid target name")?);
    if std::fs::symlink_metadata(&result).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("Symlink output targets are not allowed");
    }
    Ok(result)
}
fn patterns(values: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for value in values {
        b.add(Glob::new(value)?);
    }
    Ok(b.build()?)
}
pub fn validate_task(
    task: &crate::model::TaskConfig,
    settings: &crate::model::Settings,
) -> Result<()> {
    if task.name.trim().is_empty() || task.direction.trim().is_empty() || task.sources.is_empty() {
        bail!("Name, sources, target and direction are required");
    }
    if task.direction.len() > 32_000 || task.name.len() > 200 || task.sources.len() > 50 {
        bail!("Task definition is too large");
    }
    if task.max_iterations == 0
        || task.max_iterations > 10
        || task.max_seconds < 30
        || task.max_seconds > 86400
        || task.max_tokens < 1024
        || !task.max_cost.is_finite()
        || task.max_cost < 0.0
    {
        bail!("Invalid run limits");
    }
    if task.max_cost > 0.0 && (settings.llm.input_price == 0.0 || settings.llm.output_price == 0.0)
    {
        bail!("Set both model prices before enabling cost limits");
    }
    patterns(&task.include)?;
    patterns(&task.exclude)?;
    let target = target_path(&task.target, &settings.output_roots)?;
    for source in &task.sources {
        let path = std::fs::canonicalize(source).context("Source does not exist")?;
        if !matches_root(&path, &settings.source_roots) {
            bail!("Source is outside allowed source roots");
        }
        if path == target {
            bail!("Source cannot be the target document");
        }
    }
    Ok(())
}
pub async fn index(ctx: &RunContext) -> Result<()> {
    if db::load_checkpoint(&ctx.pool, &ctx.id, "indexed")
        .await?
        .is_some()
    {
        return Ok(());
    }
    sqlx::query("DELETE FROM chunks WHERE run_id=?")
        .bind(&ctx.id)
        .execute(&ctx.pool)
        .await?;
    sqlx::query("DELETE FROM files WHERE run_id=?")
        .bind(&ctx.id)
        .execute(&ctx.pool)
        .await?;
    let includes = patterns(&ctx.snapshot.task.include)?;
    let excludes = patterns(&ctx.snapshot.task.exclude)?;
    let dir = ctx.state.vault.dir.join("snapshots").join(&ctx.id);
    tokio::fs::create_dir_all(&dir).await?;
    let roots = ctx.snapshot.task.sources.clone();
    let max_files = ctx.snapshot.settings.max_files;
    let token = ctx.cancel.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(32);
    let scan = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let mut count = 0;
        for root in roots {
            for item in ignore::WalkBuilder::new(root)
                .hidden(false)
                .follow_links(false)
                .build()
            {
                if token.is_cancelled() {
                    return Ok(());
                }
                let entry = item?;
                if !entry.file_type().is_some_and(|f| f.is_file()) {
                    continue;
                }
                let p = entry.path().to_path_buf();
                if !seen.insert(p.clone()) {
                    continue;
                }
                count += 1;
                if count > max_files {
                    bail!("Source file count exceeds configured limit");
                }
                if tx.blocking_send(p).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    });
    let target = PathBuf::from(&ctx.snapshot.task.target);
    let local = std::fs::canonicalize(&ctx.state.vault.dir)?;
    let mut fingerprint = Sha256::new();
    fingerprint.update(parser::VERSION);
    let mut indexed = 0usize;
    let mut skipped = 0usize;
    let mut cache_hits = 0usize;
    while let Some(path) = tokio::select! { _ = ctx.cancel.cancelled() => { bail!("CANCELLED"); }, value = rx.recv() => value }
    {
        ctx.check()?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let normalized = path.to_string_lossy().replace('\\', "/");
        let builtin = path.components().any(|p| {
            [
                "node_modules",
                "target",
                "dist",
                "build",
                ".git",
                ".local",
                "vendor",
                "__pycache__",
                ".venv",
            ]
            .iter()
            .any(|n| p.as_os_str() == std::ffi::OsStr::new(n))
        }) || name.starts_with(".env")
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name == "settings.enc"
            || path == target
            || path.starts_with(&local);
        let relative = ctx
            .snapshot
            .task
            .sources
            .iter()
            .filter_map(|r| path.strip_prefix(r).ok())
            .next()
            .unwrap_or(&path);
        let relative = relative.to_string_lossy().replace('\\', "/");
        if builtin
            || excludes.is_match(&normalized)
            || excludes.is_match(&relative)
            || (!ctx.snapshot.task.include.is_empty()
                && !includes.is_match(&relative)
                && !includes.is_match(&normalized))
        {
            record_skip(ctx, &path, "excluded").await?;
            skipped += 1;
            continue;
        }
        let result = snapshot_file(ctx, &path, &dir).await;
        let (bytes, snapshot_path) = match result {
            Ok(v) => v,
            Err(e) => {
                record_skip(ctx, &path, &e.to_string()).await?;
                skipped += 1;
                continue;
            }
        };
        let digest = hash(&bytes);
        fingerprint.update(normalized.as_bytes());
        fingerprint.update(digest.as_bytes());
        let lang = parser::language(&path);
        let key = format!("{}:{lang}:{digest}", parser::VERSION);
        let cached = sqlx::query("SELECT data FROM parse_cache WHERE hash=?")
            .bind(&key)
            .fetch_optional(&ctx.pool)
            .await?;
        let parsed: parser::Parsed = if let Some(row) = cached {
            cache_hits += 1;
            serde_json::from_str(&row.try_get::<String, _>("data")?)?
        } else {
            match worker(ctx, &snapshot_path, lang).await {
                Ok(p) => {
                    let encoded = serde_json::to_string(&p)?;
                    if encoded.len() <= 4 * 1024 * 1024 {
                        sqlx::query("INSERT INTO parse_cache(hash,data) VALUES(?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&key).bind(encoded).execute(&ctx.pool).await?;
                    }
                    p
                }
                Err(e) => {
                    record_skip(ctx, &path, &e.to_string()).await?;
                    skipped += 1;
                    continue;
                }
            }
        };
        let result = sqlx::query("INSERT INTO files(run_id,path,snapshot_path,hash,language,status,detail) VALUES(?,?,?,?,?,?,?)")
            .bind(&ctx.id).bind(&normalized).bind(snapshot_path.to_string_lossy().as_ref()).bind(&digest).bind(lang).bind("indexed")
            .bind(json!({"parse_errors":parsed.has_errors,"limited":lang=="text","relations":parsed.relations}).to_string()).execute(&ctx.pool).await?;
        let file_id = result.last_insert_id();
        let mut tx = ctx.pool.begin().await?;
        for batch in parsed.chunks.chunks(64) {
            ctx.check()?;
            let blobs: Vec<_> = batch
                .iter()
                .map(|c| {
                    (
                        hash(format!("{}:{}", c.symbols.join(" "), c.content).as_bytes()),
                        c,
                    )
                })
                .collect();
            let mut shared = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT IGNORE INTO chunk_blobs(hash,symbols,content) ",
            );
            shared.push_values(&blobs, |mut b, (hash, chunk)| {
                b.push_bind(hash)
                    .push_bind(chunk.symbols.join(" "))
                    .push_bind(&chunk.content);
            });
            shared.build().execute(&mut *tx).await?;
            let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
                "INSERT INTO chunks(run_id,file_id,path,start_line,end_line,symbols,content,blob_hash) ",
            );
            query.push_values(&blobs, |mut b, (hash, chunk)| {
                b.push_bind(&ctx.id)
                    .push_bind(file_id)
                    .push_bind(&normalized)
                    .push_bind(chunk.start)
                    .push_bind(chunk.end)
                    .push_bind("")
                    .push_bind("")
                    .push_bind(hash);
            });
            query.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
        indexed += 1;
        if indexed.is_multiple_of(10) || indexed == 1 {
            ctx.event("index", json!({"stage":"indexing","indexed":indexed,"skipped":skipped,"parse_cache_hits":cache_hits,"current_file":normalized})).await?;
        }
    }
    scan.await??;
    db::checkpoint(
        &ctx.pool,
        &ctx.id,
        "index_fingerprint",
        &json!(hex::encode(fingerprint.finalize())),
    )
    .await?;
    if indexed == 0 {
        bail!("No readable source files remain after filtering");
    }
    db::checkpoint(
        &ctx.pool,
        &ctx.id,
        "indexed",
        &json!({"indexed":indexed,"skipped":skipped,"cache_hits":cache_hits}),
    )
    .await?;
    ctx.event("index",json!({"stage":"indexed","indexed":indexed,"skipped":skipped,"parse_cache_hits":cache_hits})).await?;
    Ok(())
}
async fn snapshot_file(ctx: &RunContext, path: &Path, dir: &Path) -> Result<(Vec<u8>, PathBuf)> {
    for _ in 0..3 {
        ctx.check()?;
        let canonical = tokio::fs::canonicalize(path).await?;
        if !matches_root(&canonical, &ctx.snapshot.settings.source_roots) {
            bail!("File escaped allowed source roots");
        }
        let before = tokio::fs::metadata(&canonical).await?;
        if before.len() > ctx.snapshot.settings.max_file_bytes {
            bail!("File exceeds configured byte limit");
        }
        let mut bytes = Vec::new();
        tokio::fs::File::open(&canonical)
            .await?
            .take(ctx.snapshot.settings.max_file_bytes + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > ctx.snapshot.settings.max_file_bytes {
            bail!("File grew beyond byte limit");
        }
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            bail!("Binary or non-UTF-8 file");
        }
        let after = tokio::fs::metadata(&canonical).await?;
        if before.len() != after.len() || before.modified()? != after.modified()? {
            continue;
        }
        let snapshot = dir.join(hash(&bytes));
        if !snapshot.exists() {
            tokio::fs::write(&snapshot, &bytes).await?;
        }
        return Ok((bytes, snapshot));
    }
    bail!("File changed during snapshot after three attempts")
}
async fn record_skip(ctx: &RunContext, path: &Path, detail: &str) -> Result<()> {
    sqlx::query("INSERT INTO files(run_id,path,snapshot_path,hash,language,status,detail) VALUES(?,?, '', '', '', 'skipped', ?)")
        .bind(&ctx.id).bind(path.to_string_lossy().as_ref()).bind(detail.chars().take(500).collect::<String>()).execute(&ctx.pool).await?;
    Ok(())
}
async fn worker(ctx: &RunContext, path: &Path, lang: &str) -> Result<parser::Parsed> {
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--parse-worker")
        .arg(path)
        .arg(lang)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .context("Missing worker stdout")?
        .take(150 * 1024 * 1024);
    let mut bytes = Vec::new();
    tokio::select! {
        _ = ctx.cancel.cancelled() => { let _ = child.kill().await; bail!("CANCELLED"); },
        result = tokio::time::timeout(Duration::from_secs(30), async { stdout.read_to_end(&mut bytes).await?; child.wait().await }) => {
            let status = result.context("Parser worker timeout")??;
            if !status.success() { bail!("Parser worker failed; file isolated"); }
        }
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub async fn inventory(ctx: &RunContext) -> Result<String> {
    let rows = sqlx::query("SELECT path,language,LEFT(detail,300) detail FROM files WHERE run_id=? AND status='indexed' ORDER BY CASE WHEN path LIKE '%/src/%' THEN 0 WHEN path LIKE '%README%' OR path LIKE '%/context.md' OR path LIKE '%/package.json' OR path LIKE '%/Cargo.toml' THEN 1 WHEN path LIKE '%/test/%' OR path LIKE '%/tests/%' THEN 3 ELSE 2 END,path LIMIT 500")
        .bind(&ctx.id).fetch_all(&ctx.pool).await?;
    let mut out = String::new();
    for row in rows {
        let path: String = row.try_get("path")?;
        let lang: String = row.try_get("language")?;
        let detail: String = row.try_get("detail")?;
        out.push_str(&format!(
            "{path} ({lang}) {}\n",
            detail.chars().take(300).collect::<String>()
        ));
        if out.len() > 40_000 {
            break;
        }
    }
    Ok(out)
}
pub async fn retrieve(ctx: &RunContext, query: &str, max_bytes: usize) -> Result<Vec<Evidence>> {
    let fingerprint = db::load_checkpoint(&ctx.pool, &ctx.id, "index_fingerprint")
        .await?
        .unwrap_or(json!(ctx.id));
    let key = hash(format!("retrieval-v4:{fingerprint}:{query}:{max_bytes}").as_bytes());
    if let Some(row) = sqlx::query("SELECT data FROM retrieval_cache WHERE hash=?")
        .bind(&key)
        .fetch_optional(&ctx.pool)
        .await?
    {
        return Ok(serde_json::from_str(&row.try_get::<String, _>("data")?)?);
    }
    let terms = search_terms(query);
    let mut after = 0u64;
    let mut best: Vec<(i64, Evidence)> = vec![];
    loop {
        ctx.check()?;
        let rows = sqlx::query("SELECT c.id,c.path,c.start_line,c.end_line,COALESCE(b.symbols,c.symbols) symbols,COALESCE(b.content,c.content) content FROM chunks c LEFT JOIN chunk_blobs b ON b.hash=c.blob_hash WHERE c.run_id=? AND c.id>? ORDER BY c.id LIMIT 64").bind(&ctx.id).bind(after).fetch_all(&ctx.pool).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let id: u64 = row.try_get("id")?;
            after = id;
            let path: String = row.try_get("path")?;
            let content: String = row.try_get("content")?;
            let symbols: String = row.try_get("symbols")?;
            let score = evidence_score(query, &terms, &path, &symbols, &content);
            best.push((
                score,
                Evidence {
                    id: hash(
                        format!(
                            "{}:{}:{}",
                            path,
                            row.try_get::<u32, _>("start_line")?,
                            hash(content.as_bytes())
                        )
                        .as_bytes(),
                    ),
                    path,
                    start: row.try_get("start_line")?,
                    end: row.try_get("end_line")?,
                    content,
                },
            ));
        }
        best.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
        let mut per_path = std::collections::HashMap::new();
        best.retain(|(_, e)| {
            let count = per_path.entry(e.path.clone()).or_insert(0);
            *count += 1;
            *count <= 8
        });
        best.truncate(64);
    }
    let distinct = best
        .iter()
        .map(|(_, e)| e.path.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
        .clamp(1, 8);
    let ranked = diverse_evidence(best);
    let per_file = (max_bytes / distinct).max(1024);
    let mut used = 0;
    let mut result = vec![];
    for (tier, _, mut e) in ranked {
        let available = max_bytes.saturating_sub(used + e.path.len() + 256);
        let available = if tier < 2 {
            available.min(per_file)
        } else {
            available
        };
        if available < 256 {
            continue;
        }
        if e.content.len() > available {
            let mut end = available;
            while !e.content.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            let shortened = e.content.get(..end).unwrap_or_default();
            let lines = shortened.bytes().filter(|b| *b == b'\n').count() as u32;
            e.end = e.start + lines - u32::from(shortened.ends_with('\n'));
            e.content = shortened.to_string();
            e.id =
                hash(format!("{}:{}:{}", e.path, e.start, hash(e.content.as_bytes())).as_bytes());
        }
        let size = e.content.len() + e.path.len() + 128;
        if used + size > max_bytes {
            continue;
        }
        used += size;
        result.push(e);
    }
    sqlx::query("INSERT INTO retrieval_cache(hash,data) VALUES(?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&key).bind(serde_json::to_string(&result)?).execute(&ctx.pool).await?;
    Ok(result)
}

fn diverse_evidence(best: Vec<(i64, Evidence)>) -> Vec<(u8, i64, Evidence)> {
    let mut seen_paths = std::collections::HashSet::new();
    let mut ranked = Vec::new();
    for (score, e) in best {
        let first = seen_paths.insert(e.path.clone());
        let tier = match (first, is_implementation(&e.path)) {
            (true, true) => 0,
            (true, false) => 1,
            _ => 2,
        };
        ranked.push((tier, score, e));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    ranked
}
pub fn is_implementation(path: &str) -> bool {
    let lower = path.to_lowercase();
    !lower.contains("/test/")
        && !lower.contains("/tests/")
        && !lower.contains(".test.")
        && !lower.contains(".spec.")
        && Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                matches!(
                    e,
                    "rs" | "js"
                        | "jsx"
                        | "ts"
                        | "tsx"
                        | "py"
                        | "java"
                        | "go"
                        | "c"
                        | "h"
                        | "cpp"
                        | "cs"
                        | "sql"
                        | "mjs"
                        | "cjs"
                )
            })
}
fn search_terms(query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(str::to_lowercase)
        .filter(|s| {
            s.len() > 1
                && ![
                    "src",
                    "js",
                    "jsx",
                    "ts",
                    "tsx",
                    "test",
                    "tests",
                    "backend",
                    "frontend",
                    "users",
                    "workspace",
                ]
                .contains(&s.as_str())
                && seen.insert(s.clone())
        })
        .take(60)
        .collect()
}
fn evidence_score(query: &str, terms: &[String], path: &str, symbols: &str, content: &str) -> i64 {
    let hay = content.to_lowercase();
    let names = format!("{path} {symbols}").to_lowercase();
    let filename = Path::new(path)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let query = query.to_lowercase();
    let exact = if !filename.is_empty() && query.contains(&filename) {
        80
    } else {
        0
    };
    let implementation = if path.contains("/src/") { 6 } else { 0 };
    exact
        + implementation
        + terms
            .iter()
            .map(|t| {
                if names.contains(t) {
                    10
                } else if hay.contains(t) {
                    2
                } else {
                    0
                }
            })
            .sum::<i64>()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relevant_implementation_files_precede_repeated_chunks_and_readme() {
        let e = |path: &str| Evidence {
            id: path.into(),
            path: path.into(),
            start: 1,
            end: 1,
            content: "code".into(),
        };
        let ranked = diverse_evidence(vec![
            (100, e("/README.md")),
            (90, e("/src/large.rs")),
            (89, e("/src/large.rs")),
            (80, e("/src/helper.rs")),
        ]);
        assert_eq!(
            ranked
                .iter()
                .map(|(_, _, e)| e.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/src/large.rs",
                "/src/helper.rs",
                "/README.md",
                "/src/large.rs"
            ]
        );
        assert!(!is_implementation("/src/helper.test.ts"));
    }
    #[test]
    fn source_names_outweigh_generic_test_paths() {
        let query = "backend/src/agent.js decision loop";
        let terms = search_terms(query);
        assert!(
            evidence_score(
                query,
                &terms,
                "/repo/backend/src/agent.js",
                "decide",
                "loop"
            ) > evidence_score(
                query,
                &terms,
                "/repo/backend/test/context.test.js",
                "test",
                "agent loop"
            )
        );
        assert_eq!(search_terms("src src backend agent agent"), vec!["agent"]);
    }
    #[test]
    fn roots_and_target_constraints() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = std::fs::canonicalize(dir.path())?;
        let roots = vec![root.to_string_lossy().into()];
        assert!(target_path(root.join("doc.md").to_str().context("utf8")?, &roots).is_ok());
        assert!(target_path(root.join("doc.rs").to_str().context("utf8")?, &roots).is_err());
        assert!(target_path("relative.md", &roots).is_err());
        assert!(!matches_root(root.parent().context("parent")?, &roots));
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn output_symlinks_are_rejected() -> Result<()> {
        let a = tempfile::tempdir()?;
        let b = tempfile::tempdir()?;
        let file = b.path().join("target.md");
        std::fs::write(&file, "private")?;
        let link = a.path().join("doc.md");
        std::os::unix::fs::symlink(file, &link)?;
        assert!(
            target_path(
                link.to_str().context("utf8")?,
                &[a.path().to_string_lossy().into()]
            )
            .is_err()
        );
        Ok(())
    }
}
