//! Exhaustive, resumable source reading. Request size limits split work; they
//! never truncate the file inventory or mark unread input as understood.
use crate::{
    db, editorial, llm,
    model::{Evidence, LlmConfig},
    planning::{Discovery, SourceBrief, pack_evidence, validate_brief},
    runner::{RunContext, fatal, is_budget},
    source,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::collections::{HashMap, HashSet};

const READ: &str = "Read source evidence before planning, independently of any future documentation purpose. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[]}. Read ALL supplied passages. Explain module responsibilities, entry points, inputs, conditions, decisions, state/data changes, outputs, consumers and errors/cancellation/lifecycle. Preserve distinct public workflows, important branches, and producer/consumer contracts. Imports and call names are navigation candidates, not proof of execution. Mark unresolved connections. Runtime observations require implementation passages; tests/docs describe context only. Use at most 12 findings, observations under 1000 characters, at most 6 evidence IDs each, and 8 uncertainties. Do not design a table of contents or force unrelated flows into one sequence. Use the requested language.";
const REDUCE: &str = "Read source evidence before planning. Integrate ALL supplied child summaries into a higher-level source overview. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[]}. Preserve distinct workflows, module contracts, state changes, result consumers, conditional/error/cancel branches and unresolved cross-module links. Child findings have already been checked against their original passages. Preserve their important workflows and source anchors even when those passages are not repeated in this bounded request. previously_read anchors identify those originals; they support carrying the child observation, not inventing new facts. source_graph lists the declarations and connections of the files those children read: a connection there is a syntax candidate, so use it to name and follow a link between two children instead of dropping it, and mark it unresolved rather than asserting execution it does not prove. A finding about such a link still cites supplied evidence IDs at its ends; graph records carry names, not IDs. Use newly supplied original evidence to establish NEW connections; mark any other cross-module synthesis as uncertain. Never invent an execution order to join independent workflows. Do not assume a missing excerpt is absent from the project. Include at most 12 findings, each observation under 1000 characters and with 1-6 supplied evidence IDs, and at most 8 uncertainties. Do not produce a document outline. Use the requested language.";

// Ceilings, not the operating limits. Both effective limits are derived from
// the configured context window so that a request always fits the gate in
// budget::check; these only stop a very large context from growing a single
// batch or summary without bound.
const SUMMARY_MAX_BYTES: usize = 48_000;
const SOURCE_INPUT_MAX_BYTES: usize = 96_000;
// A leaf must cite every passage it was given, and one brief can name at most
// MAX_FINDINGS x MAX_EVIDENCE_IDS distinct passages. Filling a batch to that
// exact ceiling would demand a perfect partition - twelve findings of six
// distinct passages each, no passage shared - which no reading produces, so the
// batch takes half of it and leaves the rest for passages that support more
// than one observation.
const MAX_BATCH_PASSAGES: usize =
    crate::planning::MAX_FINDINGS * crate::planning::MAX_EVIDENCE_IDS / 2;
// Instructions, policies and the per-evidence class table that ride along with
// every understanding request. The structural hint is not in here: it scales
// with the request instead, so a small context window still leaves room to read
// source rather than reserving a fixed block it cannot afford.
const REQUEST_OVERHEAD_BYTES: usize = 16_000;
// The structural hint describes every file in the batch, not just the first.
const GRAPH_CONTEXT_MAX_BYTES: usize = 12_000;

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub key: String,
    pub files: Vec<String>,
    pub children: Vec<String>,
    pub discovery: Discovery,
    #[serde(default)]
    pub unresolved_nodes: usize,
    #[serde(default)]
    pub validation_issues: Vec<String>,
    #[serde(default)]
    pub unverified_brief: Option<SourceBrief>,
    #[serde(default)]
    pub unverified_output: Option<String>,
}

/// Salvage independently validated observations; never relabel unsupported runtime claims.
fn validated_findings(brief: &SourceBrief, evidence: &[Evidence]) -> Vec<crate::planning::Finding> {
    brief
        .findings
        .iter()
        .cloned()
        .filter_map(|finding| {
            let mut one = SourceBrief {
                findings: vec![finding],
                uncertainties: vec![],
                followup_queries: vec![],
            };
            validate_brief(&mut one, evidence, true).ok()?;
            one.findings.pop()
        })
        .collect()
}
fn salvage(mut brief: SourceBrief, evidence: &[Evidence], maximum: usize) -> SourceBrief {
    brief.findings = validated_findings(&brief, evidence)
        .into_iter()
        .take(12)
        .collect();
    brief.uncertainties = brief
        .uncertainties
        .into_iter()
        .chain(brief.followup_queries)
        .filter(|s| !s.trim().is_empty() && s.len() <= 1500)
        .take(7)
        .collect();
    brief.uncertainties.push("일부 소스 관찰은 근거 검증을 통과하지 못해 제외되었습니다. 해당 분석 묶음의 검증 오류와 원문을 재검토해야 합니다.".into());
    brief.followup_queries = vec![];
    while serde_json::to_vec(&brief).map_or(true, |v| v.len() > maximum)
        && !brief.findings.is_empty()
    {
        brief.findings.pop();
    }
    brief
}

/// Recover only independently checkable findings; malformed JSON contributes no claims.
fn recover_output(
    output: &str,
    children: &[Node],
    evidence: &[Evidence],
    maximum: usize,
) -> SourceBrief {
    let mut findings = children
        .iter()
        .flat_map(|n| n.discovery.brief.findings.clone())
        .collect::<Vec<_>>();
    if let Ok(value) = llm::decode::<serde_json::Value>(output)
        && let Some(items) = value.get("findings").and_then(|v| v.as_array())
    {
        findings.extend(
            items
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok()),
        );
    } else {
        // The response did not parse as a whole - most often it was cut off
        // inside an observation. Discarding the batch would lose the readings
        // the model did finish, and a leaf batch is the only place those
        // passages are ever read.
        findings.extend(
            llm::array_prefix(output, "findings")
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok()),
        );
    }
    salvage(
        SourceBrief {
            findings,
            uncertainties: vec![],
            followup_queries: vec![],
        },
        evidence,
        maximum,
    )
}

/// Raw bytes one understanding request may carry for source and its structural
/// hint together, mirroring the gate in `budget::check` so that packing never
/// builds a request the gate rejects.
fn request_space(l: &LlmConfig, extra_margin: u32) -> usize {
    crate::budget::packing_limit(l, extra_margin, REQUEST_OVERHEAD_BYTES)
        .min(SOURCE_INPUT_MAX_BYTES + GRAPH_CONTEXT_MAX_BYTES)
}

/// The hint shares the request with the source it describes, so it takes a
/// share rather than a fixed block.
fn graph_budget(space: usize) -> usize {
    (space / 8).min(GRAPH_CONTEXT_MAX_BYTES)
}

/// Raw source bytes one request may carry, once the hint has taken its share.
fn request_limit(l: &LlmConfig, extra_margin: u32) -> usize {
    let space = request_space(l, extra_margin);
    space.saturating_sub(graph_budget(space))
}

fn margin(ctx: &RunContext) -> u32 {
    // The provider may already have forced extra margin on this run. Ignoring
    // it rebuilds the same oversized request on every retry.
    ctx.extra_margin.load(std::sync::atomic::Ordering::Relaxed)
}

fn input_limit(ctx: &RunContext) -> usize {
    request_limit(&ctx.snapshot.settings.llm, margin(ctx))
}

fn graph_limit(ctx: &RunContext) -> usize {
    graph_budget(request_space(&ctx.snapshot.settings.llm, margin(ctx)))
}

/// A reduction carries its children's summaries whole, so no single summary may
/// claim more than its share of one request.
fn summary_maximum(limit: usize) -> usize {
    (limit / 6).clamp(4_000, SUMMARY_MAX_BYTES)
}

fn summary_limit(ctx: &RunContext) -> usize {
    summary_maximum(input_limit(ctx))
}

/// Bytes a reduction must carry before any original passage is re-read: every
/// child summary, plus one `source_anchors` entry and one `evidence_classes`
/// entry per retained original. Only what is left may be spent on originals.
fn mandatory_bytes(children: &[Node]) -> Result<usize> {
    let mut total = 0usize;
    for child in children {
        total = total.saturating_add(serde_json::to_vec(&child.discovery.brief)?.len());
        for e in &child.discovery.evidence {
            total = total.saturating_add(2 * (e.id.len() + e.path.len()) + 128);
        }
    }
    Ok(total)
}

/// Every byte belongs to one segment, including long lines and Unicode.
pub(crate) fn segments(e: &Evidence, maximum: usize) -> Vec<Evidence> {
    let mut result = vec![];
    let mut offset = 0;
    let mut line = e.start;
    while offset < e.content.len() {
        let mut end = (offset + maximum.max(4)).min(e.content.len());
        while !e.content.is_char_boundary(end) {
            end -= 1;
        }
        if end < e.content.len()
            && let Some(n) = e.content[offset..end].rfind('\n')
        {
            end = offset + n + 1;
        }
        let content = e.content[offset..end].to_string();
        let lines = content.bytes().filter(|b| *b == b'\n').count() as u32;
        result.push(Evidence {
            id: source::hash(
                format!("{}:{line}:{}", e.path, source::hash(content.as_bytes())).as_bytes(),
            ),
            path: e.path.clone(),
            start: line,
            end: line + lines - u32::from(content.ends_with('\n')),
            content,
        });
        line += lines;
        offset = end;
    }
    result
}

async fn node(
    ctx: &RunContext,
    system: &str,
    evidence: Vec<Evidence>,
    children: &[Node],
    final_overview: bool,
) -> Result<Node> {
    let summaries = children
        .iter()
        .map(|n| &n.discovery.brief)
        .collect::<Vec<_>>();
    let mut available = evidence.clone();
    let mut seen = available
        .iter()
        .map(|e| e.id.clone())
        .collect::<HashSet<_>>();
    for child in children {
        for original in &child.discovery.evidence {
            if child
                .discovery
                .brief
                .findings
                .iter()
                .any(|f| f.evidence_ids.contains(&original.id))
                && seen.insert(original.id.clone())
            {
                available.push(original.clone());
            }
        }
    }
    let anchors = children
        .iter()
        .flat_map(|n| {
            n.discovery.evidence.iter().filter(|e| {
                n.discovery
                    .brief
                    .findings
                    .iter()
                    .any(|f| f.evidence_ids.contains(&e.id))
            })
        })
        .map(|e| json!({"id":e.id,"path":e.path,"previously_read":true}))
        .collect::<Vec<_>>();
    let mut input = json!({"phase":if children.is_empty(){"understanding_batch"}else{"understanding_reduce"},
        "language":ctx.snapshot.task.language,"final_pass":true,"evidence":evidence,
        "summaries":summaries,"source_anchors":anchors,
        "evidence_classes":available.iter().map(|e| json!({"id":e.id,"path":e.path,"runtime_allowed":source::is_implementation(&e.path)})).collect::<Vec<_>>(),
        "finding_kind_policy":crate::planning::FINDING_KIND_POLICY,
        "classification_policy":"Use evidence_classes from the first attempt, including previously_read anchors. XML/configuration declarations are context: describe what is declared, not whether it is loaded or executed. Runtime observations must cite supplied implementation. If a batch has no implementation, return context observations only. On repair, preserve valid observations and rewrite only invalid ones; never merely relabel an unsupported execution claim.","instruction":if children.is_empty(){READ}else{REDUCE}});
    if children.is_empty()
        && let Some(object) = input.as_object_mut()
    {
        object.remove("source_anchors");
    }
    if children.is_empty() {
        input["source_graph"] = crate::graph::context(ctx, &evidence, graph_limit(ctx)).await?;
        input["preservation_policy"] = json!(
            "This is an overview of preserved originals. Use graph symbols and branches to check important contracts; they carry names, not evidence IDs, so every finding still cites supplied passages. Every supplied evidence passage must be cited by at least one finding; do not silently leave a passage unread. The final document is independently checked against originals even when a fact does not fit this overview."
        );
    } else {
        // A reduction reads its children's summaries, not their source, so the
        // structure of the files those children read is the one thing it cannot
        // recover from the request. Without it every connection between two
        // children has to be written off as uncertain even where the graph
        // plainly holds the call. The hint is drawn over the same originals the
        // children kept, and its share of the request is already reserved.
        input["source_graph"] = crate::graph::context(ctx, &available, graph_limit(ctx)).await?;
    }
    if final_overview {
        input["evidence"] = json!([]);
        input["instruction"] = json!(
            "Read source evidence before planning through verified child findings. Synthesize ALL verified child findings into a balanced project overview. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[]}. Preserve the main product workflows, public entry points, processing, persisted/returned results, consumers and error/cancel paths across ALL children. Dependencies and test harness details together need at most two findings; do not let them displace product behavior. Use at most 12 findings, each under 1000 characters and with 1-6 source_anchors from its child findings, and at most 8 uncertainties. Previously-read originals are retained and validated by the caller; do not treat their omission from this synthesis request as an unknown project behavior. Carry only observations already in children and distinguish their runtime/context kinds. Do not add new facts or invent cross-module execution order; retain genuinely unresolved connections. Use the requested language."
        );
    }
    // LLM cache also includes model/endpoint/settings. Checkpoints must obey the
    // same contract when resuming with explicitly changed model settings.
    let config = &ctx.snapshot.settings.llm;
    let key = format!("understanding:node:{}", source::hash(serde_json::to_string(&json!({
        "version":7,"input":input,"children":children.iter().map(|n| &n.key).collect::<Vec<_>>(),"model":config.model,"endpoint":config.base_url,
        "reasoning":config.reasoning,"effort":config.effort,"output":config.max_output_tokens
    }))?.as_bytes()));
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let summary_cap = summary_limit(ctx);
    let mut error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        let mut validation_issues = vec![];
        let mut unverified_brief = None;
        let mut request = input.clone();
        request["attempt"] = json!(attempt);
        request["previous_error"] = json!(error);
        repair.apply(&mut request);
        let result = llm::call(ctx, system, request.clone()).await.and_then(|s| {
            let mut brief: SourceBrief = match repair.decode(&s) {
                Ok(brief) => brief,
                Err(error) if attempt == 2 => {
                    validation_issues.push(format!("{error:#}"));
                    return Ok(recover_output(&s, children, &available, summary_cap));
                }
                Err(error) => return Err(error),
            };
            let validation = validate_brief(&mut brief, &available, true).and_then(|_| {
                ensure!(
                    serde_json::to_vec(&brief)?.len() <= summary_cap,
                    "Source summary exceeds {summary_cap} bytes; compress observations"
                );
                if children.is_empty() {
                    ensure!(available.iter().all(|e| brief.findings.iter().any(|f|f.evidence_ids.contains(&e.id))),
                        "Source overview left supplied passages unaccounted for; include a supported observation for every evidence ID");
                }
                Ok(())
            });
            if let Err(error) = validation {
                if attempt < 2 {
                    return Err(error);
                }
                validation_issues.push(error.to_string());
                unverified_brief = Some(brief.clone());
                brief = salvage(brief, &available, summary_cap);
            }
            Ok(brief)
        });
        match result {
            Ok(brief) => {
                let files: Vec<_> = evidence
                    .iter()
                    .map(|e| e.path.clone())
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                let mut files = files;
                files.sort();
                let details = unverified_brief
                    .as_ref()
                    .map(|original| validated_findings(original, &available))
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|f| {
                        !brief.findings.iter().any(|kept| {
                            kept.topic == f.topic
                                && kept.observation == f.observation
                                && kept.evidence_ids == f.evidence_ids
                        })
                    })
                    .collect();
                let result = Node {
                    unresolved_nodes: children.iter().map(|n| n.unresolved_nodes).sum::<usize>()
                        + usize::from(!validation_issues.is_empty()),
                    unverified_output: if validation_issues.is_empty() {
                        None
                    } else {
                        repair.response.clone()
                    },
                    validation_issues,
                    unverified_brief,
                    key: key.clone(),
                    files,
                    children: children.iter().map(|n| n.key.clone()).collect(),
                    discovery: Discovery {
                        details,
                        validation_unresolved: false,
                        // Leaves are the authoritative source memory. A summary
                        // may not delete originals that it failed to mention.
                        evidence: if children.is_empty() {
                            available
                        } else {
                            available
                                .into_iter()
                                .filter(|e| {
                                    brief
                                        .findings
                                        .iter()
                                        .any(|f| f.evidence_ids.contains(&e.id))
                                })
                                .collect()
                        },
                        brief,
                    },
                };
                if !result.validation_issues.is_empty() {
                    ctx.event("source_validation_warning", json!({"node":key,"files":result.files,"issues":result.validation_issues,"retained_findings":result.discovery.brief.findings.len(),"title":"검증 미해결 항목을 보존하고 소스 읽기를 계속합니다"})).await?;
                }
                db::checkpoint(&ctx.pool, &ctx.id, &key, &serde_json::to_value(&result)?).await?;
                return Ok(result);
            }
            // A context-budget rejection happens before the request is sent and
            // the request is identical on every attempt. Hand it back so the
            // caller can shrink the request instead of burning the retries.
            Err(e) if fatal(&e) || is_budget(&e) || crate::runner::is_context_budget(&e) => {
                return Err(e);
            }
            Err(e) => {
                llm::forget(ctx, system, request).await?;
                error = editorial::excerpt(&format!("{e:#}"), 1500);
                ctx.event(
                    "source_validation",
                    json!({"attempt":attempt+1,"error":error}),
                )
                .await?;
            }
        }
    }
    anyhow::bail!("SOURCE_UNDERSTANDING: {error}")
}

fn reduction_work(mut nodes: usize) -> usize {
    let mut total = 0;
    while nodes > 1 {
        total += nodes / 4 + usize::from(nodes % 4 > 1);
        nodes = nodes.div_ceil(4);
    }
    total
}

/// Group one level's children so that a reduction's mandatory payload fits a
/// single request. Four keeps the tree shallow; narrower groups are used only
/// when the summaries and their anchors leave no room. Two is the floor, because
/// a group of one makes no progress towards a single root.
async fn reduction_groups(
    ctx: &RunContext,
    nodes: &[String],
    limit: usize,
) -> Result<Vec<Vec<String>>> {
    // Measure first and drop each child again; a level's nodes together hold
    // far more source than one request may carry.
    let mut costs = Vec::with_capacity(nodes.len());
    for key in nodes {
        let value = db::load_checkpoint(&ctx.pool, &ctx.id, key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Missing source analysis node"))?;
        let child: Node = serde_json::from_value(value)?;
        costs.push(mandatory_bytes(std::slice::from_ref(&child))?);
    }
    Ok(plan_groups(&costs, limit)?
        .into_iter()
        .map(|(start, end)| nodes[start..end].to_vec())
        .collect())
}

/// Half-open ranges over one level's children, at most four wide.
fn plan_groups(costs: &[usize], limit: usize) -> Result<Vec<(usize, usize)>> {
    let mut groups = vec![];
    let mut start = 0;
    while start < costs.len() {
        let mut end = start + 1;
        let mut used = costs[start];
        while end < costs.len() && end - start < 4 && used + costs[end] <= limit {
            used += costs[end];
            end += 1;
        }
        // A trailing single child passes through to the next level, but a
        // single child with work still behind it would never converge.
        ensure!(
            end - start > 1 || end == costs.len(),
            "CONTEXT_BUDGET: two source summaries no longer fit one request ({} of {limit} bytes); raise the context limit or lower the output limit in LLM settings",
            costs[start] + costs[end]
        );
        groups.push((start, end));
        start = end;
    }
    Ok(groups)
}

pub async fn analyze(ctx: &RunContext, system: &str) -> Result<Discovery> {
    if db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:version").await? == Some(json!(7))
        && let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:root").await?
    {
        return Ok(serde_json::from_value::<Node>(saved)?.discovery);
    }
    let limit = input_limit(ctx);
    ensure!(
        limit >= 1024,
        "CONTEXT_BUDGET: insufficient input space for whole-source reading"
    );
    let total_chunks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE run_id=?")
        .bind(&ctx.id)
        .fetch_one(&ctx.pool)
        .await?;
    ensure!(
        total_chunks > 0,
        "No source passages available for whole-source understanding"
    );
    ctx.event(
        "source_progress",
        json!({"stage":"understanding","title":"전체 소스 구현 읽기","read_batches":0,"read_chunks":0,"total_chunks":total_chunks}),
    )
    .await?;
    let mut after = 0u64;
    let mut source_chunks = Vec::<(u64, crate::graph::ChunkSpan)>::new();
    loop {
        ctx.check()?;
        let rows = sqlx::query("SELECT c.id,c.path,c.start_line,c.end_line FROM chunks c WHERE c.run_id=? AND c.id>? ORDER BY c.id LIMIT 64")
            .bind(&ctx.id).bind(after).fetch_all(&ctx.pool).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let id: u64 = row.try_get("id")?;
            after = id;
            source_chunks.push((
                id,
                crate::graph::ChunkSpan {
                    path: row.try_get("path")?,
                    start: row.try_get("start_line")?,
                    end: row.try_get("end_line")?,
                },
            ));
        }
    }
    let spans = source_chunks
        .iter()
        .map(|(_, span)| span.clone())
        .collect::<Vec<_>>();
    let order = crate::graph::order_chunks(ctx, &spans).await?;
    ctx.event(
        "source_order",
        json!({"stage":"understanding","title":"코드 그래프 기반 읽기 순서 구성","chunks":source_chunks.len(),"graph_ordered":true}),
    )
    .await?;
    let mut nodes = vec![];
    let mut group: Vec<Evidence> = vec![];
    let mut size = 0;
    let mut chunks_read = 0;
    for indices in order.chunks(64) {
        ctx.check()?;
        let limit = input_limit(ctx);
        // Forced margin can shrink the request mid-pass. Splitting source into
        // fragments to fit a window this small would read nothing useful.
        ensure!(
            limit >= 1024,
            "CONTEXT_BUDGET: insufficient input space for whole-source reading"
        );
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT c.id,c.path,c.start_line,c.end_line,COALESCE(b.content,c.content) content FROM chunks c LEFT JOIN chunk_blobs b ON b.hash=c.blob_hash WHERE c.run_id=",
        );
        query.push_bind(&ctx.id).push(" AND c.id IN (");
        for (position, index) in indices.iter().enumerate() {
            if position > 0 {
                query.push(", ");
            }
            query.push_bind(source_chunks[*index].0);
        }
        query.push(")");
        let rows = query.build().fetch_all(&ctx.pool).await?;
        let mut evidence_by_id = HashMap::with_capacity(rows.len());
        for row in rows {
            let id: u64 = row.try_get("id")?;
            evidence_by_id.insert(
                id,
                Evidence {
                    id: String::new(),
                    path: row.try_get("path")?,
                    start: row.try_get("start_line")?,
                    end: row.try_get("end_line")?,
                    content: row.try_get("content")?,
                },
            );
        }
        for index in indices {
            let id = source_chunks[*index].0;
            let e = evidence_by_id
                .remove(&id)
                .ok_or_else(|| anyhow::anyhow!("Missing source chunk {id}"))?;
            for part in segments(&e, limit.saturating_sub(e.path.len() + 512)) {
                let bytes = part.content.len() + part.path.len() + 256;
                if (size + bytes > limit || group.len() >= MAX_BATCH_PASSAGES) && !group.is_empty()
                {
                    nodes.push(
                        node(ctx, system, std::mem::take(&mut group), &[], false)
                            .await?
                            .key,
                    );
                    size = 0;
                    ctx.event("source_batch", json!({"stage":"understanding","title":"전체 소스 구현 읽기","read_batches":nodes.len(),"read_chunks":chunks_read,"total_chunks":total_chunks})).await?;
                }
                size += bytes;
                group.push(part);
            }
            chunks_read += 1;
        }
    }
    if !group.is_empty() {
        nodes.push(node(ctx, system, group, &[], false).await?.key);
        ctx.event("source_batch", json!({"stage":"understanding","title":"전체 소스 구현 읽기","read_batches":nodes.len(),"read_chunks":chunks_read,"total_chunks":total_chunks})).await?;
    }
    ensure!(
        !nodes.is_empty(),
        "No source passages available for whole-source understanding"
    );
    let leaves = nodes.clone();
    let batch_count = leaves.len();
    db::checkpoint(&ctx.pool, &ctx.id, "understanding:leaves", &json!(leaves)).await?;
    let mut total_summaries = reduction_work(batch_count);
    let mut completed_summaries = 0usize;
    if total_summaries > 0 {
        ctx.event("source_connections", json!({"stage":"understanding","title":"모듈 역할과 흐름 종합","level":1,"completed":0,"total":nodes.len().div_ceil(4),"level_completed":0,"level_total":nodes.len().div_ceil(4),"completed_summaries":0,"total_summaries":total_summaries})).await?;
    }
    let mut level = 0;
    while nodes.len() > 1 {
        level += 1;
        // Re-read the limit each level: the provider may have forced extra
        // margin on this run since the leaves were read.
        let limit = input_limit(ctx);
        let groups = reduction_groups(ctx, &nodes, limit).await?;
        let mut next = vec![];
        let level_total = groups.iter().filter(|children| children.len() > 1).count();
        // A narrower fan-out means more summaries than the four-way estimate.
        total_summaries = total_summaries.max(completed_summaries + level_total);
        let mut level_completed = 0usize;
        for children in &groups {
            if children.len() == 1 {
                next.push(children[0].clone());
                continue;
            }
            let mut loaded = vec![];
            for key in children {
                let value = db::load_checkpoint(&ctx.pool, &ctx.id, key)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Missing source analysis node"))?;
                loaded.push(serde_json::from_value::<Node>(value)?);
            }
            // The children's summaries and anchors are mandatory in this
            // request; originals are re-read only with what is left over. The
            // final synthesis is shown no originals at all, so packing them
            // would spend the budget on passages it never sees and would let it
            // cite an anchor it was never given.
            let final_overview = groups.len() == 1;
            let evidence = if final_overview {
                vec![]
            } else {
                pack_evidence(
                    &loaded
                        .iter()
                        .map(|n| n.discovery.evidence.clone())
                        .collect::<Vec<_>>(),
                    limit.saturating_sub(mandatory_bytes(&loaded)?),
                )
            };
            next.push(
                node(ctx, system, evidence, &loaded, final_overview)
                    .await?
                    .key,
            );
            level_completed += 1;
            completed_summaries += 1;
            ctx.event("source_connections", json!({"stage":"understanding","title":"모듈 역할과 흐름 종합","level":level,"completed":next.len(),"total":groups.len(),"level_completed":level_completed,"level_total":level_total,"completed_summaries":completed_summaries,"total_summaries":total_summaries})).await?;
        }
        nodes = next;
    }
    let key = nodes.remove(0);
    let root: Node = serde_json::from_value(
        db::load_checkpoint(&ctx.pool, &ctx.id, &key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Missing source overview"))?,
    )?;
    let files: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM files WHERE run_id=? AND status='indexed'")
            .bind(&ctx.id)
            .fetch_one(&ctx.pool)
            .await?;
    let coverage = json!({"complete":root.unresolved_nodes == 0,"reading_complete":true,"unresolved_nodes":root.unresolved_nodes,"read_files":files,"read_chunks":chunks_read,"read_batches":batch_count,"levels":level,"root":root.key});
    let mut tx = ctx.pool.begin().await?;
    for (step, value) in [
        ("understanding:root", serde_json::to_value(&root)?),
        ("understanding:coverage", coverage.clone()),
        ("understanding:version", json!(7)),
    ] {
        sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&ctx.id).bind(step).bind(value.to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    ctx.event(
        "source_understanding_complete",
        json!({"stage":"understood","title":if root.unresolved_nodes == 0 {"전체 소스 읽기와 흐름 종합 완료"} else {"전체 소스 읽기 완료 · 검증 미해결 항목 있음"},"coverage":coverage}),
    )
    .await?;
    Ok(root.discovery)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_response_cut_off_mid_observation_keeps_what_it_finished() -> Result<()> {
        let evidence = Evidence {
            id: source::hash(b"source"),
            path: "/project/main.rs".into(),
            start: 1,
            end: 1,
            content: "fn main() {}".into(),
        };
        // Two complete findings, then the provider stops inside the third
        // observation. A leaf batch is the only place these passages are ever
        // read, so what the response finished has to survive.
        let cut = format!(
            concat!(
                r#"{{"findings":[{{"topic":"entry","observation":"An empty main is defined","kind":"runtime","evidence_ids":["{id}"]}},"#,
                r#"{{"topic":"scope","observation":"The file declares nothing else","kind":"context","evidence_ids":["{id}"]}},"#,
                r#"{{"topic":"cut","observation":"The provider stopped here"#
            ),
            id = evidence.id
        );
        assert!(llm::decode::<serde_json::Value>(&cut).is_err());
        let recovered = recover_output(
            &cut,
            &[],
            std::slice::from_ref(&evidence),
            SUMMARY_MAX_BYTES,
        );
        assert_eq!(
            recovered.findings.len(),
            2,
            "{:?}",
            recovered.findings.len()
        );
        assert_eq!(recovered.findings[0].topic, "entry");
        assert_eq!(recovered.findings[1].topic, "scope");
        Ok(())
    }

    #[test]
    fn malformed_output_remains_unresolved_and_valid_siblings_survive_bad_schema() {
        let evidence = Evidence {
            id: source::hash(b"source"),
            path: "/project/main.rs".into(),
            start: 1,
            end: 1,
            content: "fn main() {}".into(),
        };
        let malformed = recover_output(
            "{not JSON",
            &[],
            std::slice::from_ref(&evidence),
            SUMMARY_MAX_BYTES,
        );
        assert!(malformed.findings.is_empty());
        assert!(!malformed.uncertainties.is_empty());
        let output = json!({"findings":[
            {"topic":"main","observation":"An empty main is defined","kind":"runtime","evidence_ids":[evidence.id]},
            {"topic":"bad type","observation":42,"kind":"runtime","evidence_ids":null}
        ],"uncertainties":"wrong type"}).to_string();
        let recovered = recover_output(&output, &[], &[evidence], SUMMARY_MAX_BYTES);
        assert_eq!(recovered.findings.len(), 1);
        assert_eq!(recovered.findings[0].topic, "main");
        assert!(!recovered.uncertainties.is_empty());
    }

    #[test]
    fn failed_reduction_preserves_verified_child_findings_and_warning_on_resume() -> Result<()> {
        let evidence = Evidence {
            id: source::hash(b"main"),
            path: "/project/main.rs".into(),
            start: 1,
            end: 1,
            content: "fn main() {}".into(),
        };
        let brief: SourceBrief = serde_json::from_value(json!({"findings":[
            {"topic":"main","observation":"An empty main is defined","kind":"runtime","evidence_ids":[evidence.id]}
        ],"uncertainties":[],"followup_queries":[]}))?;
        let child = Node {
            key: "child".into(),
            files: vec![evidence.path.clone()],
            children: vec![],
            discovery: Discovery {
                details: vec![],
                validation_unresolved: false,
                brief,
                evidence: vec![evidence.clone()],
            },
            unresolved_nodes: 1,
            validation_issues: vec!["JSON error".into()],
            unverified_brief: None,
            unverified_output: Some("broken".into()),
        };
        let saved = serde_json::to_value(&child)?;
        let restored: Node = serde_json::from_value(saved)?;
        assert_eq!(restored.unresolved_nodes, 1);
        assert_eq!(restored.unverified_output.as_deref(), Some("broken"));
        let recovered = recover_output(
            "broken reduction",
            &[restored],
            &[evidence],
            SUMMARY_MAX_BYTES,
        );
        assert_eq!(recovered.findings.len(), 1);
        assert_eq!(recovered.findings[0].topic, "main");
        assert!(!recovered.uncertainties.is_empty());
        Ok(())
    }

    #[test]
    fn salvage_keeps_valid_observations_without_relabeling_xml_runtime() -> Result<()> {
        let xml = Evidence {
            id: source::hash(b"xml"),
            path: "/project/mapper.xml".into(),
            start: 1,
            end: 1,
            content: "<select id=\"load\">SELECT 1</select>".into(),
        };
        let code = Evidence {
            id: source::hash(b"code"),
            path: "/project/main.rs".into(),
            start: 1,
            end: 1,
            content: "fn main() {}".into(),
        };
        let brief: SourceBrief = serde_json::from_value(json!({
            "findings":[
                {"topic":"unsupported execution","observation":"The mapper executes on startup", "kind":"runtime","evidence_ids":[xml.id]},
                {"topic":"declaration","observation":"XML declares a select statement", "kind":"context","evidence_ids":[xml.id]},
                {"topic":"entry","observation":"An empty main is defined", "kind":"runtime","evidence_ids":[&code.id[..8]]},
                {"topic":"unknown anchor","observation":"Unverified", "kind":"context","evidence_ids":["ffffffff"]}
            ],"uncertainties":[],"followup_queries":[]
        }))?;
        let recovered = salvage(brief, &[xml, code.clone()], SUMMARY_MAX_BYTES);
        assert_eq!(recovered.findings.len(), 2);
        assert_eq!(recovered.findings[0].topic, "declaration");
        assert_eq!(recovered.findings[1].evidence_ids, vec![code.id]);
        assert!(!recovered.uncertainties.is_empty());
        Ok(())
    }

    #[test]
    fn salvage_does_not_invent_findings_when_all_evidence_is_invalid() -> Result<()> {
        let brief: SourceBrief = serde_json::from_value(json!({
            "findings":[{"topic":"invalid","observation":"Unsupported execution", "kind":"runtime","evidence_ids":["ffffffff"]}],
            "uncertainties":[],"followup_queries":[]
        }))?;
        let recovered = salvage(brief, &[], SUMMARY_MAX_BYTES);
        assert!(recovered.findings.is_empty());
        assert_eq!(recovered.uncertainties.len(), 1);
        Ok(())
    }

    /// A child summarised right up to its cap, with the originals it must keep
    /// anchoring, is what a reduction has to carry before it reads anything new.
    fn saturated_child(seed: u8, maximum: usize, originals: usize) -> Node {
        let evidence: Vec<Evidence> = (0..originals)
            .map(|i| Evidence {
                id: source::hash(&[seed, i as u8]),
                path: format!("/Users/someone/workspace/project/backend/src/module{i}.rs"),
                start: 1,
                end: 200,
                content: String::new(),
            })
            .collect();
        let mut brief = SourceBrief {
            findings: vec![],
            uncertainties: vec![],
            followup_queries: vec![],
        };
        while serde_json::to_vec(&brief).is_ok_and(|v| v.len() <= maximum) {
            brief.findings.push(crate::planning::Finding {
                topic: "관측".into(),
                observation: "가".repeat(900),
                kind: crate::planning::FindingKind::Context,
                evidence_ids: evidence.iter().take(6).map(|e| e.id.clone()).collect(),
            });
        }
        brief.findings.pop();
        assert!(serde_json::to_vec(&brief).is_ok_and(|v| v.len() <= maximum));
        Node {
            key: format!("understanding:node:{seed}"),
            files: vec![],
            children: vec![],
            unresolved_nodes: 0,
            validation_issues: vec![],
            unverified_brief: None,
            unverified_output: None,
            discovery: Discovery {
                brief,
                evidence,
                details: vec![],
                validation_unresolved: false,
            },
        }
    }

    #[test]
    fn a_request_packed_to_the_limit_survives_the_budget_gate() {
        for output in [4_096u32, 16_384, 32_000, 64_000] {
            for safety in [5u32, 20, 40] {
                for extra in [0u32, 5, 10] {
                    let c = LlmConfig {
                        max_output_tokens: output,
                        model_max_output: 150_000,
                        safety_percent: safety,
                        ..Default::default()
                    };
                    let space = request_space(&c, extra);
                    let limit = request_limit(&c, extra);
                    assert_eq!(limit + graph_budget(space), space);
                    // What the gate will actually weigh: the packed source and
                    // its structural hint and the fixed request scaffolding,
                    // all of it through both JSON escapes.
                    let serialized = (space + REQUEST_OVERHEAD_BYTES)
                        * crate::budget::ESCAPE_EXPANSION_PERCENT as usize
                        / 100;
                    assert!(
                        crate::budget::check(&c, serialized as u64, extra).is_ok(),
                        "packing {limit} bytes at output {output}, safety {safety}+{extra} builds a request the gate rejects"
                    );
                }
            }
        }
    }

    #[test]
    fn a_leaf_batch_can_always_be_cited_by_one_brief() {
        // A leaf must cite every passage it was given. Filling a batch to the
        // citation ceiling would require twelve findings of six distinct
        // passages each, with no passage supporting two observations, so the
        // batch has to stop well short of it.
        let ceiling = crate::planning::MAX_FINDINGS * crate::planning::MAX_EVIDENCE_IDS;
        assert!(MAX_BATCH_PASSAGES < ceiling);
        let findings_needed = MAX_BATCH_PASSAGES.div_ceil(crate::planning::MAX_EVIDENCE_IDS);
        assert!(
            findings_needed <= crate::planning::MAX_FINDINGS / 2,
            "a full batch already needs {findings_needed} of {} findings just to name its passages",
            crate::planning::MAX_FINDINGS
        );
    }

    #[test]
    fn a_modest_context_window_still_leaves_room_to_read_source() {
        // A fixed scaffolding reserve larger than the window itself would refuse
        // every batch outright, so the hint has to scale instead.
        for context in [32_000u32, 64_000, 128_000] {
            let c = LlmConfig {
                context_limit: context,
                model_context_limit: context,
                max_output_tokens: 4_096,
                ..Default::default()
            };
            let limit = request_limit(&c, 0);
            assert!(
                limit >= 1024,
                "a {context}-token window leaves only {limit} bytes for source"
            );
            assert!(graph_budget(request_space(&c, 0)) < limit);
        }
    }

    #[test]
    fn a_reduction_reads_originals_only_with_what_its_summaries_leave() -> Result<()> {
        let c = LlmConfig {
            max_output_tokens: 32_000,
            ..Default::default()
        };
        let limit = request_limit(&c, 0);
        let maximum = summary_maximum(limit);
        let children: Vec<Node> = (0..4)
            .map(|seed| saturated_child(seed, maximum, 12))
            .collect();
        let mandatory = mandatory_bytes(&children)?;
        let room = limit.saturating_sub(mandatory);
        assert!(
            room >= limit / 8,
            "four saturated summaries ({mandatory} bytes) leave only {room} of {limit} for originals"
        );
        let fits = (mandatory + room + REQUEST_OVERHEAD_BYTES)
            * crate::budget::ESCAPE_EXPANSION_PERCENT as usize
            / 100;
        assert!(crate::budget::check(&c, fits as u64, 0).is_ok());
        // Spending the whole limit on originals, as the fan-out did before the
        // summaries were measured, is exactly what the gate rejected.
        let ignoring_summaries = (mandatory + limit + REQUEST_OVERHEAD_BYTES)
            * crate::budget::ESCAPE_EXPANSION_PERCENT as usize
            / 100;
        assert!(crate::budget::check(&c, ignoring_summaries as u64, 0).is_err());
        Ok(())
    }

    #[test]
    fn reduction_narrows_the_fan_out_instead_of_overfilling_a_request() -> Result<()> {
        assert_eq!(plan_groups(&[10, 10, 10, 10, 10], 100)?, [(0, 4), (4, 5)]);
        // Wide summaries fall back to pairs rather than four at a time.
        assert_eq!(plan_groups(&[60, 60, 60, 60], 150)?, [(0, 2), (2, 4)]);
        // A lone trailing child is carried to the next level untouched.
        assert_eq!(plan_groups(&[60, 60, 60], 150)?, [(0, 2), (2, 3)]);
        // Two that cannot share a request would never converge to one root.
        assert!(
            plan_groups(&[200, 200, 200], 150)
                .is_err_and(|e| e.to_string().contains("CONTEXT_BUDGET"))
        );
        Ok(())
    }

    #[test]
    fn reduction_progress_counts_only_summaries_that_are_created() {
        assert_eq!(reduction_work(1), 0);
        assert_eq!(reduction_work(2), 1);
        assert_eq!(reduction_work(5), 2);
        assert_eq!(reduction_work(16), 5);
        assert_eq!(reduction_work(900), 300);
    }

    #[test]
    fn exhaustive_segments_preserve_unicode_and_long_lines() {
        let e = Evidence {
            id: String::new(),
            path: "src/a.rs".into(),
            start: 7,
            end: 9,
            content: format!("{}\nsecond\n", "가🦀".repeat(3000)),
        };
        let parts = segments(&e, 1000);
        assert_eq!(
            parts.iter().map(|p| p.content.as_str()).collect::<String>(),
            e.content
        );
        assert!(
            parts
                .iter()
                .all(|p| p.content.len() <= 1000 && p.start >= 7 && p.end >= p.start)
        );
        assert_eq!(parts.last().map(|p| p.end), Some(8));
    }
}
