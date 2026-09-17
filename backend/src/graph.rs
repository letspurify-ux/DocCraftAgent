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
use std::collections::{BTreeMap, HashMap, HashSet};

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

/// The project-wide call graph, built once per run.
///
/// Both callers resolve a name against every file and use only `calls` edges, so
/// the rest is dropped on the way in: at a few thousand files the branch, return
/// and assignment sites are the majority of the bytes and none of them are read
/// here.
async fn all(ctx: &RunContext) -> Result<std::sync::Arc<Vec<FileGraph>>> {
    ctx.graph_index
        .get_or_try_init(|| async {
            let mut cursor = String::new();
            let mut result: Vec<FileGraph> = vec![];
            loop {
                ctx.check()?;
                let rows = sqlx::query("SELECT step,data FROM checkpoints WHERE run_id=? AND step LIKE 'graph:file:%' AND step>? ORDER BY step LIMIT 16")
                    .bind(&ctx.id).bind(&cursor).fetch_all(&ctx.pool).await?;
                if rows.is_empty() {
                    return Ok(std::sync::Arc::new(result));
                }
                for row in rows {
                    cursor = row.try_get("step")?;
                    let mut file: FileGraph =
                        serde_json::from_str(&row.try_get::<String, _>("data")?)?;
                    file.graph.edges.retain(|e| e.kind == "calls");
                    result.push(file);
                }
            }
        })
        .await
        .cloned()
}

#[derive(Clone)]
pub struct Target {
    pub path: String,
    pub span: Span,
    pub score: i64,
}

/// The source-reading pass uses this small, immutable view to order chunks
/// without exposing database rows or source contents to the graph module.
#[derive(Clone)]
pub struct ChunkSpan {
    pub path: String,
    pub start: u32,
    pub end: u32,
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

fn graph_order(adjacency: &[HashMap<usize, u32>]) -> Vec<usize> {
    let mut seen = vec![false; adjacency.len()];
    let mut order = Vec::with_capacity(adjacency.len());
    for seed in 0..adjacency.len() {
        if seen[seed] {
            continue;
        }
        let mut pending = vec![seed];
        seen[seed] = true;
        while let Some(current) = pending.pop() {
            order.push(current);
            let mut neighbors: Vec<_> = adjacency[current].iter().collect();
            neighbors.sort_by(|(left, left_weight), (right, right_weight)| {
                left_weight
                    .cmp(right_weight)
                    .reverse()
                    .then_with(|| left.cmp(right))
            });
            // `pending` is a stack; push the strongest neighbor last so it is
            // visited first while preserving deterministic tie-breaking.
            for (neighbor, _) in neighbors.into_iter().rev() {
                if !seen[*neighbor] {
                    seen[*neighbor] = true;
                    pending.push(*neighbor);
                }
            }
        }
    }
    order
}

/// The chunk holding a site, found by line. The chunks of one file arrive in
/// line order, so a call site does not have to scan them.
fn chunk_at(chunks: &[ChunkSpan], file_chunks: &[usize], span: &Span) -> Option<usize> {
    let at = file_chunks.partition_point(|index| chunks[*index].end < span.start);
    file_chunks
        .get(at)
        .copied()
        .filter(|index| span.overlaps(chunks[*index].start, chunks[*index].end))
}

fn link(adjacency: &mut [HashMap<usize, u32>], left: usize, right: usize, weight: u32) {
    if left == right {
        return;
    }
    *adjacency[left].entry(right).or_default() += weight;
    *adjacency[right].entry(left).or_default() += weight;
}

/// Order whole-source chunks so graph-connected symbols are read together.
///
/// This is deliberately an ordering operation, not a filter: every supplied
/// chunk is returned exactly once. The graph is syntax-only, so call targets
/// are used as navigation hints and never as proof of runtime execution.
pub async fn order_chunks(ctx: &RunContext, chunks: &[ChunkSpan]) -> Result<Vec<usize>> {
    if chunks.len() < 2 {
        return Ok((0..chunks.len()).collect());
    }
    let files = all(ctx).await?;
    let mut by_path = HashMap::<String, Vec<usize>>::new();
    for (index, chunk) in chunks.iter().enumerate() {
        by_path.entry(chunk.path.clone()).or_default().push(index);
    }
    let mut adjacency = vec![HashMap::<usize, u32>::new(); chunks.len()];
    let mut symbol_chunks = HashMap::<String, Vec<usize>>::new();
    let mut symbols = HashMap::<String, (&str, &Symbol)>::new();
    let mut names = HashMap::<String, Vec<String>>::new();

    for file in files.iter() {
        let Some(file_chunks) = by_path.get(&file.path) else {
            continue;
        };
        for symbol in file.graph.symbols.iter().filter(|s| s.kind != "file") {
            symbols.insert(symbol.id.clone(), (file.path.as_str(), symbol));
            names
                .entry(symbol.name.to_lowercase())
                .or_default()
                .push(symbol.id.clone());
            let covered: Vec<_> = file_chunks
                .iter()
                .copied()
                .filter(|index| {
                    symbol
                        .span
                        .overlaps(chunks[*index].start, chunks[*index].end)
                })
                .collect();
            if !covered.is_empty() {
                symbol_chunks.insert(symbol.id.clone(), covered.clone());
                if let Some(parent) = &symbol.parent
                    && let Some(parent_chunks) = symbol_chunks.get(parent).cloned()
                {
                    for left in &covered {
                        for right in &parent_chunks {
                            link(&mut adjacency, *left, *right, 3);
                        }
                    }
                }
            }
        }
    }

    // Parent symbols may have appeared after their children in a file's graph,
    // so connect the remaining parent/child pairs in a second pass.
    for (_, symbol) in symbols.values() {
        let Some(parent) = &symbol.parent else {
            continue;
        };
        let (Some(children), Some(parents)) =
            (symbol_chunks.get(&symbol.id), symbol_chunks.get(parent))
        else {
            continue;
        };
        for left in children {
            for right in parents {
                link(&mut adjacency, *left, *right, 3);
            }
        }
    }

    for file in files.iter() {
        let Some(file_chunks) = by_path.get(&file.path) else {
            continue;
        };
        for edge in file.graph.edges.iter().filter(|e| e.kind == "calls") {
            // The call site, not every chunk of the symbol that owns it. Linking
            // whole symbols joined k source chunks to m target chunks for every
            // call, which is quadratic in how far a declaration is split; the
            // site is where the connection physically is, and a symbol's own
            // chunks are already tied together by their parent links.
            let Some(source) = chunk_at(chunks, file_chunks, &edge.span) else {
                continue;
            };
            let target = edge
                .target
                .rsplit([':', '.'])
                .next()
                .unwrap_or(&edge.target)
                .to_lowercase();
            let Some(target_ids) = names.get(&target) else {
                continue;
            };
            // Ambiguous syntax names remain useful as local grouping hints, but
            // cap fan-out so one common name cannot join the entire repository.
            for target_id in target_ids.iter().take(8) {
                let Some(target) = symbol_chunks.get(target_id).and_then(|c| c.first()) else {
                    continue;
                };
                link(&mut adjacency, source, *target, 5);
            }
        }
    }

    Ok(graph_order(&adjacency))
}

/// Compact structural records for the passages in one request.
///
/// The complete graph stays in the checkpoints, so this projection is a reading
/// hint, not the record of what exists. Nothing downstream re-checks what it
/// leaves out, so what it drops is dropped for good: the order below is the
/// whole defence, and the request that needs links rather than declarations
/// uses `connections` instead. Its job is to tell the reader
/// what structure the supplied passages contain, which is why it drops
/// storage-only fields, folds the target-less branch and exit sites into
/// per-symbol counts, and names whatever it could not fit instead of reporting
/// a bare total.
fn records(g: &CodeGraph, spans: &[(u32, u32)]) -> Vec<Value> {
    let overlaps = |s: &Span| spans.iter().any(|(a, b)| s.overlaps(*a, *b));
    let owners: HashMap<&str, &str> = g
        .symbols
        .iter()
        .map(|s| (s.id.as_str(), s.qualified_name.as_str()))
        .collect();
    let mut result = vec![];
    for s in g
        .symbols
        .iter()
        .filter(|s| s.kind != "file" && overlaps(&s.span))
    {
        result.push(
            json!({"symbol":s.qualified_name,"kind":s.kind,"lines":[s.span.start,s.span.end]}),
        );
    }
    let mut sites: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut structural = vec![];
    let mut calls = vec![];
    for e in g.edges.iter().filter(|e| overlaps(&e.span)) {
        let owner = owners.get(e.source.as_str()).copied().unwrap_or("<file>");
        if e.target.is_empty() {
            *sites
                .entry(owner)
                .or_default()
                .entry(e.kind.as_str())
                .or_default() += 1;
            continue;
        }
        let link = json!({"from":owner,"kind":e.kind,"to":crate::editorial::excerpt(&e.target, 200),"line":e.span.start});
        if e.kind == "calls" {
            calls.push(link);
        } else {
            structural.push(link);
        }
    }
    // Branch, return, error and assignment sites carry no target, so one record
    // per site would spend the whole budget restating that a line exists. The
    // counts still tell the reader how much conditional and error handling the
    // supplied passage contains, and the passage itself holds the detail.
    //
    // The order below is the order the budget is spent in: the declarations in
    // the passage, then how much branching and error handling each one carries,
    // then its module-level dependencies, and only then the individual call
    // sites, which are the most numerous and the easiest to re-read in the
    // passage itself.
    for (owner, kinds) in sites {
        result.push(json!({"in":owner,"sites":kinds}));
    }
    result.extend(structural);
    result.extend(calls);
    result
}

/// Spend the budget breadth-first so every file in the request is described.
/// Taking one record per file per round means a long first file can no longer
/// consume the whole budget and leave its neighbours undescribed.
fn ration(files: &[(String, Vec<Value>)], max: usize) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut items = vec![];
    let mut taken = vec![0usize; files.len()];
    let mut used = 0usize;
    let mut progress = true;
    while progress {
        progress = false;
        for (index, (path, records)) in files.iter().enumerate() {
            let Some(record) = records.get(taken[index]) else {
                continue;
            };
            let mut value = record.clone();
            value["path"] = json!(path);
            let size = serde_json::to_vec(&value)?.len() + 1;
            if used + size > max {
                continue;
            }
            used += size;
            taken[index] += 1;
            items.push(value);
            progress = true;
        }
    }
    let omitted = files
        .iter()
        .enumerate()
        .filter(|(index, (_, records))| taken[*index] < records.len())
        .map(|(index, (path, records))| {
            json!({"path":path,"omitted_records":records.len() - taken[index]})
        })
        .collect();
    Ok((items, omitted))
}

/// The bounded structural hint for a leaf, which reads source. What it cannot
/// carry is named per file; the omitted records stay in the persisted graph but
/// are not revisited, so they are unresolved rather than absent.
pub async fn context(ctx: &RunContext, evidence: &[Evidence], max: usize) -> Result<Value> {
    let mut paths: Vec<_> = evidence.iter().map(|e| &e.path).collect();
    paths.sort();
    paths.dedup();
    let mut files = vec![];
    let mut ungraphed = vec![];
    for path in paths {
        let file_id: Option<u64> = sqlx::query_scalar("SELECT id FROM files WHERE run_id=? AND path=? AND status='indexed' ORDER BY id LIMIT 1")
            .bind(&ctx.id).bind(path).fetch_optional(&ctx.pool).await?;
        let Some(file_id) = file_id else {
            // Silence here would read as "this file has no structure", so the
            // gap is named instead.
            ungraphed.push(json!({"path":path,"reason":"not_indexed"}));
            continue;
        };
        let graph = file(ctx, file_id).await?.graph;
        let spans: Vec<_> = evidence
            .iter()
            .filter(|e| &e.path == path)
            .map(|e| (e.start, e.end))
            .collect();
        let records = records(&graph, &spans);
        if records.is_empty() {
            ungraphed.push(json!({"path":path,"reason":if graph.supported {"no_declarations_in_passage"} else {"unsupported_language"}}));
            continue;
        }
        files.push((path.clone(), records));
    }
    let (items, omitted) = ration(&files, max)?;
    let deferred: usize = omitted
        .iter()
        .filter_map(|o| o["omitted_records"].as_u64())
        .sum::<u64>() as usize;
    Ok(
        json!({"items":items,"omitted":omitted,"files_without_graph":ungraphed,"deferred_records":deferred,
            "semantics":"Syntax only, and never a citation: these records carry names and line numbers, not evidence IDs, so cite the supplied passages instead. Resolve candidate connections against supplied original evidence; do not assert runtime order from this graph. `sites` counts branch/returns/error_path/writes/awaits sites inside a symbol: read the supplied passage for the actual conditions rather than treating a count as a described behavior. `omitted` and `files_without_graph` name structure this request could not carry; treat those as unresolved, never as evidence that the code has none."}),
    )
}

/// One call between two files, folded across every site that makes it.
///
/// Folding by symbol pair rather than by site is what makes this table small
/// enough to send: the same pair is typically called from many lines, and the
/// count carries that without spending a record per line.
///
/// `to_path` is empty when the target name is declared in more than one file,
/// and `candidates` then names those files. A resolved link is a much stronger
/// claim than a bare callee name, so an ambiguous one is never dressed up as
/// one.
#[derive(Clone)]
pub struct CrossLink {
    pub from_path: String,
    pub from: String,
    pub to_path: String,
    pub to: String,
    pub candidates: Vec<String>,
    pub calls: usize,
}

/// Every call in the run, bucketed so that none is unaccounted for.
pub struct LinkIndex {
    pub cross: Vec<CrossLink>,
    pub paths: HashSet<String>,
    /// Calls whose target is declared in the same file, per file. A reduction
    /// reads its children's summaries, which already describe what happens
    /// inside one file, so these are counted rather than sent.
    pub internal: HashMap<String, usize>,
    /// Calls whose target name no indexed file declares, per file: library and
    /// runtime calls, and anything the parser could not resolve.
    pub unresolved: HashMap<String, usize>,
}

/// Where one name is declared, resolved once for the whole run. `files` is
/// ascending and distinct because symbols are walked file by file, so telling
/// "declared nowhere else", "declared in exactly one other file" and "declared
/// in several" apart reads at most three of its entries, whatever the name.
#[derive(Default)]
struct NameEntry<'a> {
    files: Vec<usize>,
    pick: HashMap<usize, &'a Symbol>,
}

const AMBIGUOUS_CANDIDATES_MAX: usize = 4;
const NAMED_FILES_MAX: usize = 12;

/// Resolve every call target to its declaring file, once per run.
///
/// The memoised `all()` index already holds exactly what this needs - every
/// symbol, and the `calls` edges - so resolution costs no further query, and a
/// reduction node filters this table by its own paths instead of reloading a
/// checkpoint per file.
async fn link_index(ctx: &RunContext) -> Result<std::sync::Arc<LinkIndex>> {
    ctx.cross_links
        .get_or_try_init(|| async {
            let files = all(ctx).await?;
            ctx.check()?;
            let index = resolve_links(&files);
            ctx.check()?;
            Ok(std::sync::Arc::new(index))
        })
        .await
        .cloned()
}

/// Resolution and folding, kept free of the database so it can be exercised on
/// a parsed graph directly.
fn resolve_links(files: &[FileGraph]) -> LinkIndex {
    // Per name rather than per call site. A name like `get` or `run` is
    // declared in hundreds of files, and rebuilding its candidate list
    // for every call that mentions it makes the pass quadratic in how
    // common the name is; here each name is resolved once and every
    // call site then costs a binary search.
    let mut by_name: HashMap<&str, NameEntry> = HashMap::new();
    for (index, f) in files.iter().enumerate() {
        for s in f.graph.symbols.iter().filter(|s| s.kind != "file") {
            let entry = by_name.entry(s.name.as_str()).or_default();
            if entry.files.last() != Some(&index) {
                entry.files.push(index);
            }
            entry.pick.entry(index).or_insert(s);
        }
    }
    let mut folded: HashMap<(usize, &str, Option<usize>, &str), usize> = HashMap::new();
    let mut internal: HashMap<String, usize> = HashMap::new();
    let mut unresolved: HashMap<String, usize> = HashMap::new();
    for (index, f) in files.iter().enumerate() {
        let owners: HashMap<&str, &str> = f
            .graph
            .symbols
            .iter()
            .map(|s| (s.id.as_str(), s.qualified_name.as_str()))
            .collect();
        for e in f.graph.edges.iter().filter(|e| e.kind == "calls") {
            let tail = e.target.rsplit([':', '.']).next().unwrap_or(&e.target);
            let owner = owners.get(e.source.as_str()).copied().unwrap_or("<file>");
            let Some(entry) = by_name.get(tail) else {
                *unresolved.entry(f.path.clone()).or_default() += 1;
                continue;
            };
            let mut others = entry.files.iter().copied().filter(|i| *i != index);
            let (first, second) = (others.next(), others.next());
            match (first, second) {
                (None, _) => *internal.entry(f.path.clone()).or_default() += 1,
                (Some(file), None) => {
                    let target = entry.pick[&file].qualified_name.as_str();
                    *folded
                        .entry((index, owner, Some(file), target))
                        .or_default() += 1;
                }
                _ => *folded.entry((index, owner, None, tail)).or_default() += 1,
            }
        }
    }
    let mut cross: Vec<CrossLink> = folded
        .into_iter()
        .map(|((from, owner, target, to), calls)| {
            let (to_path, candidates) = match target {
                Some(file) => (files[file].path.clone(), vec![]),
                None => {
                    let paths: Vec<String> = by_name
                        .get(to)
                        .map(|entry| {
                            entry
                                .files
                                .iter()
                                .filter(|i| **i != from)
                                .take(AMBIGUOUS_CANDIDATES_MAX)
                                .map(|i| files[*i].path.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    (String::new(), paths)
                }
            };
            CrossLink {
                from_path: files[from].path.clone(),
                from: owner.to_string(),
                to_path,
                to: to.to_string(),
                candidates,
                calls,
            }
        })
        .collect();
    // Resolved links before ambiguous ones, then the busiest pair
    // first, so rationing keeps the strongest and most certain links.
    cross.sort_by(|a, b| {
        a.from_path
            .cmp(&b.from_path)
            .then(a.to_path.is_empty().cmp(&b.to_path.is_empty()))
            .then(b.calls.cmp(&a.calls))
            .then(a.to.cmp(&b.to))
            .then(a.from.cmp(&b.from))
    });
    LinkIndex {
        cross,
        paths: files.iter().map(|f| f.path.clone()).collect(),
        internal,
        unresolved,
    }
}

/// The structural hint for a reduction: which supplied file calls which.
///
/// A reduction reads its children's summaries rather than their source, so what
/// each symbol does is already in the request and the connections between the
/// children are not. Declarations, per-symbol site counts and same-file calls
/// are therefore left out entirely, and the whole budget buys cross-file links -
/// the inverse of what a leaf needs, which is why this is not `context`.
pub async fn connections(ctx: &RunContext, evidence: &[Evidence], max: usize) -> Result<Value> {
    let index = link_index(ctx).await?;
    let scope: HashSet<&str> = evidence.iter().map(|e| e.path.as_str()).collect();
    let projection = project(&index, &scope, max)?;
    // What the projection had to leave out decides whether a reduction can see
    // past its own group, and it is only ever sent to the model. Recording it
    // makes the rationing measurable from a finished run instead of guessed at.
    ctx.event(
        "graph_projection",
        json!({"stage":"understanding","files":scope.len(),"budget_bytes":max,
            "links_sent":projection["items"].as_array().map_or(0, Vec::len),
            "links_deferred":projection["deferred_links"],
            "calls_leaving":projection["not_shown"]["calls_leaving_this_request"],
            "same_file_calls":projection["not_shown"]["same_file_calls"],
            "undeclared_names":projection["not_shown"]["calls_to_undeclared_names"],
            "files_without_links":projection["files_without_links"].as_array().map_or(0, Vec::len),
            "files_without_graph":projection["files_without_graph"].as_array().map_or(0, Vec::len)}),
    )
    .await?;
    Ok(projection)
}

fn project(index: &LinkIndex, scope: &HashSet<&str>, max: usize) -> Result<Value> {
    let mut by_file: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    let mut leaving = 0usize;
    for link in &index.cross {
        if !scope.contains(link.from_path.as_str()) {
            continue;
        }
        let inside = if link.to_path.is_empty() {
            link.candidates.iter().any(|c| scope.contains(c.as_str()))
        } else {
            scope.contains(link.to_path.as_str())
        };
        if !inside {
            leaving += link.calls;
            continue;
        }
        let record = if link.to_path.is_empty() {
            json!({"from":link.from,"to":link.to,"declared_in":link.candidates,"calls":link.calls,"resolved":false})
        } else {
            json!({"from":link.from,"to":format!("{}::{}",link.to_path,link.to),"calls":link.calls})
        };
        by_file
            .entry(link.from_path.as_str())
            .or_default()
            .push(record);
    }
    let linked: HashSet<&str> = by_file.keys().copied().collect();
    let mut without_links: Vec<&str> = scope
        .iter()
        .copied()
        .filter(|p| index.paths.contains(*p) && !linked.contains(p))
        .collect();
    let mut without_graph: Vec<&str> = scope
        .iter()
        .copied()
        .filter(|p| !index.paths.contains(*p))
        .collect();
    without_links.sort_unstable();
    without_graph.sort_unstable();
    let files: Vec<(String, Vec<Value>)> = by_file
        .into_iter()
        .map(|(path, records)| (path.to_string(), records))
        .collect();
    let (items, omitted) = ration(&files, max)?;
    let deferred: usize = omitted
        .iter()
        .filter_map(|o| o["omitted_records"].as_u64())
        .sum::<u64>() as usize;
    let counted =
        |m: &HashMap<String, usize>| -> usize { scope.iter().filter_map(|p| m.get(*p)).sum() };
    Ok(json!({
        "items":items,"omitted":omitted,"deferred_links":deferred,
        "files_without_links":named(&without_links),"files_without_graph":named(&without_graph),
        "not_shown":{"same_file_calls":counted(&index.internal),
            "calls_leaving_this_request":leaving,
            "calls_to_undeclared_names":counted(&index.unresolved)},
        "semantics":"Syntax only, and never a citation: `from` and `to` are declaration names with a call count folded across every site, not evidence IDs, so cite the supplied child findings instead. A `to` of the form `path::symbol` means that name is declared in exactly one other supplied file; a record with `resolved:false` means several files declare the name and `declared_in` lists them, so treat which one runs as unresolved. `calls` counts call sites, not executions, and this table carries no order or condition: do not assert runtime sequence from it. Same-file calls are left out because the child summaries already describe them. `omitted`, `files_without_links` and `files_without_graph` name connections this request could not carry or could not see; treat those as unresolved, never as evidence that none exist."
    }))
}

/// Name the files, up to a cap, rather than report a bare total: a reader that
/// sees only a count cannot tell which summary to distrust.
fn named(paths: &[&str]) -> Value {
    if paths.len() <= NAMED_FILES_MAX {
        return json!(paths);
    }
    json!({"paths":&paths[..NAMED_FILES_MAX],"further":paths.len()-NAMED_FILES_MAX})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(path: &str, source: &str) -> Result<FileGraph> {
        Ok(FileGraph {
            path: path.into(),
            graph: crate::code_graph::parse_test(source, "rust")?.at_path(path),
        })
    }

    fn scope_of<'a>(paths: &[&'a str]) -> HashSet<&'a str> {
        paths.iter().copied().collect()
    }

    #[test]
    fn a_call_into_another_file_folds_into_one_counted_link() -> Result<()> {
        let files = [
            parsed("a.rs", "fn run() { helper(); helper(); helper(); }")?,
            parsed("b.rs", "fn helper() {}")?,
        ];
        let index = resolve_links(&files);
        assert_eq!(index.cross.len(), 1);
        let link = &index.cross[0];
        assert_eq!(
            (link.from_path.as_str(), link.from.as_str()),
            ("a.rs", "run")
        );
        assert_eq!(
            (link.to_path.as_str(), link.to.as_str()),
            ("b.rs", "helper")
        );
        // Three sites, one record: the count is what keeps the table sendable.
        assert_eq!(link.calls, 3);
        let value = project(&index, &scope_of(&["a.rs", "b.rs"]), 4000)?;
        assert_eq!(value["items"][0]["to"], "b.rs::helper");
        assert_eq!(value["items"][0]["calls"], 3);
        assert_eq!(value["items"][0]["path"], "a.rs");
        Ok(())
    }

    #[test]
    fn a_same_file_call_is_counted_rather_than_sent() -> Result<()> {
        let files = [parsed("a.rs", "fn run() { helper(); }\nfn helper() {}")?];
        let index = resolve_links(&files);
        assert!(index.cross.is_empty());
        // The child summary already describes what happens inside one file, so
        // the budget must not be spent restating it - but it is still reported.
        assert_eq!(index.internal.get("a.rs").copied(), Some(1));
        let value = project(&index, &scope_of(&["a.rs"]), 4000)?;
        assert_eq!(value["not_shown"]["same_file_calls"], 1);
        assert_eq!(value["files_without_links"][0], "a.rs");
        Ok(())
    }

    #[test]
    fn an_ambiguous_target_is_never_dressed_up_as_resolved() -> Result<()> {
        let files = [
            parsed("a.rs", "fn run() { helper(); }")?,
            parsed("b.rs", "fn helper() {}")?,
            parsed("c.rs", "fn helper() {}")?,
        ];
        let index = resolve_links(&files);
        assert_eq!(index.cross.len(), 1);
        assert!(index.cross[0].to_path.is_empty());
        let value = project(&index, &scope_of(&["a.rs", "b.rs", "c.rs"]), 4000)?;
        let record = &value["items"][0];
        assert_eq!(record["resolved"], false);
        assert_eq!(record["to"], "helper");
        assert_eq!(record["declared_in"], json!(["b.rs", "c.rs"]));
        Ok(())
    }

    #[test]
    fn a_call_to_a_name_no_file_declares_is_separated_from_a_missing_link() -> Result<()> {
        let files = [parsed(
            "a.rs",
            "fn run() { println!(\"x\"); external_thing(); }",
        )?];
        let index = resolve_links(&files);
        assert!(index.cross.is_empty());
        assert!(index.unresolved.get("a.rs").copied().unwrap_or(0) >= 1);
        let value = project(&index, &scope_of(&["a.rs"]), 4000)?;
        assert!(
            value["not_shown"]["calls_to_undeclared_names"]
                .as_u64()
                .unwrap_or(0)
                >= 1
        );
        Ok(())
    }

    #[test]
    fn a_link_leaving_the_request_is_counted_not_silently_dropped() -> Result<()> {
        let files = [
            parsed("a.rs", "fn run() { helper(); helper(); }")?,
            parsed("b.rs", "fn helper() {}")?,
        ];
        let index = resolve_links(&files);
        let value = project(&index, &scope_of(&["a.rs"]), 4000)?;
        assert_eq!(value["items"].as_array().map(Vec::len), Some(0));
        // A reduction that cannot see the far end must know the group is open,
        // not conclude that the caller talks to nobody.
        assert_eq!(value["not_shown"]["calls_leaving_this_request"], 2);
        Ok(())
    }

    #[test]
    fn a_file_the_graph_never_saw_is_named_apart_from_one_without_links() -> Result<()> {
        let files = [
            parsed("a.rs", "fn run() { helper(); }")?,
            parsed("b.rs", "fn helper() {}")?,
        ];
        let index = resolve_links(&files);
        let value = project(&index, &scope_of(&["a.rs", "b.rs", "z.md"]), 4000)?;
        assert_eq!(value["files_without_graph"], json!(["z.md"]));
        // b.rs is graphed and calls nothing; that is not the same as unseen.
        assert_eq!(value["files_without_links"], json!(["b.rs"]));
        Ok(())
    }

    #[test]
    fn a_name_hundreds_of_files_declare_resolves_once_and_caps_its_candidates() -> Result<()> {
        // Resolving per call site would build this candidate list 300 times.
        let mut files = vec![parsed(
            "caller.rs",
            "fn run() { handle(); handle(); handle(); }",
        )?];
        for i in 0..300 {
            files.push(parsed(&format!("m{i:03}.rs"), "fn handle() {}")?);
        }
        let index = resolve_links(&files);
        assert_eq!(index.cross.len(), 1);
        let link = &index.cross[0];
        assert!(link.to_path.is_empty());
        assert_eq!(link.calls, 3);
        assert_eq!(link.candidates.len(), AMBIGUOUS_CANDIDATES_MAX);
        Ok(())
    }

    #[test]
    fn a_squeezed_budget_still_describes_every_calling_file() -> Result<()> {
        let files = [
            parsed("a.rs", "fn a1() { t1(); t1(); t1(); }\nfn a2() { t2(); }")?,
            parsed("b.rs", "fn b1() { t1(); }\nfn b2() { t2(); t2(); }")?,
            parsed("t.rs", "fn t1() {}\nfn t2() {}")?,
        ];
        let index = resolve_links(&files);
        let scope = scope_of(&["a.rs", "b.rs", "t.rs"]);
        let full = project(&index, &scope, 4000)?;
        assert_eq!(full["items"].as_array().map(Vec::len), Some(4));
        let tight = project(&index, &scope, 150)?;
        let items = tight["items"].as_array().cloned().unwrap_or_default();
        assert!(!items.is_empty() && items.len() < 4);
        let described: HashSet<&str> = items.iter().filter_map(|i| i["path"].as_str()).collect();
        // Breadth-first: a long first file cannot consume the whole budget.
        assert_eq!(described.len(), 2);
        assert!(tight["deferred_links"].as_u64().unwrap_or(0) > 0);
        // The busiest pair survives the squeeze.
        let kept = items.iter().find(|i| i["path"] == "a.rs").context("a.rs")?;
        assert_eq!(kept["calls"], 3);
        Ok(())
    }

    #[test]
    fn site_records_fold_targetless_edges_and_drop_storage_only_fields() -> Result<()> {
        let g = crate::code_graph::parse_test(
            "fn run(x: i32) -> i32 { if x > 0 { return helper(x); } while x > 1 { break; } 0 }\nfn helper(v: i32) -> i32 { v }\n",
            "rust",
        )?
        .at_path("a.rs");
        let items = records(&g, &[(1, 99)]);
        let text = serde_json::to_string(&items)?;
        assert!(
            items
                .iter()
                .any(|r| r["symbol"] == "run" && r["kind"] == "function")
        );
        assert!(
            items
                .iter()
                .any(|r| r["from"] == "run" && r["to"] == "helper")
        );
        // Every branch/return site of one symbol becomes a single counted
        // record instead of one near-empty record per site.
        let counted = items
            .iter()
            .find(|r| r["in"] == "run")
            .context("run site counts")?;
        assert!(counted["sites"]["branch"].as_u64().unwrap_or(0) >= 2);
        assert_eq!(counted["sites"]["returns"], 1);
        assert_eq!(items.iter().filter(|r| r["in"] == "run").count(), 1);
        // Symbol ids and byte offsets are storage, not something the reader can
        // check against a passage.
        assert!(!text.contains("start_byte"), "{text}");
        assert!(!text.contains("\"id\""), "{text}");
        Ok(())
    }

    #[test]
    fn a_projection_over_several_files_carries_both_ends_of_a_connection() -> Result<()> {
        // a.rs calls validate(), which b.rs declares. A reduction reads only its
        // children's summaries, so unless the projection carries the call site
        // and the declaration together it has nothing to name that link with and
        // has to write the connection off as uncertain.
        let a =
            crate::code_graph::parse_test("fn start() { validate(); }", "rust")?.at_path("a.rs");
        let b = crate::code_graph::parse_test("fn validate() -> bool { true }", "rust")?
            .at_path("b.rs");
        let files = vec![
            ("a.rs".to_string(), records(&a, &[(1, 99)])),
            ("b.rs".to_string(), records(&b, &[(1, 99)])),
        ];
        let (items, omitted) = ration(&files, 4000)?;
        assert!(omitted.is_empty(), "{omitted:?}");
        assert!(
            items
                .iter()
                .any(|r| r["path"] == "a.rs" && r["from"] == "start" && r["to"] == "validate"),
            "{items:?}"
        );
        assert!(
            items
                .iter()
                .any(|r| r["path"] == "b.rs" && r["symbol"] == "validate"),
            "{items:?}"
        );
        Ok(())
    }

    #[test]
    fn every_file_in_a_request_is_described_and_omissions_are_named() -> Result<()> {
        let files = vec![
            (
                "a.rs".to_string(),
                vec![
                    json!({"symbol":"a1"}),
                    json!({"symbol":"a2"}),
                    json!({"symbol":"a3"}),
                ],
            ),
            ("b.rs".to_string(), vec![json!({"symbol":"b1"})]),
        ];
        let one = serde_json::to_vec(&json!({"path":"a.rs","symbol":"a1"}))?.len() + 1;
        let (items, omitted) = ration(&files, one * 2)?;
        // A budget for two records describes both files, not the first twice.
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|r| r["path"] == "b.rs"));
        assert_eq!(omitted, vec![json!({"path":"a.rs","omitted_records":2})]);
        // With room for everything nothing is reported as omitted.
        let (all, none) = ration(&files, one * 10)?;
        assert_eq!(all.len(), 4);
        assert!(none.is_empty());
        Ok(())
    }

    #[test]
    fn a_call_site_maps_to_one_chunk_by_line() {
        let span = |start: u32, end: u32| ChunkSpan {
            path: "a.rs".into(),
            start,
            end,
        };
        let chunks = vec![span(1, 100), span(101, 200), span(201, 300)];
        let file_chunks = vec![0usize, 1, 2];
        let at = |line: u32| {
            chunk_at(
                &chunks,
                &file_chunks,
                &Span {
                    start: line,
                    end: line,
                    ..Default::default()
                },
            )
        };
        // A site resolves to the one chunk that holds it, so a declaration split
        // across chunks no longer multiplies out against its call targets.
        assert_eq!(at(1), Some(0));
        assert_eq!(at(100), Some(0));
        assert_eq!(at(101), Some(1));
        assert_eq!(at(300), Some(2));
        assert_eq!(at(301), None);
    }

    #[test]
    fn graph_order_keeps_connected_chunks_adjacent_and_preserves_isolates() {
        let mut adjacency = vec![
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        ];
        link(&mut adjacency, 0, 2, 5);
        link(&mut adjacency, 2, 3, 3);
        let order = graph_order(&adjacency);
        let positions: HashMap<_, _> = order
            .iter()
            .enumerate()
            .map(|(position, index)| (*index, position))
            .collect();
        assert!(positions[&2].abs_diff(positions[&0]) == 1);
        assert!(positions[&3].abs_diff(positions[&2]) == 1);
        assert_eq!(
            order.iter().copied().collect::<HashSet<_>>(),
            (0..4).collect()
        );
    }

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
