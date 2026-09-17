//! One purpose-focused source summary over reusable general reading.
//! Preserve detailed observations and originals without inventing a question list.
use crate::{
    db, editorial, llm,
    model::{Evidence, Outline, TaskConfig},
    planning::{Discovery, Finding, SourceBrief, evidence_budget, pack_evidence, validate_brief},
    runner::{RunContext, fatal, is_budget},
    source,
    understanding::Node,
};
use anyhow::Result;
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::{HashMap, HashSet};

const READ_TEMPLATE: &str = "Read source evidence before planning the document. Summarize the code according to purpose, grouping related responsibilities and actual workflows. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[string]}. Use 1-{MAX_FINDINGS} findings with short topics, observations under 1000 characters and 1-{MAX_EVIDENCE_IDS} supplied evidence IDs. Preserve important entry points, processing, conditions, state changes, outputs, consumers and error/cancellation paths. Adapt the emphasis to the user's requested audience and scope. Do not generate a list of reader questions or a table of contents. verified_overview and supporting_findings contain prior observations checked against their original passages. Carry relevant observations using previously_read anchors; use original passages supplied in this request for new claims or cross-module connections. Runtime findings require implementation evidence; XML/configuration/docs/tests establish context, not execution. Do not infer call order from filenames/imports or treat separate alternatives as consecutive steps. Do not infer absent implementation from an omitted excerpt. Record genuinely unresolved links in uncertainties (at most 8). Request at most 3 focused followup_queries using observed paths/symbols ONLY when an important part of the requested flow needs more source evidence. Avoid extra investigation of minor helper details. On the final pass return followup_queries:[] and keep remaining gaps in uncertainties. When correcting an earlier finding reuse its exact topic. Include all three arrays, even when empty. Use the requested language.";
pub(crate) static READ: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::planning::with_limits(READ_TEMPLATE));

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
            "verified_overview":prior.brief,"supporting_findings":context(prior, true),
            "source_anchors":crate::planning::anchors(&prior.evidence, &evidence),
            "evidence":crate::planning::classified(&evidence),
            "open_questions":prior.brief.uncertainties,"final_pass":final_pass,"attempt":attempt+1,"previous_error":error,
            "finding_kind_policy":crate::planning::FINDING_KIND_POLICY,"instruction":READ.as_str()});
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
        "reasoning":config.reasoning,"effort":config.effort,"system":system,"instruction":READ.as_str()
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
/// A request that never cites a passage is sent the observations without ids.
pub(crate) fn context(discovery: &Discovery, with_ids: bool) -> Vec<Value> {
    let mut result = vec![];
    let per = 20_000 / discovery.details.len().max(1);
    for f in &discovery.details {
        let mut view = json!(f);
        if !with_ids && let Some(object) = view.as_object_mut() {
            object.remove("evidence_ids");
        }
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
    let tree = crate::understanding::tree(ctx).await?;
    let (mut used, mut deferred) = (0usize, 0usize);
    let mut findings = vec![];
    let mut seen = HashSet::new();
    for key in &tree.leaves {
        let Some(node) = tree.nodes.get(key) else {
            continue;
        };
        for finding in node.observations() {
            let matches = node.spans.iter().any(|original| {
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

/// The leaves of this section's branches that it explains, most relevant first,
/// and how many it leaves to other sections.
///
/// Sections that share a branch used to receive the same observations - the
/// first of each leaf, in reading order, up to one request's budget - so the
/// rest of a large branch reached none of them. A leaf several sections can
/// reach now goes to the one whose title, key points and query it matches best;
/// ties are spread by reading order so that no section takes them all. Every
/// section computes the same assignment, so each leaf has exactly one owner.
pub(crate) fn assign_leaves(
    tree: &crate::understanding::TreeIndex,
    outline: &Outline,
    index: usize,
    mine: &[String],
) -> (Vec<String>, usize) {
    // Every section's claims, not only this one's: a tie is broken by how many
    // ties each section has already won, and that count has to come out the
    // same whichever section is asking.
    let mut claims: HashMap<&str, Vec<usize>> = HashMap::new();
    let reached: Vec<Vec<String>> = outline
        .sections
        .iter()
        .map(|section| {
            if section.branches.is_empty() {
                vec![]
            } else {
                tree.leaves_under(&section.branches)
            }
        })
        .collect();
    for (section, leaves) in reached.iter().enumerate() {
        for leaf in leaves {
            claims.entry(leaf.as_str()).or_default().push(section);
        }
    }
    let terms: Vec<Vec<String>> = outline
        .sections
        .iter()
        .map(|s| {
            source::search_terms(&format!(
                "{} {} {}",
                s.title,
                s.key_points.join(" "),
                s.query
            ))
        })
        .collect();
    let text = |leaf: &str| {
        tree.nodes.get(leaf).map_or(String::new(), |node| {
            node.observations()
                .map(|f| format!("{} {}", f.topic, f.observation))
                .chain(node.spans.iter().map(|s| s.path.clone()))
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        })
    };
    let score = |text: &str, section: usize| {
        terms[section]
            .iter()
            .filter(|term| text.contains(term.as_str()))
            .count()
    };
    let mut owner: HashMap<&str, usize> = HashMap::new();
    let mut won = vec![0usize; outline.sections.len()];
    for leaf in &tree.leaves {
        let Some(candidates) = claims.get(leaf.as_str()) else {
            continue;
        };
        if candidates.len() < 2 {
            continue;
        }
        let text = text(leaf);
        let scores: Vec<usize> = candidates.iter().map(|c| score(&text, *c)).collect();
        let best = scores.iter().copied().max().unwrap_or(0);
        let chosen = candidates
            .iter()
            .zip(&scores)
            .filter(|(_, s)| **s == best)
            .map(|(c, _)| *c)
            .min_by_key(|c| (won[*c], *c))
            .unwrap_or(candidates[0]);
        won[chosen] += 1;
        owner.insert(leaf.as_str(), chosen);
    }
    let mut kept = vec![];
    let mut elsewhere = 0;
    for leaf in mine {
        if owner.get(leaf.as_str()).is_some_and(|o| *o != index) {
            elsewhere += 1;
            continue;
        }
        kept.push((score(&text(leaf), index), leaf.clone()));
    }
    // Most relevant first; the sort is stable, so reading order breaks ties.
    kept.sort_by(|a, b| b.0.cmp(&a.0));
    (kept.into_iter().map(|(_, leaf)| leaf).collect(), elsewhere)
}

/// The readings of the source a section was planned to cover, and the passages
/// they rest on.
///
/// Whole-source reading visits every passage, but a writer used to see only the
/// observations that overlapped what its search happened to retrieve, so the
/// reading could not tell it what the search had missed. A section that names
/// its branches gets their leaf observations as the list of behavior in its
/// scope - one observation from every leaf before a second from any, so a large
/// branch is described across its whole breadth - and the passages behind the
/// shown observations as evidence it may pack.
pub(crate) async fn branch_memory(
    ctx: &RunContext,
    outline: &Outline,
    index: usize,
    limit: usize,
) -> Result<(Value, Vec<Evidence>)> {
    let Some(plan) = outline.sections.get(index) else {
        return Ok((Value::Null, vec![]));
    };
    if plan.branches.is_empty() {
        return Ok((Value::Null, vec![]));
    }
    let tree = crate::understanding::tree(ctx).await?;
    let mine = tree.leaves_under(&plan.branches);
    let (leaves, elsewhere) = assign_leaves(&tree, outline, index, &mine);
    let roots = &ctx.snapshot.task.sources;
    let relative = |path: &str| {
        roots
            .iter()
            .filter_map(|root| path.strip_prefix(&format!("{}/", root.trim_end_matches('/'))))
            .min_by_key(|rest| rest.len())
            .unwrap_or(path)
            .to_string()
    };
    let mut shown = vec![];
    let mut cited: Vec<(String, String)> = vec![];
    let (mut used, mut deferred) = (0usize, 0usize);
    let deepest = leaves
        .iter()
        .filter_map(|key| tree.nodes.get(key))
        .map(|node| node.observations().count())
        .max()
        .unwrap_or(0);
    for round in 0..deepest {
        for key in &leaves {
            let Some(node) = tree.nodes.get(key) else {
                continue;
            };
            let Some(finding) = node.observations().nth(round) else {
                continue;
            };
            let sources: Vec<String> = node
                .spans
                .iter()
                .filter(|span| finding.evidence_ids.contains(&span.id))
                .map(|span| format!("{}:{}-{}", relative(&span.path), span.start, span.end))
                .collect();
            let record = json!({"topic":finding.topic,
                "observation":editorial::excerpt(&finding.observation, 900),
                "kind":finding.kind,"sources":sources});
            let size = serde_json::to_vec(&record)?.len();
            if used + size > limit {
                deferred += 1;
                continue;
            }
            used += size;
            shown.push(record);
            cited.extend(
                finding
                    .evidence_ids
                    .iter()
                    .map(|id| (key.clone(), id.clone())),
            );
        }
    }
    // Only the leaves that contributed a shown observation are opened for text.
    let mut opened: HashMap<String, Vec<Evidence>> = HashMap::new();
    let mut evidence = vec![];
    let mut seen = HashSet::new();
    for (key, id) in cited {
        if !opened.contains_key(&key) {
            ctx.check()?;
            let passages = db::load_checkpoint(&ctx.pool, &ctx.id, &key)
                .await?
                .and_then(|value| serde_json::from_value::<Node>(value).ok())
                .map(|node| node.discovery.evidence)
                .unwrap_or_default();
            opened.insert(key.clone(), passages);
        }
        if let Some(passage) = opened
            .get(&key)
            .and_then(|passages| passages.iter().find(|e| e.id == id))
            && seen.insert(id)
        {
            evidence.push(passage.clone());
        }
    }
    Ok((
        json!({"findings":shown,"deferred_findings":deferred,"leaves":leaves.len(),
            "leaves_left_to_other_sections":elsewhere,
            "policy":"What the whole-source reading found in the parts of the source this section covers, one observation per leaf before a second from any. It is the checklist of behavior in scope, not evidence: cite supplied passages."}),
        evidence,
    ))
}

#[cfg(test)]
#[path = "purpose_tests.rs"]
mod tests;
