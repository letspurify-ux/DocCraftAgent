//! File-scoped graph persistence and bounded graph-assisted source navigation.
use crate::{
    code_graph::{CodeGraph, Span, Symbol},
    db,
    model::Evidence,
    runner::RunContext,
    source,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Serialize, Deserialize)]
pub struct FileGraph {
    pub path: String,
    pub graph: CodeGraph,
}

pub async fn save(ctx: &RunContext, file: u64, path: &str, graph: CodeGraph) -> Result<()> {
    db::checkpoint(
        &ctx.pool,
        &ctx.id,
        &format!("graph:file:{file}"),
        &json!(FileGraph {
            path: path.into(),
            graph: graph.at_path(path)
        }),
    )
    .await
}

/// Upgrade old checkpoints from immutable snapshots, never current source files.
pub async fn ensure(ctx: &RunContext) -> Result<()> {
    if db::load_checkpoint(&ctx.pool, &ctx.id, "graph:version").await?
        == Some(json!(crate::parser::VERSION))
    {
        return Ok(());
    }
    let mut after = 0u64;
    let (mut files, mut symbols, mut edges, mut unsupported, mut errors) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    loop {
        ctx.check()?;
        let rows = sqlx::query("SELECT id,path,snapshot_path,language,hash FROM files WHERE run_id=? AND status='indexed' AND id>? ORDER BY id LIMIT 16")
            .bind(&ctx.id).bind(after).fetch_all(&ctx.pool).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after = row.try_get("id")?;
            let saved =
                db::load_checkpoint(&ctx.pool, &ctx.id, &format!("graph:file:{after}")).await?;
            let file: FileGraph = if let Some(saved) = saved {
                serde_json::from_value(saved)?
            } else {
                let path: String = row.try_get("path")?;
                let snapshot: String = row.try_get("snapshot_path")?;
                let language: String = row.try_get("language")?;
                anyhow::ensure!(
                    source::hash(&tokio::fs::read(&snapshot).await?)
                        == row.try_get::<String, _>("hash")?,
                    "SOURCE_GRAPH: stored snapshot changed; cannot reconstruct a trustworthy graph"
                );
                let parsed =
                    source::worker(ctx, std::path::Path::new(&snapshot), &language).await?;
                save(ctx, after, &path, parsed.graph.clone()).await?;
                FileGraph {
                    path: path.clone(),
                    graph: parsed.graph.at_path(&path),
                }
            };
            files += 1;
            symbols += file
                .graph
                .symbols
                .iter()
                .filter(|s| s.kind != "file")
                .count();
            edges += file.graph.edges.len();
            unsupported += usize::from(!file.graph.supported);
            errors += usize::from(file.graph.parse_errors);
        }
    }
    let coverage = json!({"files":files,"symbols":symbols,"edges":edges,"unsupported_files":unsupported,"parse_error_files":errors,
        "syntax_complete":unsupported == 0 && errors == 0,"runtime_resolution":false});
    db::checkpoint(&ctx.pool, &ctx.id, "graph:coverage", &coverage).await?;
    db::checkpoint(
        &ctx.pool,
        &ctx.id,
        "graph:version",
        &json!(crate::parser::VERSION),
    )
    .await?;
    ctx.event(
        "source_graph",
        json!({"stage":"indexed","title":"코드 구조 그래프 저장 완료","coverage":coverage}),
    )
    .await
}

pub async fn file(ctx: &RunContext, file: u64) -> Result<FileGraph> {
    let value = db::load_checkpoint(&ctx.pool, &ctx.id, &format!("graph:file:{file}"))
        .await?
        .context("Missing source graph; indexing must finish before documentation")?;
    Ok(serde_json::from_value(value)?)
}

async fn all(ctx: &RunContext) -> Result<Vec<FileGraph>> {
    let mut cursor = String::new();
    let mut result = vec![];
    loop {
        ctx.check()?;
        let rows = sqlx::query("SELECT step,data FROM checkpoints WHERE run_id=? AND step LIKE 'graph:file:%' AND step>? ORDER BY step LIMIT 16")
            .bind(&ctx.id).bind(&cursor).fetch_all(&ctx.pool).await?;
        if rows.is_empty() {
            return Ok(result);
        }
        for row in rows {
            cursor = row.try_get("step")?;
            result.push(serde_json::from_str(&row.try_get::<String, _>("data")?)?);
        }
    }
}

#[derive(Clone)]
pub struct Target {
    pub path: String,
    pub span: Span,
    pub score: i64,
}

pub fn neighborhood(files: &[FileGraph], query: &str) -> Vec<Target> {
    let terms: HashSet<_> = source::search_terms(query).into_iter().collect();
    let mut by_name: HashMap<&str, Vec<&Symbol>> = HashMap::new();
    for symbol in files
        .iter()
        .flat_map(|f| &f.graph.symbols)
        .filter(|s| s.kind != "file")
    {
        by_name.entry(&symbol.name).or_default().push(symbol);
    }
    let mut scores = HashMap::<String, i64>::new();
    for f in files {
        for s in &f.graph.symbols {
            if s.kind != "file"
                && (terms.contains(&s.name.to_lowercase())
                    || query.contains(&s.qualified_name) && s.qualified_name.len() > 2)
            {
                scores.insert(s.id.clone(), 1000);
            }
        }
    }
    let seeds: HashSet<_> = scores.keys().cloned().collect();
    for f in files {
        for edge in f.graph.edges.iter().filter(|e| e.kind == "calls") {
            let tail = edge
                .target
                .rsplit([':', '.'])
                .next()
                .unwrap_or(&edge.target);
            let targets = by_name.get(tail).map(Vec::as_slice).unwrap_or_default();
            if seeds.contains(&edge.source) {
                for target in targets {
                    scores.entry(target.id.clone()).or_insert(600);
                }
            }
            if targets.iter().any(|s| seeds.contains(&s.id)) {
                scores.entry(edge.source.clone()).or_insert(500);
            }
        }
        for symbol in &f.graph.symbols {
            if seeds.contains(&symbol.id)
                && let Some(parent) = &symbol.parent
            {
                scores.entry(parent.clone()).or_insert(200);
            }
        }
    }
    files
        .iter()
        .flat_map(|f| {
            f.graph.symbols.iter().filter_map(|s| {
                scores.get(&s.id).map(|score| Target {
                    path: f.path.clone(),
                    span: if s.kind == "file"
                        || matches!(
                            s.kind.as_str(),
                            "class" | "type" | "implementation" | "module" | "interface"
                        ) {
                        s.signature.clone()
                    } else {
                        s.span.clone()
                    },
                    score: *score,
                })
            })
        })
        .collect()
}

pub async fn targets(ctx: &RunContext, query: &str) -> Result<Vec<Target>> {
    Ok(neighborhood(&all(ctx).await?, query))
}

/// Whole records only; omitted records remain in the persisted graph and audit.
pub async fn context(ctx: &RunContext, evidence: &[Evidence], max: usize) -> Result<Value> {
    let mut paths: Vec<_> = evidence.iter().map(|e| &e.path).collect();
    paths.sort();
    paths.dedup();
    let mut items = vec![];
    let (mut used, mut deferred) = (0usize, 0usize);
    for path in paths {
        let file_id: Option<u64> = sqlx::query_scalar("SELECT id FROM files WHERE run_id=? AND path=? AND status='indexed' ORDER BY id LIMIT 1")
            .bind(&ctx.id).bind(path).fetch_optional(&ctx.pool).await?;
        let Some(file_id) = file_id else {
            continue;
        };
        let graph = file(ctx, file_id).await?;
        let spans: Vec<_> = evidence
            .iter()
            .filter(|e| &e.path == path)
            .map(|e| (e.start, e.end))
            .collect();
        for symbol in graph
            .graph
            .symbols
            .iter()
            .filter(|s| s.kind != "file" && spans.iter().any(|(a, b)| s.span.overlaps(*a, *b)))
        {
            let value = json!({"path":path,"symbol":symbol});
            let size = serde_json::to_vec(&value)?.len();
            if used + size <= max {
                used += size;
                items.push(value);
            } else {
                deferred += 1;
            }
        }
        for edge in graph
            .graph
            .edges
            .iter()
            .filter(|e| spans.iter().any(|(a, b)| e.span.overlaps(*a, *b)))
        {
            let value = json!({"path":path,"edge":edge});
            let size = serde_json::to_vec(&value)?.len();
            if used + size <= max {
                used += size;
                items.push(value);
            } else {
                deferred += 1;
            }
        }
    }
    Ok(
        json!({"items":items,"deferred_records":deferred,"semantics":"Syntax only. Resolve candidate connections against supplied original evidence; do not assert runtime order from this graph."}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_hop_retrieval_reaches_callers_and_callees_without_cycle_expansion() -> Result<()> {
        let a = crate::code_graph::parse_test(
            "fn start() { validate(); } fn validate() { store(); }",
            "rust",
        )?
        .at_path("a.rs");
        let b =
            crate::code_graph::parse_test("fn store() { validate(); } fn unrelated() {}", "rust")?
                .at_path("b.rs");
        let files = vec![
            FileGraph {
                path: "a.rs".into(),
                graph: a,
            },
            FileGraph {
                path: "b.rs".into(),
                graph: b,
            },
        ];
        let selected = neighborhood(&files, "validate");
        assert!(selected.iter().any(|t| t.path == "b.rs" && t.score == 600));
        assert!(selected.iter().any(|t| t.path == "a.rs" && t.score == 500));
        assert_eq!(selected.iter().filter(|t| t.score == 1000).count(), 1);
        assert!(neighborhood(&files, "unknown_identifier").is_empty());
        Ok(())
    }
}
