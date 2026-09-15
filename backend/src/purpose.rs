//! One purpose-focused source summary over reusable general reading.
//! Preserve detailed observations and originals without inventing a question list.
use crate::{
    db, editorial, llm,
    model::{Evidence, TaskConfig},
    planning::{Discovery, Finding, SourceBrief, evidence_budget, pack_evidence, validate_brief},
    runner::{RunContext, fatal, is_budget},
    source,
    understanding::Node,
};
use anyhow::Result;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::{HashMap, HashSet};

const READ: &str = "Read source evidence before planning the document. Summarize the code according to purpose, grouping related responsibilities and actual workflows. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[string]}. Use 1-12 findings with short topics, observations under 1000 characters and 1-6 supplied evidence IDs. Preserve important entry points, processing, conditions, state changes, outputs, consumers and error/cancellation paths. Adapt the emphasis to the user's requested audience and scope. Do not generate a list of reader questions or a table of contents. verified_overview and supporting_findings contain prior observations checked against their original passages. Carry relevant observations using previously_read anchors; use original passages supplied in this request for new claims or cross-module connections. Runtime findings require implementation evidence; XML/configuration/docs/tests establish context, not execution. Do not infer call order from filenames/imports or treat separate alternatives as consecutive steps. Do not infer absent implementation from an omitted excerpt. Record genuinely unresolved links in uncertainties (at most 8). Request at most 3 focused followup_queries using observed paths/symbols ONLY when an important part of the requested flow needs more source evidence. Avoid extra investigation of minor helper details. On the final pass return followup_queries:[] and keep remaining gaps in uncertainties. When correcting an earlier finding reuse its exact topic. Include all three arrays, even when empty. Use the requested language.";

pub(crate) fn intent_key(task: &TaskConfig) -> String {
    source::hash(
        json!({"version":1,"purpose":task.direction,"language":task.language})
            .to_string()
            .as_bytes(),
    )
}

/// A running snapshot normally keeps its direction fixed. Also guard any future
/// direction-edit path against stale plans/drafts, without touching general reading.
pub(crate) async fn prepare(ctx: &RunContext) -> Result<()> {
    let scope = json!(intent_key(&ctx.snapshot.task));
    let previous = db::load_checkpoint(&ctx.pool, &ctx.id, "document_scope").await?;
    if previous.as_ref().is_some_and(|p| p != &scope) {
        let mut tx = ctx.pool.begin().await?;
        sqlx::query("DELETE FROM checkpoints WHERE run_id=? AND (step IN ('source_understanding','source_understanding_scope','document_requirements','document_validation_version','review_start_iteration') OR step LIKE 'outline%' OR step LIKE 'section:%' OR step LIKE 'section_output:%' OR step LIKE 'review:%' OR step LIKE 'repair:%')")
            .bind(&ctx.id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,'document_scope',?) ON DUPLICATE KEY UPDATE data=VALUES(data)")
            .bind(&ctx.id).bind(scope.to_string()).execute(&mut *tx).await?;
        tx.commit().await?;
    } else {
        db::checkpoint(&ctx.pool, &ctx.id, "document_scope", &scope).await?;
    }
    Ok(())
}

pub(crate) fn merge_evidence(groups: &[Vec<Evidence>]) -> Vec<Evidence> {
    let mut seen = HashSet::new();
    groups
        .iter()
        .flatten()
        .filter(|e| seen.insert(e.id.clone()))
        .cloned()
        .collect()
}

fn merge_findings(groups: &[Vec<Finding>]) -> Vec<Finding> {
    let mut seen = HashSet::new();
    groups
        .iter()
        .flatten()
        .filter(|f| {
            seen.insert((
                f.topic.clone(),
                f.observation.clone(),
                f.evidence_ids.clone(),
            ))
        })
        .cloned()
        .collect()
}

#[derive(Clone)]
struct Detail {
    score: usize,
    finding: Finding,
    evidence: Vec<Evidence>,
}

fn select_details(best: &mut Vec<Detail>, discovery: &Discovery, terms: &[String]) {
    for finding in discovery.brief.findings.iter().chain(&discovery.details) {
        let evidence: Vec<_> = discovery
            .evidence
            .iter()
            .filter(|e| finding.evidence_ids.contains(&e.id))
            .cloned()
            .collect();
        if evidence.len() != finding.evidence_ids.len() {
            continue;
        }
        let text = format!(
            "{} {} {}",
            finding.topic,
            finding.observation,
            evidence
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        )
        .to_lowercase();
        let score = terms.iter().filter(|t| text.contains(t.as_str())).count();
        best.push(Detail {
            score,
            finding: finding.clone(),
            evidence,
        });
    }
    best.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.finding.evidence_ids.cmp(&b.finding.evidence_ids))
    });
    let mut seen = HashSet::new();
    let mut per_file = HashMap::new();
    best.retain(|d| {
        let count = per_file
            .entry(
                d.evidence
                    .first()
                    .map(|e| e.path.clone())
                    .unwrap_or_default(),
            )
            .or_insert(0);
        if *count >= 4 || !seen.insert((d.finding.topic.clone(), d.finding.evidence_ids.clone())) {
            return false;
        }
        *count += 1;
        true
    });
    best.truncate(24);
}

async fn seed(ctx: &RunContext, whole: &Discovery) -> Result<Discovery> {
    let leaves: HashSet<String> = db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:leaves")
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let terms = source::search_terms(&ctx.snapshot.task.direction);
    let mut best = vec![];
    let mut cursor = String::new();
    loop {
        ctx.check()?;
        let rows = sqlx::query("SELECT step,data FROM checkpoints WHERE run_id=? AND step LIKE 'understanding:node:%' AND step>? ORDER BY step LIMIT 16")
            .bind(&ctx.id).bind(&cursor).fetch_all(&ctx.pool).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            cursor = row.try_get("step")?;
            if !leaves.is_empty() && !leaves.contains(&cursor) {
                continue;
            }
            let node: Node = serde_json::from_str(&row.try_get::<String, _>("data")?)?;
            if node.children.is_empty() {
                select_details(&mut best, &node.discovery, &terms);
            }
        }
    }
    let mut groups = vec![whole.evidence.clone()];
    groups.extend(best.iter().map(|d| d.evidence.clone()));
    Ok(Discovery {
        brief: whole.brief.clone(),
        details: best.into_iter().map(|d| d.finding).collect(),
        evidence: merge_evidence(&groups),
        validation_unresolved: false,
    })
}

fn recover(prior: &Discovery) -> Discovery {
    let mut recovered = prior.clone();
    recovered
        .brief
        .uncertainties
        .extend(std::mem::take(&mut recovered.brief.followup_queries));
    recovered.brief.uncertainties.truncate(7);
    recovered.brief.uncertainties.push("목적별 추가 요약을 검증하지 못했습니다. 보존한 소스 관찰로 진행하며 미확인 연결은 본문 작성에서 다시 확인합니다.".into());
    recovered.validation_unresolved = true;
    recovered
}

fn retain_details(prior: &Discovery, brief: SourceBrief, evidence: Vec<Evidence>) -> Discovery {
    let replaced: HashSet<_> = brief.findings.iter().map(|f| f.topic.as_str()).collect();
    let details = merge_findings(&[prior.brief.findings.clone(), prior.details.clone()])
        .into_iter()
        .filter(|f| !replaced.contains(f.topic.as_str()))
        .collect();
    Discovery {
        brief,
        details,
        evidence,
        validation_unresolved: prior.validation_unresolved,
    }
}

async fn read_sources(
    ctx: &RunContext,
    system: &str,
    prior: &Discovery,
    queries: &[String],
    final_pass: bool,
    key: &str,
) -> Result<Discovery> {
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, key).await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let mut limit = evidence_budget(ctx).min(32_000);
    let mut error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        ctx.check()?;
        if limit < 1024 {
            error = "CONTEXT_BUDGET: insufficient room for source summary".into();
            break;
        }
        ctx.event("source_reading",json!({"stage":"understanding","title":"요청 방향에 맞춰 코드 흐름 요약","pass":if final_pass {2}else{1},"attempt":attempt+1})).await?;
        let mut groups = vec![pack(prior, limit / 2)];
        for query in queries.iter().take(3) {
            groups.push(
                source::retrieve(ctx, query, (limit / 2 / queries.len().clamp(1, 3)).max(512))
                    .await?,
            );
        }
        let evidence = pack_evidence(&groups, limit);
        let available = merge_evidence(&[evidence.clone(), prior.evidence.clone()]);
        let mut input = json!({"phase":"purpose_reading","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "verified_overview":prior.brief,"supporting_findings":context(prior),
            "source_anchors":prior.evidence.iter().map(|e|json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),
            "evidence":evidence,"evidence_classes":available.iter().map(|e|json!({"id":e.id,"path":e.path,"runtime_allowed":source::is_implementation(&e.path)})).collect::<Vec<_>>(),
            "open_questions":prior.brief.uncertainties,"final_pass":final_pass,"attempt":attempt+1,"previous_error":error,
            "finding_kind_policy":crate::planning::FINDING_KIND_POLICY,"instruction":READ});
        repair.apply(&mut input);
        let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let mut brief: SourceBrief = repair.decode(&s)?;
            validate_brief(&mut brief, &available, final_pass)?;
            Ok(brief)
        });
        match result {
            Ok(brief) => {
                let read = retain_details(prior, brief, available);
                db::checkpoint(&ctx.pool, &ctx.id, key, &serde_json::to_value(&read)?).await?;
                return Ok(read);
            }
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                error = editorial::excerpt(&format!("{e:#}"), 1500);
                if error.contains("CONTEXT_BUDGET") {
                    limit /= 2;
                }
                ctx.event(
                    "source_reading_retry",
                    json!({"stage":"understanding","attempt":attempt+1,"error":error}),
                )
                .await?;
            }
        }
    }
    let recovered = recover(prior);
    db::checkpoint(
        &ctx.pool,
        &ctx.id,
        &format!("{key}:unresolved"),
        &json!({"error":error,"response":repair.response}),
    )
    .await?;
    db::checkpoint(&ctx.pool, &ctx.id, key, &serde_json::to_value(&recovered)?).await?;
    ctx.event(
        "source_validation_warning",
        json!({"title":"추가 요약 미해결 · 보존한 소스 관찰로 계속합니다","error":error}),
    )
    .await?;
    Ok(recovered)
}

pub(crate) async fn analyze(
    ctx: &RunContext,
    system: &str,
    whole: &Discovery,
) -> Result<Discovery> {
    let config = &ctx.snapshot.settings.llm;
    let scope = source::hash(json!({"version":2,"intent":intent_key(&ctx.snapshot.task),
        "source":db::load_checkpoint(&ctx.pool,&ctx.id,"index_fingerprint").await?,
        "root":db::load_checkpoint(&ctx.pool,&ctx.id,"understanding:coverage").await?.and_then(|v|v.get("root").cloned()),
        "model":config.model,"endpoint":config.base_url,"output":config.max_output_tokens,
        "context":config.context_limit,"model_context":config.model_context_limit,"safety":config.safety_percent,
        "reasoning":config.reasoning,"effort":config.effort,"system":system,"instruction":READ
    }).to_string().as_bytes());
    if db::load_checkpoint(&ctx.pool, &ctx.id, "source_understanding_scope").await?
        == Some(json!(scope))
        && let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "source_understanding").await?
    {
        return Ok(serde_json::from_value(saved)?);
    }
    let seed = seed(ctx, whole).await?;
    let key = format!("purpose:{scope}");
    let first = read_sources(
        ctx,
        system,
        &seed,
        std::slice::from_ref(&ctx.snapshot.task.direction),
        false,
        &format!("{key}:first"),
    )
    .await?;
    let complete = if first.validation_unresolved || first.brief.followup_queries.is_empty() {
        first
    } else {
        read_sources(
            ctx,
            system,
            &first,
            &first.brief.followup_queries,
            true,
            &format!("{key}:final"),
        )
        .await?
    };
    let mut tx = ctx.pool.begin().await?;
    for (key, value) in [
        ("source_understanding_scope", json!(scope)),
        ("source_understanding", serde_json::to_value(&complete)?),
    ] {
        sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)")
            .bind(&ctx.id).bind(key).bind(value.to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    ctx.event("source_understood",json!({"stage":"understood","findings":complete.brief.findings.len(),"supporting_findings":complete.details.len(),
        "uncertainties":complete.brief.uncertainties,"validation_unresolved":complete.validation_unresolved,"selective_analysis":true})).await?;
    Ok(complete)
}

/// Give different files a turn without removing any originals from storage.
pub(crate) fn pack(discovery: &Discovery, limit: usize) -> Vec<Evidence> {
    let mut paths = HashMap::new();
    let mut groups: Vec<Vec<Evidence>> = vec![];
    for e in &discovery.evidence {
        let index = *paths.entry(&e.path).or_insert_with(|| {
            groups.push(vec![]);
            groups.len() - 1
        });
        groups[index].push(e.clone());
    }
    pack_evidence(&groups, limit)
}

/// Bounded view only. Keep full detailed observations and their originals stored.
pub(crate) fn context(discovery: &Discovery) -> Vec<Value> {
    let mut result = vec![];
    let per = 20_000 / discovery.details.len().max(1);
    for f in &discovery.details {
        let mut view = json!(f);
        let overhead = view.to_string().len().saturating_sub(f.observation.len()) + 40;
        let limit = per.saturating_sub(overhead);
        view["observation"] = json!(editorial::excerpt(&f.observation, limit));
        view["excerpted"] = json!(f.observation.len() > limit);
        result.push(view);
    }
    result
}

/// Rehydrate relevant leaf observations, including those absent from the root
/// overview and purpose's selected 24 details. The budget limits this VIEW only.
pub(crate) async fn section_memory(
    ctx: &RunContext,
    evidence: &[Evidence],
    limit: usize,
) -> Result<Value> {
    let leaves: Vec<String> = db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:leaves")
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let (mut used, mut deferred) = (0usize, 0usize);
    let mut findings = vec![];
    let mut seen = HashSet::new();
    for key in leaves {
        ctx.check()?;
        let Some(value) = db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? else {
            continue;
        };
        let node: Node = serde_json::from_value(value)?;
        for finding in node
            .discovery
            .brief
            .findings
            .iter()
            .chain(&node.discovery.details)
        {
            let matches = node.discovery.evidence.iter().any(|original| {
                finding.evidence_ids.contains(&original.id)
                    && evidence.iter().any(|e| {
                        e.path == original.path
                            && e.start <= original.end
                            && e.end >= original.start
                    })
            });
            if !matches {
                continue;
            }
            let value = json!(finding);
            let encoded = serde_json::to_vec(&value)?;
            if !seen.insert(source::hash(&encoded)) {
                continue;
            }
            if used + encoded.len() <= limit {
                used += encoded.len();
                findings.push(value);
            } else {
                deferred += 1;
            }
        }
    }
    Ok(
        json!({"findings":findings,"deferred_findings":deferred,"policy":"Preserved observations for navigation. Cite only original evidence supplied to this writing request after verifying each claim."}),
    )
}

#[cfg(test)]
#[path = "purpose_tests.rs"]
mod tests;
