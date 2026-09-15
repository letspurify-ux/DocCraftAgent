//! Purpose-specific reading over reusable source summaries and original passages.
//! Question results survive compression independently; only request context is packed.
use crate::{
    db, editorial, llm,
    model::{Evidence, Requirement, TaskConfig},
    planning::{Discovery, Finding, SourceBrief, evidence_budget, pack_evidence, validate_brief},
    runner::{RunContext, fatal, is_budget},
    source,
    understanding::Node,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::HashSet;

const READ: &str = "Read source evidence before planning the document. Answer only required_question in the context of purpose. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[string]}. Use 1-4 findings, each with a short topic, an observation under 700 characters, and 1-6 supplied evidence IDs. Keep the entire response under 6500 UTF-8 bytes. Preserve the conditions, branches, inputs, state changes, outputs and producer/consumer connections needed to answer THIS question. Omit unrelated helper details. Do not create an outline or assume runtime order from names/imports. verified_overview contains selected, previously checked observations from detailed source reading, not just the final project summary. Carry only relevant observations with their previously_read anchors; new claims and cross-module connections require original passages supplied in this request. Runtime findings require implementation evidence. XML/configuration/docs/tests only establish context, not execution. Missing excerpts do not prove absent implementation. If the question cannot be fully answered, explicitly record the missing links in uncertainties (at most 4); on the first pass request at most 2 focused followup_queries using observed paths/symbols to retrieve those links. On the final pass keep unresolved gaps in uncertainties and return followup_queries:[]. Retain valid findings from the first pass when adding new details. Reuse the exact topic when correcting or superseding a prior finding. Always include all three arrays; never return null or scalar strings. Use the requested language. The caller assigns the question ID; do not invent requirement IDs.";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct QuestionAnalysis {
    pub requirement_id: String,
    pub question: String,
    pub brief: SourceBrief,
    pub validation_unresolved: bool,
}

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

fn empty_brief() -> SourceBrief {
    SourceBrief {
        findings: vec![],
        uncertainties: vec![],
        followup_queries: vec![],
    }
}

#[derive(Clone)]
struct Detail {
    score: usize,
    finding: Finding,
    evidence: Vec<Evidence>,
}

fn relevance(finding: &Finding, evidence: &[Evidence], terms: &[String]) -> usize {
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
    terms.iter().filter(|t| text.contains(t.as_str())).count()
}

fn select_details(
    best: &mut Vec<Detail>,
    discovery: &Discovery,
    question: &[String],
    purpose: &[String],
) {
    for finding in &discovery.brief.findings {
        let evidence: Vec<_> = discovery
            .evidence
            .iter()
            .filter(|e| finding.evidence_ids.contains(&e.id))
            .cloned()
            .collect();
        // Unverified output is never used as a source summary.
        if evidence.len() != finding.evidence_ids.len() {
            continue;
        }
        let score = relevance(finding, &evidence, question) * (purpose.len() + 1)
            + relevance(finding, &evidence, purpose);
        if score == 0 {
            continue;
        }
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
    best.retain(|d| seen.insert((d.finding.topic.clone(), d.finding.evidence_ids.clone())));
    best.truncate(8);
}

fn detail_seed(details: &[Detail], fallback: &Discovery) -> Discovery {
    let mut brief = empty_brief();
    let mut groups = vec![];
    let mut used: usize = 0;
    for detail in details {
        let size = serde_json::to_vec(&detail.finding).map_or(usize::MAX, |v| v.len());
        if used.saturating_add(size) > 8000 {
            continue;
        }
        used += size;
        brief.findings.push(detail.finding.clone());
        groups.push(detail.evidence.clone());
    }
    if brief.findings.is_empty() {
        // The general overview remains navigation context when lexical matching
        // cannot connect a question to a detailed summary (e.g. another language).
        return fallback.clone();
    }
    Discovery {
        brief,
        evidence: merge_evidence(&groups),
        questions: vec![],
    }
}

/// Scan saved leaf summaries once, keeping bounded candidates per question. Reading
/// them from the database does not re-run the purpose-independent LLM analysis.
async fn seeds(
    ctx: &RunContext,
    whole: &Discovery,
    requirements: &[Requirement],
) -> Result<Vec<Discovery>> {
    let leaves: HashSet<String> = db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:leaves")
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let purpose = source::search_terms(&ctx.snapshot.task.direction);
    let terms: Vec<_> = requirements
        .iter()
        .map(|r| source::search_terms(&r.question))
        .collect();
    let mut best = vec![vec![]; requirements.len()];
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
            if !node.children.is_empty() {
                continue;
            }
            for (selected, question) in best.iter_mut().zip(&terms) {
                select_details(selected, &node.discovery, question, &purpose);
            }
        }
    }
    Ok(best
        .iter()
        .map(|details| detail_seed(details, whole))
        .collect())
}

#[derive(Clone, Serialize, Deserialize)]
struct Reading {
    discovery: Discovery,
    validation_unresolved: bool,
}

fn recover(prior: &Discovery) -> Reading {
    let mut discovery = prior.clone();
    discovery
        .brief
        .uncertainties
        .extend(std::mem::take(&mut discovery.brief.followup_queries));
    discovery.brief.uncertainties.truncate(7);
    discovery.brief.uncertainties.push("이 질문의 추가 분석을 검증하지 못했습니다. 보존한 소스 관찰은 참고할 수 있으나 질문의 답과 누락된 연결은 미확인입니다.".into());
    Reading {
        discovery,
        validation_unresolved: true,
    }
}

fn retain_first_pass(first: Reading, mut last: Reading) -> Reading {
    let mut seen: HashSet<_> = last
        .discovery
        .brief
        .findings
        .iter()
        .map(|f| f.topic.clone())
        .collect();
    last.discovery.brief.findings.extend(
        first
            .discovery
            .brief
            .findings
            .into_iter()
            .filter(|f| seen.insert(f.topic.clone())),
    );
    last.discovery.evidence = merge_evidence(&[last.discovery.evidence, first.discovery.evidence]);
    last.validation_unresolved |= first.validation_unresolved;
    last
}

async fn read_question(
    ctx: &RunContext,
    system: &str,
    requirement: &Requirement,
    prior: &Discovery,
    queries: &[String],
    final_pass: bool,
    key: &str,
) -> Result<Reading> {
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, key).await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let mut limit = evidence_budget(ctx).min(32_000);
    let mut error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        ctx.check()?;
        if limit < 1024 {
            error = "CONTEXT_BUDGET: insufficient room for question reading".into();
            break;
        }
        ctx.event("source_reading", json!({"stage":"understanding","title":"문서 목적에 맞춰 질문별 구현 근거 확인","requirement_id":requirement.id,"question":requirement.question,"pass":if final_pass {2}else{1},"attempt":attempt+1})).await?;
        let mut groups = vec![pack_evidence(
            std::slice::from_ref(&prior.evidence),
            limit / 2,
        )];
        for query in queries.iter().take(2) {
            groups.push(
                source::retrieve(ctx, query, (limit / 2 / queries.len().clamp(1, 2)).max(512))
                    .await?,
            );
        }
        let evidence = pack_evidence(&groups, limit);
        let available = merge_evidence(&[evidence.clone(), prior.evidence.clone()]);
        let mut input = json!({"phase":"purpose_reading","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "required_question":requirement,"verified_overview":prior.brief,
            "source_anchors":prior.evidence.iter().map(|e|json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),
            "evidence":evidence,"evidence_classes":available.iter().map(|e|json!({"id":e.id,"path":e.path,"runtime_allowed":source::is_implementation(&e.path)})).collect::<Vec<_>>(),
            "open_questions":prior.brief.uncertainties,"final_pass":final_pass,"attempt":attempt+1,"previous_error":error,
            "finding_kind_policy":crate::planning::FINDING_KIND_POLICY,"instruction":READ});
        repair.apply(&mut input);
        let parsed = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let mut brief: SourceBrief = repair.decode(&s)?;
            validate_brief(&mut brief, &available, final_pass)?;
            Ok(brief)
        });
        match parsed {
            Ok(brief) => {
                let result = Reading {
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
                        questions: vec![],
                    },
                    validation_unresolved: false,
                };
                db::checkpoint(&ctx.pool, &ctx.id, key, &serde_json::to_value(&result)?).await?;
                return Ok(result);
            }
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                error = editorial::excerpt(&format!("{e:#}"), 1500);
                if error.contains("CONTEXT_BUDGET") {
                    limit /= 2;
                }
                ctx.event("source_reading_retry", json!({"stage":"understanding","requirement_id":requirement.id,"attempt":attempt+1,"error":error})).await?;
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
    ctx.event("source_validation_warning", json!({"title":"질문별 분석 미해결 · 보존한 근거로 계속합니다","requirement_id":requirement.id,"error":error})).await?;
    Ok(recovered)
}

fn combine(questions: Vec<QuestionAnalysis>, evidence: Vec<Evidence>) -> Discovery {
    let mut brief = empty_brief();
    // A navigation overview is bounded, but the complete per-question findings
    // and their originals remain separately addressable in the same checkpoint.
    for index in 0..4 {
        for q in &questions {
            if let Some(f) = q.brief.findings.get(index)
                && brief.findings.len() < 12
            {
                brief.findings.push(f.clone());
            }
        }
    }
    brief.uncertainties = questions
        .iter()
        .filter_map(|q| {
            q.brief
                .uncertainties
                .first()
                .map(|s| format!("{}: {s}", q.requirement_id))
        })
        .take(8)
        .collect();
    Discovery {
        brief,
        evidence,
        questions,
    }
}

pub(crate) async fn analyze(
    ctx: &RunContext,
    system: &str,
    whole: &Discovery,
    requirements: &[Requirement],
) -> Result<Discovery> {
    let config = &ctx.snapshot.settings.llm;
    let scope = source::hash(json!({"version":1,"intent":intent_key(&ctx.snapshot.task),"requirements":requirements,
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
    let seeds = seeds(ctx, whole, requirements).await?;
    let mut questions = vec![];
    let mut groups = vec![];
    for (requirement, seed) in requirements.iter().zip(&seeds) {
        let key = format!("purpose:{scope}:{}", requirement.id);
        let queries = vec![format!(
            "{} {}",
            requirement.question, ctx.snapshot.task.direction
        )];
        let first = read_question(
            ctx,
            system,
            requirement,
            seed,
            &queries,
            false,
            &format!("{key}:first"),
        )
        .await?;
        let mut queries = first.discovery.brief.followup_queries.clone();
        if queries.is_empty()
            && let Some(gap) = first.discovery.brief.uncertainties.first()
        {
            queries.push(format!("{} {gap}", requirement.question));
        }
        let complete = if queries.is_empty() || first.validation_unresolved {
            first
        } else {
            let last = read_question(
                ctx,
                system,
                requirement,
                &first.discovery,
                &queries,
                true,
                &format!("{key}:final"),
            )
            .await?;
            retain_first_pass(first, last)
        };
        groups.push(complete.discovery.evidence);
        questions.push(QuestionAnalysis {
            requirement_id: requirement.id.clone(),
            question: requirement.question.clone(),
            brief: complete.discovery.brief,
            validation_unresolved: complete.validation_unresolved,
        });
    }
    let discovery = combine(questions, merge_evidence(&groups));
    let mut tx = ctx.pool.begin().await?;
    for (key, value) in [
        ("source_understanding_scope", json!(scope)),
        ("source_understanding", serde_json::to_value(&discovery)?),
    ] {
        sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)")
            .bind(&ctx.id).bind(key).bind(value.to_string()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    ctx.event("source_understood", json!({"stage":"understood","findings":discovery.brief.findings.len(),"questions":discovery.questions.len(),
        "unresolved_questions":discovery.questions.iter().filter(|q|q.validation_unresolved || !q.brief.uncertainties.is_empty()).map(|q|&q.requirement_id).collect::<Vec<_>>(),
        "evidence_files":discovery.evidence.iter().map(|e|&e.path).collect::<HashSet<_>>().len(),"selective_analysis":true})).await?;
    Ok(discovery)
}

/// Allocate original excerpts across questions; keep the full archive untouched.
pub(crate) fn pack(discovery: &Discovery, limit: usize) -> Vec<Evidence> {
    let mut groups = vec![];
    let mut assigned = HashSet::new();
    for q in &discovery.questions {
        let ids: HashSet<_> = q
            .brief
            .findings
            .iter()
            .flat_map(|f| &f.evidence_ids)
            .collect();
        let evidence: Vec<_> = discovery
            .evidence
            .iter()
            .filter(|e| ids.contains(&e.id))
            .cloned()
            .collect();
        assigned.extend(evidence.iter().map(|e| e.id.clone()));
        groups.push(evidence);
    }
    groups.push(
        discovery
            .evidence
            .iter()
            .filter(|e| !assigned.contains(&e.id))
            .cloned()
            .collect(),
    );
    pack_evidence(&groups, limit)
}

/// Bounded views for planning/review. Full question results stay in checkpoints
/// and their evidence is also passed to the owning section during writing.
pub(crate) fn context(discovery: &Discovery) -> Vec<Value> {
    let per_question = 20_000 / discovery.questions.len().max(1);
    discovery.questions.iter().map(|q| {
        let mut findings = vec![];
        let mut used: usize = 0;
        for f in &q.brief.findings {
            let mut view = json!(f);
            let remaining = per_question.saturating_sub(800 + used);
            if remaining < 700 { break; }
            let observation_budget = remaining.saturating_sub(view.to_string().len().saturating_sub(f.observation.len()) + 40);
            view["observation"] = json!(editorial::excerpt(&f.observation, observation_budget));
            view["excerpted"] = json!(f.observation.len() > observation_budget);
            used += view.to_string().len();
            findings.push(view);
        }
        json!({"requirement_id":q.requirement_id,"question":editorial::excerpt(&q.question,300),"findings":findings,
            "uncertainties":editorial::excerpt(&q.brief.uncertainties.join("\n"),300),
            "validation_unresolved":q.validation_unresolved,"excerpted":true})
    }).collect()
}

#[cfg(test)]
#[path = "purpose_tests.rs"]
mod tests;
