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
fn ordered_by_hash<T>(blobs: &[(String, T)]) -> Vec<&(String, T)> {
    let mut ordered: Vec<_> = blobs.iter().collect();
    ordered.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    ordered
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
fn matches_filters(
    path: &Path,
    roots: &[String],
    includes: &GlobSet,
    excludes: &GlobSet,
    include_all: bool,
) -> bool {
    let mut names = vec![path.to_string_lossy().replace('\\', "/")];
    for root in roots {
        if let Ok(relative) = path.strip_prefix(root) {
            // A source may be a single file, in which case stripping its root is empty.
            let relative = if relative.as_os_str().is_empty() {
                Path::new(path.file_name().unwrap_or_default())
            } else {
                relative
            };
            names.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    !names.iter().any(|name| excludes.is_match(name))
        && (include_all || names.iter().any(|name| includes.is_match(name)))
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
    if task.max_diagrams.is_some_and(|n| n > 32)
        || task.max_iterations == 0
        || task.max_iterations > 20
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
    let token = ctx.cancel.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(32);
    let scan = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut seen = std::collections::HashSet::new();
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
    let mut excluded = 0usize;
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
        if builtin
            || !matches_filters(
                &path,
                &ctx.snapshot.task.sources,
                &includes,
                &excludes,
                ctx.snapshot.task.include.is_empty(),
            )
        {
            record_file_status(ctx, &path, "excluded", "excluded").await?;
            excluded += 1;
            continue;
        }
        let result = snapshot_file(ctx, &path, &dir).await;
        let (bytes, snapshot_path) = match result {
            Ok(v) => v,
            Err(e) => {
                record_file_status(ctx, &path, "skipped", &e.to_string()).await?;
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
                    record_file_status(ctx, &path, "skipped", &e.to_string()).await?;
                    skipped += 1;
                    continue;
                }
            }
        };
        let result = sqlx::query("INSERT INTO files(run_id,path,snapshot_path,hash,language,status,detail) VALUES(?,?,?,?,?,?,?)")
            .bind(&ctx.id).bind(&normalized).bind(snapshot_path.to_string_lossy().as_ref()).bind(&digest).bind(lang).bind("indexed")
            .bind(json!({"parse_errors":parsed.has_errors,"limited":lang=="text","relations":parsed.relations}).to_string()).execute(&ctx.pool).await?;
        let file_id = result.last_insert_id();
        persist_chunks(ctx, file_id, &normalized, &parsed.chunks).await?;
        indexed += 1;
        if indexed.is_multiple_of(10) || indexed == 1 {
            ctx.event("index", json!({"stage":"indexing","indexed":indexed,"excluded":excluded,"skipped":skipped,"parse_cache_hits":cache_hits,"current_file":normalized})).await?;
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
        &json!({"indexed":indexed,"excluded":excluded,"skipped":skipped,"cache_hits":cache_hits}),
    )
    .await?;
    ctx.event("index",json!({"stage":"indexed","indexed":indexed,"excluded":excluded,"skipped":skipped,"parse_cache_hits":cache_hits})).await?;
    Ok(())
}

/// A conflicting statement may leave earlier batches in the transaction alive
/// (1205), or roll back the entire transaction (1213). Retry the whole file only
/// after rollback, so no run-specific chunks are duplicated or omitted.
async fn persist_chunks(
    ctx: &RunContext,
    file_id: u64,
    path: &str,
    chunks: &[parser::Chunk],
) -> Result<()> {
    let blobs: Vec<_> = chunks
        .iter()
        .map(|chunk| {
            (
                hash(format!("{}:{}", chunk.symbols.join(" "), chunk.content).as_bytes()),
                chunk,
            )
        })
        .collect();
    let mut retries = 0;
    loop {
        ctx.check()?;
        let mut tx = ctx.pool.begin().await?;
        let written: Result<()> = async {
            // Concurrent runs can share the same blobs. Lock every shared hash in a
            // stable global order before inserting run-specific chunks, including
            // files whose evidence spans more than one SQL batch.
            let shared_blobs = ordered_by_hash(&blobs);
            for batch in shared_blobs.chunks(64) {
                ctx.check()?;
                let mut shared = sqlx::QueryBuilder::<sqlx::MySql>::new(
                    "INSERT IGNORE INTO chunk_blobs(hash,symbols,content) ",
                );
                shared.push_values(batch, |mut b, blob| {
                    b.push_bind(blob.0.as_str())
                        .push_bind(blob.1.symbols.join(" "))
                        .push_bind(&blob.1.content);
                });
                shared.build().execute(&mut *tx).await?;
            }
            for batch in blobs.chunks(64) {
                ctx.check()?;
                let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
                    "INSERT INTO chunks(run_id,file_id,path,start_line,end_line,symbols,content,blob_hash) ",
                );
                query.push_values(batch, |mut b, (hash, chunk)| {
                    b.push_bind(&ctx.id)
                        .push_bind(file_id)
                        .push_bind(path)
                        .push_bind(chunk.start)
                        .push_bind(chunk.end)
                        .push_bind("")
                        .push_bind("")
                        .push_bind(hash);
                });
                query.build().execute(&mut *tx).await?;
            }

            Ok(())
        }
        .await;
        let result = match written {
            Ok(()) => tx.commit().await.map_err(Into::into),
            Err(error) => {
                tx.rollback().await?;
                Err(error)
            }
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error)
                if retries < 3
                    && error
                        .downcast_ref::<sqlx::Error>()
                        .is_some_and(db::transaction_conflict) =>
            {
                let delay = Duration::from_millis(
                    (50u64 << retries) + u64::from(rand::random::<u8>() % 50),
                );
                retries += 1;
                tokio::select! {
                    _ = ctx.cancel.cancelled() => bail!("CANCELLED"),
                    _ = ctx.state.shutdown.cancelled() => bail!("CANCELLED"),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
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
        // A previous attempt may have stopped halfway through writing this hash path.
        // Always replace it atomically so workers only receive the complete source.
        crate::config::atomic_private(&snapshot, &bytes)?;
        return Ok((bytes, snapshot));
    }
    bail!("File changed during snapshot after three attempts")
}
async fn record_file_status(
    ctx: &RunContext,
    path: &Path,
    status: &str,
    detail: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO files(run_id,path,snapshot_path,hash,language,status,detail) VALUES(?,?, '', '', '', ?, ?)")
        .bind(&ctx.id)
        .bind(path.to_string_lossy().as_ref())
        .bind(status)
        .bind(detail.chars().take(500).collect::<String>())
        .execute(&ctx.pool)
        .await?;
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
    let stdout = child.stdout.take().context("Missing worker stdout")?;
    let mut bytes = Vec::new();
    const WORKER_OUTPUT_LIMIT: u64 = 150 * 1024 * 1024;
    tokio::select! {
        _ = ctx.cancel.cancelled() => { let _ = child.kill().await; bail!("CANCELLED"); },
        result = tokio::time::timeout(Duration::from_secs(30), async {
            let mut limited = stdout.take(WORKER_OUTPUT_LIMIT + 1);
            limited.read_to_end(&mut bytes).await?;
            if bytes.len() as u64 > WORKER_OUTPUT_LIMIT {
                return Err(std::io::Error::other("Parser worker output exceeded 150 MiB"));
            }
            child.wait().await
        }) => {
            let status = result.context("Parser worker timeout")??;
            if !status.success() { bail!("Parser worker failed; file isolated"); }
        }
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub async fn retrieve(ctx: &RunContext, query: &str, max_bytes: usize) -> Result<Vec<Evidence>> {
    let fingerprint = db::load_checkpoint(&ctx.pool, &ctx.id, "index_fingerprint")
        .await?
        .unwrap_or(json!(ctx.id));
    let key = hash(format!("retrieval-v7:{fingerprint}:{query}:{max_bytes}").as_bytes());
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
    let mut ranked = diverse_evidence(best);
    prioritize_named(&mut ranked, query);
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
            let (offset, shortened) = focused_excerpt(&e.content, available, &terms);
            e.start += e.content[..offset].bytes().filter(|b| *b == b'\n').count() as u32;
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

fn prioritize_named(ranked: &mut [(u8, i64, Evidence)], query: &str) {
    let query = query.to_lowercase();
    let mut counts = std::collections::HashMap::new();
    let mut priorities = std::collections::HashMap::new();
    for (tier, _, e) in ranked.iter() {
        let named = Path::new(&e.path)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| query.contains(&name.to_lowercase()));
        let priority = if named && is_implementation(&e.path) {
            let count = counts.entry(e.path.clone()).or_insert(0usize);
            let priority = *count;
            *count += 1;
            priority
        } else {
            100 + usize::from(*tier)
        };
        priorities.insert(e.id.clone(), priority);
    }
    ranked.sort_by_key(|(_, _, e)| priorities.get(&e.id).copied().unwrap_or(usize::MAX));
}

fn focused_excerpt<'a>(content: &'a str, limit: usize, terms: &[String]) -> (usize, &'a str) {
    if content.len() <= limit {
        return (0, content);
    }
    let mut anchors = vec![0];
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if terms.iter().any(|term| line.to_lowercase().contains(term)) {
            anchors.push(offset);
        }
        offset += line.len();
    }
    let stride = anchors.len().div_ceil(128).max(1);
    let mut best = (0usize, 0usize, 0usize);
    for anchor in anchors.iter().step_by(stride) {
        let mut start = anchor.saturating_sub(limit / 3);
        while !content.is_char_boundary(start) {
            start = start.saturating_sub(1);
        }
        if start > 0 {
            start = content[..start].rfind('\n').map_or(0, |n| n + 1);
        }
        let mut end = (start + limit).min(content.len());
        while !content.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        if let Some(n) = content[start..end].rfind('\n') {
            end = start + n + 1;
        }
        let text = content[start..end].to_lowercase();
        let code = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with('*'))
            .collect::<Vec<_>>()
            .join("\n");
        let score = terms
            .iter()
            .map(|t| usize::from(text.contains(t)) + 3 * usize::from(code.contains(t)))
            .sum::<usize>();
        if end > start && (score > best.0 || best.2 == 0) {
            best = (score, start, end);
        }
    }
    (best.1, &content[best.1..best.2])
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
                        | "sh"
                        | "bash"
                        | "zsh"
                        | "bat"
                        | "cmd"
                        | "ps1"
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
    let hay = content
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !line.starts_with("//") && !line.starts_with('*') && !line.starts_with("/*")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
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
                i64::from(names.contains(t)) * 10
                    + hay.matches(t.as_str()).count().min(4) as i64 * 6
            })
            .sum::<i64>()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn conflicting_chunk_batches_rollback_retry_and_stop_at_limit() -> Result<()> {
        let pool = crate::test_support::pool(1).await?;
        let run = crate::test_support::TestRun::new(pool.clone())?;
        let chunks: Vec<_> = (1..=130)
            .map(|line| parser::Chunk {
                start: line,
                end: line,
                symbols: vec![],
                content: format!("{} line {line}", run.ctx.id),
            })
            .collect();
        let trigger = format!("retry_{}", uuid::Uuid::new_v4().simple());
        // Fail in the second batch, after 64 run-specific rows have been written.
        // Session variables survive rollback; table writes must not survive it.
        sqlx::query(&format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON chunks FOR EACH ROW BEGIN \
             IF NEW.run_id='{}' AND NEW.start_line=65 AND @remaining_conflicts>0 THEN \
             SET @remaining_conflicts=@remaining_conflicts-1; \
             SIGNAL SQLSTATE '40001' SET MYSQL_ERRNO=@conflict_number; END IF; END",
            run.ctx.id
        ))
        .execute(&pool)
        .await?;
        let result: Result<()> = async {
            for number in [1205, 1213] {
                sqlx::query("SET @remaining_conflicts=1, @conflict_number=?")
                    .bind(number)
                    .execute(&pool)
                    .await?;
                persist_chunks(&run.ctx, 1, "source.rs", &chunks).await?;
                let rows: (i64, i64) = sqlx::query_as(
                    "SELECT COUNT(*), COUNT(DISTINCT start_line) FROM chunks WHERE run_id=?",
                )
                .bind(&run.ctx.id)
                .fetch_one(&pool)
                .await?;
                assert_eq!(rows, (130, 130));
                let remaining: i64 = sqlx::query_scalar("SELECT @remaining_conflicts")
                    .fetch_one(&pool)
                    .await?;
                assert_eq!(remaining, 0);
                sqlx::query("DELETE FROM chunks WHERE run_id=?")
                    .bind(&run.ctx.id)
                    .execute(&pool)
                    .await?;
            }
            sqlx::query("SET @remaining_conflicts=10, @conflict_number=1213")
                .execute(&pool)
                .await?;
            let error = persist_chunks(&run.ctx, 1, "source.rs", &chunks)
                .await
                .err()
                .context("Retries should be bounded")?;
            assert!(
                error
                    .downcast_ref::<sqlx::Error>()
                    .is_some_and(db::transaction_conflict)
            );
            let remaining: i64 = sqlx::query_scalar("SELECT @remaining_conflicts")
                .fetch_one(&pool)
                .await?;
            assert_eq!(remaining, 6);
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE run_id=?")
                .bind(&run.ctx.id)
                .fetch_one(&pool)
                .await?;
            assert_eq!(count, 0);
            // Cancel after the first conflict, while the retry delay is pending.
            sqlx::query("SET @remaining_conflicts=10")
                .execute(&pool)
                .await?;
            let (write, cancel) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(persist_chunks(&run.ctx, 1, "source.rs", &chunks), async {
                    loop {
                        let remaining: i64 = sqlx::query_scalar("SELECT @remaining_conflicts")
                            .fetch_one(&pool)
                            .await?;
                        if remaining < 10 {
                            run.ctx.cancel.cancel();
                            break Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                })
            })
            .await?;
            cancel?;
            let error = write.err().context("Cancelled indexing must fail")?;
            assert_eq!(error.to_string(), "CANCELLED");
            let remaining: i64 = sqlx::query_scalar("SELECT @remaining_conflicts")
                .fetch_one(&pool)
                .await?;
            assert_eq!(remaining, 9);
            Ok(())
        }
        .await;
        sqlx::query(&format!("DROP TRIGGER {trigger}"))
            .execute(&pool)
            .await?;
        crate::test_support::close(pool).await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires DOCCRAFT_TEST_DB_PORT pointing to a disposable MariaDB"]
    async fn shared_blob_deadlock_is_retried() -> Result<()> {
        let pool = crate::test_support::pool(4).await?;
        let run = crate::test_support::TestRun::new(pool.clone())?;
        let chunks: Vec<_> = (1..=2)
            .map(|line| parser::Chunk {
                start: line,
                end: line,
                symbols: vec![],
                content: format!("{} line {line}", run.ctx.id),
            })
            .collect();
        let mut hashes: Vec<_> = chunks
            .iter()
            .map(|chunk| hash(format!(":{}", chunk.content).as_bytes()))
            .collect();
        hashes.sort();
        let (_, before): (String, String) =
            sqlx::query_as("SHOW GLOBAL STATUS LIKE 'Innodb_deadlocks'")
                .fetch_one(&pool)
                .await?;
        let mut blocker = pool.begin().await?;
        // Give the blocker more undo work so InnoDB selects the indexing
        // transaction as its victim. It holds the second hash; indexing will
        // hold the first hash and wait for the second, even within one SQL batch.
        for step in 0..10 {
            sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,'{}')")
                .bind(&run.ctx.id)
                .bind(format!("blocker:{step}"))
                .execute(&mut *blocker)
                .await?;
        }
        sqlx::query("INSERT INTO chunk_blobs(hash,symbols,content) VALUES(?,'','blocker')")
            .bind(&hashes[1])
            .execute(&mut *blocker)
            .await?;
        let result: Result<()> = async {
            let conflict = async {
                tokio::time::timeout(Duration::from_secs(4), async {
                    loop {
                        let waits: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM information_schema.INNODB_LOCK_WAITS",
                        )
                        .fetch_one(&pool)
                        .await?;
                        if waits > 0 {
                            break Ok::<_, anyhow::Error>(());
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await??;
                // Close the lock cycle deliberately; normal indexers use the
                // sorted order. This tests recovery with a real 1213 packet.
                sqlx::query(
                    "INSERT IGNORE INTO chunk_blobs(hash,symbols,content) VALUES(?,'','blocker')",
                )
                .bind(&hashes[0])
                .execute(&mut *blocker)
                .await?;
                blocker.rollback().await?;
                Ok::<_, anyhow::Error>(())
            };
            tokio::try_join!(persist_chunks(&run.ctx, 1, "source.rs", &chunks), conflict)?;
            let (_, after): (String, String) =
                sqlx::query_as("SHOW GLOBAL STATUS LIKE 'Innodb_deadlocks'")
                    .fetch_one(&pool)
                    .await?;
            assert!(
                after.parse::<u64>()? > before.parse::<u64>()?,
                "The test must cause a real InnoDB deadlock"
            );
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE run_id=?")
                .bind(&run.ctx.id)
                .fetch_one(&pool)
                .await?;
            assert_eq!(count, 2);
            Ok(())
        }
        .await;
        crate::test_support::close(pool).await?;
        result
    }
    #[tokio::test]
    async fn incomplete_snapshot_is_replaced_before_parsing() -> Result<()> {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://root@127.0.0.1/doccraft_agent_test")?;
        let run = crate::test_support::TestRun::new(pool)?;
        let path = Path::new(&run.ctx.snapshot.task.sources[0]);
        let bytes = std::fs::read(path)?;
        let dir = tempfile::tempdir()?;
        let snapshot = dir.path().join(hash(&bytes));
        std::fs::write(&snapshot, &bytes[..3])?;
        let (_, actual) = snapshot_file(&run.ctx, path, dir.path()).await?;
        assert_eq!(std::fs::read(actual)?, bytes);
        Ok(())
    }

    #[test]
    fn filters_match_single_files_and_all_overlapping_roots() -> Result<()> {
        let path = Path::new("/project/src/main.rs");
        let includes = patterns(&["main.rs".into()])?;
        let empty = patterns(&[])?;
        assert!(matches_filters(
            path,
            &["/project/src/main.rs".into()],
            &includes,
            &empty,
            false
        ));
        for roots in [
            vec!["/project".into(), "/project/src".into()],
            vec!["/project/src".into(), "/project".into()],
        ] {
            assert!(matches_filters(path, &roots, &includes, &empty, false));
            assert!(!matches_filters(path, &roots, &empty, &includes, true));
        }
        Ok(())
    }

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
        assert!(is_implementation("/project/start_backend.sh"));
        assert!(is_implementation("/project/start_backend.bat"));
        assert!(!is_implementation("/project/tests/start.sh"));
    }
    #[test]
    fn explicit_files_keep_relevant_followup_chunks_before_unrelated_files() {
        let e = |id: &str, path: &str| Evidence {
            id: id.into(),
            path: path.into(),
            start: 1,
            end: 1,
            content: String::new(),
        };
        let mut ranked = vec![
            (0, 100, e("first", "/src/agent.js")),
            (0, 90, e("unrelated", "/src/helper.js")),
            (2, 80, e("loop", "/src/agent.js")),
        ];
        prioritize_named(&mut ranked, "agent.js read_result");
        assert_eq!(
            ranked
                .iter()
                .map(|(_, _, e)| e.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "loop", "unrelated"]
        );
    }

    #[test]
    fn shared_blob_locks_have_one_global_order_across_sql_batches() {
        let blobs: Vec<(String, ())> = (0..130)
            .rev()
            .map(|index| (format!("{index:03}"), ()))
            .collect();
        let ordered = ordered_by_hash(&blobs)
            .into_iter()
            .map(|blob| blob.0.as_str())
            .collect::<Vec<_>>();
        assert!(ordered.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(ordered.first().copied(), Some("000"));
        assert_eq!(ordered.last().copied(), Some("129"));
    }
    #[test]
    fn truncated_evidence_keeps_the_matching_logic_and_source_range() {
        let content = format!(
            "{}if (decision.action === 'read_result') {{\n  updateView();\n  continue;\n}}\n{}",
            "// 초기 설명\n".repeat(120),
            "// 뒤쪽 설명\n".repeat(80)
        );
        let (start, excerpt) =
            focused_excerpt(&content, 300, &["read_result".into(), "continue".into()]);
        assert!(start > 0 && excerpt.len() <= 300);
        assert!(excerpt.contains("read_result") && excerpt.contains("continue"));
        assert_eq!(content.get(start..start + excerpt.len()), Some(excerpt));
        assert!(content[..start].ends_with('\n'));
    }
    #[test]
    fn runtime_branches_rank_above_introductory_comment_summaries() {
        let query = "agent.js answer search expand read_result";
        let terms = search_terms(query);
        let comments = "// answer search expand read_result\n".repeat(50);
        let code = "if (decision.action === 'read_result') { readStoredResult(); continue; }\nif (decision.action === 'answer') return answerOf(decision);\nif (decision.action === 'search') await runSearch();";
        assert!(
            evidence_score(query, &terms, "/src/agent.js", "", code)
                > evidence_score(query, &terms, "/src/agent.js", "", &comments)
        );
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
