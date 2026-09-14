//! Exhaustive, resumable source reading. Request size limits split work; they
//! never truncate the file inventory or mark unread input as understood.
use crate::{
    db, editorial, llm,
    model::Evidence,
    planning::{Discovery, SourceBrief, pack_evidence, validate_brief},
    runner::{RunContext, fatal, is_budget},
    source,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use std::collections::HashSet;

const READ: &str = "Read source evidence before planning, independently of any future documentation purpose. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[]}. Read ALL supplied passages. Explain module responsibilities, entry points, inputs, conditions, decisions, state/data changes, outputs, consumers and errors/cancellation/lifecycle. Preserve distinct public workflows, important branches, and producer/consumer contracts. Imports and call names are navigation candidates, not proof of execution. Mark unresolved connections. Runtime observations require implementation passages; tests/docs describe context only. Use at most 12 findings, observations under 1000 characters, at most 6 evidence IDs each, and 8 uncertainties. Do not design a table of contents or force unrelated flows into one sequence. Use the requested language.";
const REDUCE: &str = "Read source evidence before planning. Integrate ALL supplied child summaries into a higher-level source overview. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[]}. Preserve distinct workflows, module contracts, state changes, result consumers, conditional/error/cancel branches and unresolved cross-module links. Child findings have already been checked against their original passages. Preserve their important workflows and source anchors even when those passages are not repeated in this bounded request. previously_read anchors identify those originals; they support carrying the child observation, not inventing new facts. Use newly supplied original evidence to establish NEW connections; mark any other cross-module synthesis as uncertain. Never invent an execution order to join independent workflows. Do not assume a missing excerpt is absent from the project. Include at most 12 findings, each observation under 1000 characters and with 1-6 supplied evidence IDs, and at most 8 uncertainties. Do not produce a document outline. Use the requested language.";

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub key: String,
    pub files: Vec<String>,
    pub children: Vec<String>,
    pub discovery: Discovery,
}

fn input_limit(ctx: &RunContext) -> usize {
    let l = &ctx.snapshot.settings.llm;
    ((l.context_limit.min(l.model_context_limit).min(200_000) as usize)
        .saturating_mul(100usize.saturating_sub(l.safety_percent as usize))
        / 100)
        .saturating_sub(l.max_output_tokens as usize + 20_000)
        .min(36_000)
}

/// Every byte belongs to one segment, including long lines and Unicode.
fn segments(e: &Evidence, maximum: usize) -> Vec<Evidence> {
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
        "summaries":summaries,"source_anchors":anchors,"instruction":if children.is_empty(){READ}else{REDUCE}});
    if children.is_empty()
        && let Some(object) = input.as_object_mut()
    {
        object.remove("source_anchors");
    }
    if final_overview {
        input["evidence"] = json!([]);
        input["instruction"] = json!(
            "Read source evidence before planning through verified child findings. Synthesize ALL verified child findings into a balanced project overview. Return ONLY JSON {findings:[{topic:string,observation:string,kind:runtime|context,evidence_ids:[string]}],uncertainties:[string],followup_queries:[]}. Preserve the main product workflows, public entry points, processing, persisted/returned results, consumers and error/cancel paths across ALL children. Dependencies and test harness details together need at most two findings; do not let them displace product behavior. Use at most 12 findings, each under 1000 characters and with 1-6 source_anchors from its child findings, and at most 8 uncertainties. Previously-read originals are retained and validated by the caller; do not treat their omission from this synthesis request as an unknown project behavior. Carry only observations already in children and distinguish their runtime/context kinds. Do not add new facts or invent cross-module execution order; retain genuinely unresolved connections. Use the requested language."
        );
    }
    // LLM cache also includes model/endpoint/settings. Checkpoints must obey the
    // same contract when resuming with explicitly changed model settings.
    let config = &ctx.snapshot.settings.llm;
    let key = format!("understanding:node:{}", source::hash(serde_json::to_string(&json!({
        "version":if final_overview{3}else if children.is_empty(){1}else{2},"input":input,"model":config.model,"endpoint":config.base_url,
        "reasoning":config.reasoning,"effort":config.effort,"output":config.max_output_tokens
    }))?.as_bytes()));
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let mut error = String::new();
    for attempt in 0..3 {
        let mut request = input.clone();
        request["attempt"] = json!(attempt);
        request["previous_error"] = json!(error);
        if attempt > 0 {
            request["evidence_classes"] = json!(evidence.iter().map(|e| json!({"path":e.path,"class":if source::is_implementation(&e.path){"implementation"}else{"context only"}})).collect::<Vec<_>>());
        }
        let result = llm::call(ctx, system, request.clone()).await.and_then(|s| {
            let mut brief: SourceBrief = llm::decode(&s)?;
            validate_brief(&mut brief, &available, true)?;
            ensure!(
                serde_json::to_vec(&brief)?.len() <= 18_000,
                "Source summary exceeds 18000 bytes; compress observations"
            );
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
                let result = Node {
                    key: key.clone(),
                    files,
                    children: children.iter().map(|n| n.key.clone()).collect(),
                    discovery: Discovery {
                        evidence: available
                            .into_iter()
                            .filter(|e| {
                                brief
                                    .findings
                                    .iter()
                                    .any(|f| f.evidence_ids.contains(&e.id))
                            })
                            .collect(),
                        brief,
                    },
                };
                db::checkpoint(&ctx.pool, &ctx.id, &key, &serde_json::to_value(&result)?).await?;
                return Ok(result);
            }
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, request).await?;
                error = editorial::excerpt(&e.to_string(), 1500);
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

pub async fn analyze(ctx: &RunContext, system: &str) -> Result<Discovery> {
    if db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:version").await? == Some(json!(3))
        && let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:root").await?
    {
        return Ok(serde_json::from_value::<Node>(saved)?.discovery);
    }
    let limit = input_limit(ctx);
    ensure!(
        limit >= 1024,
        "CONTEXT_BUDGET: insufficient input space for whole-source reading"
    );
    let mut after = 0u64;
    let mut nodes = vec![];
    let mut group: Vec<Evidence> = vec![];
    let mut size = 0;
    let mut chunks_read = 0;
    loop {
        ctx.check()?;
        let rows = sqlx::query("SELECT c.id,c.path,c.start_line,c.end_line,COALESCE(b.content,c.content) content FROM chunks c LEFT JOIN chunk_blobs b ON b.hash=c.blob_hash WHERE c.run_id=? AND c.id>? ORDER BY c.id LIMIT 32")
            .bind(&ctx.id).bind(after).fetch_all(&ctx.pool).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            after = row.try_get("id")?;
            let e = Evidence {
                id: String::new(),
                path: row.try_get("path")?,
                start: row.try_get("start_line")?,
                end: row.try_get("end_line")?,
                content: row.try_get("content")?,
            };
            for part in segments(&e, limit.saturating_sub(e.path.len() + 512)) {
                let bytes = part.content.len() + part.path.len() + 256;
                if size + bytes > limit && !group.is_empty() {
                    nodes.push(
                        node(ctx, system, std::mem::take(&mut group), &[], false)
                            .await?
                            .key,
                    );
                    size = 0;
                    ctx.event("source_batch", json!({"stage":"understanding","title":"전체 소스 구현 읽기","read_batches":nodes.len(),"read_chunks":chunks_read})).await?;
                }
                size += bytes;
                group.push(part);
            }
            chunks_read += 1;
        }
    }
    if !group.is_empty() {
        nodes.push(node(ctx, system, group, &[], false).await?.key);
    }
    ensure!(
        !nodes.is_empty(),
        "No source passages available for whole-source understanding"
    );
    let leaves = nodes.clone();
    let batch_count = leaves.len();
    db::checkpoint(&ctx.pool, &ctx.id, "understanding:leaves", &json!(leaves)).await?;
    let mut level = 0;
    while nodes.len() > 1 {
        level += 1;
        let mut next = vec![];
        for children in nodes.chunks(4) {
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
            let evidence = pack_evidence(
                &loaded
                    .iter()
                    .map(|n| n.discovery.evidence.clone())
                    .collect::<Vec<_>>(),
                limit,
            );
            next.push(
                node(ctx, system, evidence, &loaded, nodes.len() <= 4)
                    .await?
                    .key,
            );
            ctx.event("source_connections", json!({"stage":"understanding","title":"모듈 역할과 흐름 종합","level":level,"completed":next.len(),"total":nodes.len().div_ceil(4)})).await?;
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
    let coverage = json!({"complete":true,"read_files":files,"read_chunks":chunks_read,"read_batches":batch_count,"levels":level,"root":root.key});
    let mut tx = ctx.pool.begin().await?;
    for (step, value) in [
        ("understanding:root", serde_json::to_value(&root)?),
        ("understanding:coverage", coverage.clone()),
        ("understanding:version", json!(3)),
    ] {
        sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&ctx.id).bind(step).bind(value.to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    ctx.event(
        "source_understanding_complete",
        json!({"stage":"understood","title":"전체 소스 읽기와 흐름 종합 완료","coverage":coverage}),
    )
    .await?;
    Ok(root.discovery)
}

#[cfg(test)]
mod tests {
    use super::*;
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
