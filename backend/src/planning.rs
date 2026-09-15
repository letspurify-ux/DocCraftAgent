//! Read implementation before choosing the reader's journey. Discovery is bounded
//! and checkpointed independently of the final outline and section drafts.
use crate::{
    db, editorial, llm,
    model::{Evidence, Outline, OutlineReview, Requirement, SectionPlan},
    runner::{RunContext, fatal, is_budget, outline_diagram_error},
    source,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;

const DISCOVERY: &str = "Read source evidence before planning the document. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[string]}. Do not produce a table of contents yet. Infer the intended reader and task from purpose, then read actual source passages to identify the relevant entry, prerequisites, actors, inputs, processing, persisted or returned results, consumer, and important alternative/error paths. Adapt to the supplied project; do not force a web request model onto unrelated code. Each finding must explain a concrete connection or behavior, including conditions and outputs, rather than list symbols. Use at most 12 findings, each with a short topic and observation (maximum 1200 characters), and 1-6 supplied evidence IDs. Runtime findings require implementation evidence; filenames, imports, README, comments and tests alone do not prove execution. Context findings can describe documented setup or intended usage, explicitly distinguished from observed implementation. Inventory is only a sampled navigation aid. Do not infer a call order from names or treat separate alternatives as consecutive steps. Mark missing links in uncertainties (at most 8). Request at most 3 focused followup_queries naming observed files/symbols or unresolved connections most important to the reader; prefer finding missing entry/result/branch evidence over more detail on already understood helpers. Do not invent identifiers. On the final pass, return no followup_queries and retain unresolved links in uncertainties. Carry relevant findings from verified_overview using its previously_read source anchors; those observations were checked against the originals in the exhaustive reading. Use passages in THIS request for new claims and connections. Do not treat omitted excerpts as missing project coverage; prior gaps are research questions, not facts. Use the requested language for observations and uncertainties. Empty findings are invalid; if only contextual evidence exists, say so without inventing runtime behavior.";

const PLAN: &str = "Return JSON {sections:[{title:string,query:string,reader_question:string,handoff:string,diagrams:[string],depends_on:[number],evidence_ids:[string]}],reader_goal:string,storyline:string,terminology:[string]}. Design one coherent document for the intended reader using the source_brief AND actual evidence read before this plan. The brief is an evidence-linked analysis, not independently verified truth: resolve contradictions against supplied implementation and respect its uncertainties. inventory_sample is only a navigation map: its paths may be named in section queries but are not evidence and must never appear in evidence_ids. Infer the audience and desired outcome from purpose. Choose 1-32 distinct sections in the order the reader needs to understand or perform the work. 32 is a hard ceiling, not a target. Choose the smallest section count that covers the requested scope clearly, based on reader goals, source-supported workflows, complexity and distinct reader questions. A narrow topic may need only 1-3 sections. Add a section only when it answers a substantial separate reader question; merge overlapping or thin topics and use subsections for supporting details. Do not create one section per file or module, pad the outline, or split a coherent workflow just to increase the count. Explain briefly in storyline why the chosen scope and grouping suit this document. Start with orientation and the relevant end-to-end picture, then introduce prerequisites before the actions that need them, show one normal path through to an observable result, and place alternatives/troubleshooting where they help the reader. Adapt the order to the actual source and purpose, not a fixed template or catalog of files/classes/subsystems. Separate reading order from runtime order: conditional branches and independent workflows must not become a fictional single execution trace. reader_goal states what the reader should achieve. storyline explains how the questions connect and why this order helps that goal. Each reader_question is one non-duplicated question this section resolves; handoff identifies the concrete result or decision the next section builds on (empty only for the final section). depends_on lists only earlier zero-based SECTION indices needed to understand this section; it is a reading prerequisite, not a function call graph. Every section must carry 1-8 supplied evidence_ids that anchor its topic. previously_read source_anchors are originals checked during earlier reading and can anchor an existing source_brief finding even if the passage is not repeated in this bounded request; they do not justify inventing new behavior. query names concrete implementation files, symbols and actions needed to deepen those anchors during writing. For cross-layer or end-to-end documentation, distribute queries across the relevant entry, orchestration, persistence, maintenance and result-consumer modules visible in inventory_sample instead of repeatedly relying on the same few files. For an end-to-end guide, include the evidenced entry, orchestration and result consumer in the opening section's anchors/query where available. Do not invent missing links to make the story smooth; explain limits or separate paths. Assign each explanation to one section to avoid repeated overviews. terminology contains at most 12 short, consistent definitions supported by evidence. Allocate diagrams across the WHOLE document, at most 4 per section: each diagrams entry is one plain-language objective, NEVER diagram code or an assumed call sequence. An empty array means no diagram. Name diagram types explicitly when purpose requests them. Respect max_diagrams (null means no numeric cap) and the requested global number/types; do not repeat an overall flow diagram in every section. Keep titles under 300 bytes, query under 2000 bytes, reader_question and handoff under 1500 bytes, reader_goal under 2000 bytes, storyline under 4000 bytes and each terminology entry under 500 bytes. Use the requested document language. Coverage is selective; never claim all code was understood.";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingKind {
    Runtime,
    Context,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub topic: String,
    pub observation: String,
    pub kind: FindingKind,
    pub evidence_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SourceBrief {
    pub findings: Vec<Finding>,
    pub uncertainties: Vec<String>,
    pub followup_queries: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Discovery {
    pub brief: SourceBrief,
    pub evidence: Vec<Evidence>,
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}

/// Resolve only unambiguous prefixes from THIS request, then persist full hashes.
fn resolve_ids(ids: &mut [String], evidence: &[Evidence], max: usize) -> Result<()> {
    ensure!(
        !ids.is_empty() && ids.len() <= max,
        "Supply 1-{max} evidence_ids"
    );
    let mut seen = HashSet::new();
    for id in ids {
        ensure!(
            id.len() >= 8 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid evidence ID: {id}"
        );
        let matches: Vec<_> = evidence
            .iter()
            .filter(|e| e.id.starts_with(id.as_str()))
            .collect();
        ensure!(matches.len() == 1, "Unknown or ambiguous evidence ID: {id}");
        *id = matches[0].id.clone();
        ensure!(seen.insert(id.clone()), "Repeated evidence ID: {id}");
    }
    Ok(())
}

pub(crate) fn validate_brief(
    brief: &mut SourceBrief,
    evidence: &[Evidence],
    final_pass: bool,
) -> Result<()> {
    ensure!(
        !brief.findings.is_empty() && brief.findings.len() <= 12,
        "Supply 1-12 source findings"
    );
    ensure!(
        brief.uncertainties.len() <= 8 && brief.uncertainties.iter().all(|s| bounded_text(s, 1500)),
        "Invalid uncertainties"
    );
    // Empty strings are a common representation of no further questions.
    brief
        .followup_queries
        .retain(|query| !query.trim().is_empty());
    ensure!(
        brief.followup_queries.len() <= if final_pass { 8 } else { 3 }
            && brief.followup_queries.iter().all(|s| bounded_text(s, 1500)),
        "Supply at most {} nonempty followup_queries, each at most 1500 bytes (received {} queries; lengths {:?})",
        if final_pass { 8 } else { 3 },
        brief.followup_queries.len(),
        brief
            .followup_queries
            .iter()
            .map(|s| s.len())
            .collect::<Vec<_>>()
    );
    // Some providers return useful research questions even on the final pass.
    // Preserve those gaps rather than failing an otherwise grounded reading or
    // silently pretending the requested investigation happened.
    if final_pass {
        for query in std::mem::take(&mut brief.followup_queries) {
            if !brief.uncertainties.contains(&query) {
                brief.uncertainties.push(query);
            }
        }
    }
    for finding in &mut brief.findings {
        ensure!(
            bounded_text(&finding.topic, 300) && bounded_text(&finding.observation, 4000),
            "Invalid source finding text"
        );
        resolve_ids(&mut finding.evidence_ids, evidence, 6)?;
        if matches!(finding.kind, FindingKind::Runtime) {
            ensure!(
                evidence
                    .iter()
                    .any(|e| finding.evidence_ids.contains(&e.id)
                        && source::is_implementation(&e.path)),
                "Finding {:?} cites only context evidence ({:?}); classify it as context and describe documented/test intent, or cite an actual supplied implementation passage. Runtime finding must cite implementation evidence",
                finding.topic,
                evidence
                    .iter()
                    .filter(|e| finding.evidence_ids.contains(&e.id))
                    .map(|e| &e.path)
                    .collect::<Vec<_>>()
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_outline(
    outline: &mut Outline,
    evidence: &[Evidence],
    maximum: Option<u32>,
) -> Result<()> {
    ensure!(
        !outline.sections.is_empty() && outline.sections.len() <= 32,
        "Supply 1-32 sections"
    );
    ensure!(
        bounded_text(&outline.reader_goal, 2000) && bounded_text(&outline.storyline, 4000),
        "Supply a bounded reader_goal and storyline"
    );
    ensure!(
        outline.terminology.len() <= 12 && outline.terminology.iter().all(|s| bounded_text(s, 500)),
        "Invalid terminology"
    );
    let mut titles = HashSet::new();
    let mut questions = HashSet::new();
    let mut ids = HashSet::new();
    let mut ownership = HashSet::new();
    let count = outline.sections.len();
    for (index, section) in outline.sections.iter_mut().enumerate() {
        if section.id.is_empty() {
            section.id =
                source::hash(format!("{}:{}", section.title, section.reader_question).as_bytes());
        }
        ensure!(
            section.id.len() <= 64
                && section
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && ids.insert(section.id.clone()),
            "Invalid or repeated section ID"
        );
        if !outline.requirements.is_empty() {
            ensure!(
                !section.key_points.is_empty()
                    && section.key_points.len() <= 12
                    && section.key_points.iter().all(|p| bounded_text(p, 1500)),
                "Supply 1-12 key_points per section"
            );
            ensure!(
                section.out_of_scope.len() <= 12
                    && section.out_of_scope.iter().all(|p| bounded_text(p, 1500)),
                "Invalid out_of_scope"
            );
            for requirement in &section.owns_requirement_ids {
                ensure!(
                    outline.requirements.iter().any(|r| &r.id == requirement)
                        && ownership.insert(requirement.clone()),
                    "Each requirement must have exactly one owning section"
                );
            }
        }
        ensure!(
            bounded_text(&section.title, 299)
                && bounded_text(&section.query, 1999)
                && bounded_text(&section.reader_question, 1500),
            "Each section needs a bounded title, query and reader_question"
        );
        ensure!(
            section.handoff.len() <= 1500
                && (index + 1 == count || !section.handoff.trim().is_empty()),
            "Every non-final section needs a concrete handoff"
        );
        let normalize = |s: &str| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        };
        ensure!(
            titles.insert(normalize(&section.title))
                && questions.insert(normalize(&section.reader_question)),
            "Sections must have distinct titles and reader questions"
        );
        let mut dependencies = HashSet::new();
        ensure!(
            section
                .depends_on
                .iter()
                .all(|d| *d < index && dependencies.insert(*d)),
            "depends_on must contain distinct earlier zero-based section indices; place prerequisites before use"
        );
        ensure!(
            section
                .diagrams
                .as_ref()
                .is_some_and(|d| d.len() <= 4 && d.iter().all(|s| bounded_text(s, 1500))),
            "Every section needs a bounded diagrams array"
        );
        resolve_ids(&mut section.evidence_ids, evidence, 8)?;
    }
    ensure!(
        outline
            .requirements
            .iter()
            .all(|r| ownership.contains(&r.id)),
        "Missing required topic ownership"
    );
    if let Some(error) = outline_diagram_error(outline, maximum) {
        bail!("{error}");
    }
    Ok(())
}

/// Keep real passages intact (and their hashes valid), distributing space across
/// independent retrieval queries before taking more results from any one query.
pub(crate) fn pack_evidence(groups: &[Vec<Evidence>], limit: usize) -> Vec<Evidence> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    let mut used = 0;
    for index in 0..groups.iter().map(Vec::len).max().unwrap_or(0) {
        for group in groups {
            if let Some(e) = group.get(index) {
                let size = e.content.len() + e.path.len() + 256;
                if used + size <= limit && seen.insert(e.id.clone()) {
                    used += size;
                    selected.push(e.clone());
                }
            }
        }
    }
    selected
}

fn evidence_budget(ctx: &RunContext) -> usize {
    let l = &ctx.snapshot.settings.llm;
    let available = (l.context_limit.min(l.model_context_limit).min(200_000) as usize)
        .saturating_mul(100usize.saturating_sub(l.safety_percent as usize))
        / 100;
    available
        .saturating_sub(l.max_output_tokens as usize + ctx.snapshot.task.direction.len() + 32_000)
        .min(48_000)
}

async fn read_sources(
    ctx: &RunContext,
    system: &str,
    inventory: &str,
    previous: Option<&Discovery>,
) -> Result<Discovery> {
    let final_pass = previous.is_some();
    let mut queries = previous.map(|p| p.brief.followup_queries.clone()).unwrap_or_else(|| vec![format!(
        "{} main entry route handler request input process execute return result response output startup configuration error",
        ctx.snapshot.task.direction
    )]);
    queries.dedup();
    let mut previous_error = String::new();
    let mut repair = llm::JsonRepair::default();
    let mut limit = evidence_budget(ctx);
    let mut response_received = false;
    for attempt in 0..3 {
        ctx.check()?;
        ensure!(
            limit >= 1024,
            "CONTEXT_BUDGET: insufficient room for source understanding"
        );
        ctx.event("source_reading", json!({"stage":"understanding","title":if final_pass {"누락된 연결을 추가 확인"} else {"구현을 읽고 동작 흐름 파악"},"pass":if final_pass {2} else {1},"attempt":attempt+1})).await?;
        let mut groups = Vec::new();
        if let Some(p) = previous {
            let anchors: Vec<_> = p
                .evidence
                .iter()
                .filter(|e| {
                    p.brief
                        .findings
                        .iter()
                        .any(|f| f.evidence_ids.contains(&e.id))
                })
                .cloned()
                .collect();
            groups.push(pack_evidence(&[anchors], limit / 2));
        }
        let per_query = if final_pass {
            limit / 2 / queries.len().max(1)
        } else {
            limit
        };
        for query in &queries {
            groups.push(source::retrieve(ctx, query, per_query.max(512)).await?);
        }
        let evidence = pack_evidence(&groups, limit);
        ensure!(
            !evidence.is_empty(),
            "No source evidence available before planning"
        );
        let mut available = evidence.clone();
        if let Some(prior) = previous {
            for e in &prior.evidence {
                if !available.iter().any(|a| a.id == e.id) {
                    available.push(e.clone());
                }
            }
        }
        let mut input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "verified_overview":previous.map(|p| &p.brief),
            "source_anchors":previous.map(|p| p.evidence.iter().map(|e| json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>()),
            "inventory_sample":editorial::excerpt(inventory, 16000 >> attempt),"evidence":evidence,
            "open_questions":previous.map(|p| &p.brief.uncertainties),"final_pass":final_pass,
            "previous_error":previous_error,"attempt":attempt+1,"instruction":DISCOVERY});
        response_received = false;
        repair.apply(&mut input);
        let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            response_received = true;
            let mut brief: SourceBrief = repair.decode(&s)?;
            validate_brief(&mut brief, &available, final_pass)?;
            Ok(brief)
        });
        match result {
            Ok(brief) => {
                return Ok(Discovery {
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
                });
            }
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                previous_error = editorial::excerpt(&format!("{e:#}"), 1500);
                if previous_error.contains("CONTEXT_BUDGET") {
                    limit /= 2;
                }
                ctx.event(
                    "source_reading_retry",
                    json!({"stage":"understanding","attempt":attempt+1,"error":previous_error}),
                )
                .await?;
            }
        }
    }
    if response_received && let Some(prior) = previous {
        let mut recovered = prior.clone();
        recovered.brief.followup_queries.clear();
        recovered.brief.uncertainties.truncate(7);
        recovered.brief.uncertainties.push("추가 소스 분석 응답을 검증하지 못했습니다. 이전에 검증된 관찰을 유지하며 추가 연결은 미확인입니다.".into());
        let key = format!(
            "source_reading:unresolved:{}",
            if final_pass { 2 } else { 1 }
        );
        db::checkpoint(
            &ctx.pool,
            &ctx.id,
            &key,
            &json!({"error":previous_error,"response":repair.response}),
        )
        .await?;
        if let Some(mut coverage) =
            db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:coverage").await?
        {
            coverage["complete"] = json!(false);
            coverage["additional_reading_unresolved"] = json!(true);
            db::checkpoint(&ctx.pool, &ctx.id, "understanding:coverage", &coverage).await?;
        }
        ctx.event("source_validation_warning", json!({"title":"추가 분석 미해결 · 이전 검증 결과로 계속합니다","error":previous_error})).await?;
        return Ok(recovered);
    }
    bail!("Unable to understand source before planning: {previous_error}")
}

async fn discover(ctx: &RunContext, system: &str) -> Result<Discovery> {
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "source_understanding").await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let mut whole = crate::understanding::analyze(ctx, system).await?;
    whole.brief.followup_queries = vec![format!(
        "{} entry result errors",
        ctx.snapshot.task.direction
    )];
    let first: Discovery =
        if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "source_reading:0").await? {
            serde_json::from_value(saved)?
        } else {
            let overview = serde_json::to_string(&whole.brief)?;
            let read = read_sources(ctx, system, &overview, Some(&whole)).await?;
            db::checkpoint(
                &ctx.pool,
                &ctx.id,
                "source_reading:0",
                &serde_json::to_value(&read)?,
            )
            .await?;
            read
        };
    let complete = if first.brief.followup_queries.is_empty() {
        first
    } else {
        read_sources(
            ctx,
            system,
            &serde_json::to_string(&whole.brief)?,
            Some(&first),
        )
        .await?
    };
    db::checkpoint(
        &ctx.pool,
        &ctx.id,
        "source_understanding",
        &serde_json::to_value(&complete)?,
    )
    .await?;
    ctx.event("source_understood", json!({"stage":"understood","findings":complete.brief.findings.len(),"uncertainties":complete.brief.uncertainties,"evidence_files":complete.evidence.iter().map(|e| &e.path).collect::<HashSet<_>>().len(),"selective_analysis":true})).await?;
    Ok(complete)
}

pub async fn outline(ctx: &RunContext, system: &str) -> Result<Outline> {
    // Old runs retain their outline and numbered drafts. New runs always discover
    // first; no file-name-only fallback is allowed after discovery failures.
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "outline").await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let discovery = discover(ctx, system).await?;
    let inventory = serde_json::to_string(&discovery.brief)?;
    let requirements = requirements(ctx, system).await?;
    let state = db::load_checkpoint(&ctx.pool, &ctx.id, "outline_state")
        .await?
        .unwrap_or(json!({"revision":0,"round":0}));
    let mut revision = state["revision"].as_u64().unwrap_or(0) as u32;
    let mut round = state["round"].as_u64().unwrap_or(0);
    let mut extra_queries = state["extra_queries"].as_u64().unwrap_or(0) as usize;
    let mut feedback = db::load_checkpoint(&ctx.pool, &ctx.id, "outline_feedback")
        .await?
        .unwrap_or(json!({}));
    let mut discovery = discovery;
    loop {
        let pending = db::load_checkpoint(&ctx.pool, &ctx.id, "outline_candidate").await?;
        if pending.is_none() {
            revision += 1;
        }
        let mut limit = evidence_budget(ctx);
        let mut previous_error = String::new();
        let mut repair = llm::JsonRepair::default();
        let mut candidate = pending.map(serde_json::from_value::<Outline>).transpose()?;
        for attempt in 0..3 {
            if candidate.is_some() {
                break;
            }
            let evidence = pack_evidence(std::slice::from_ref(&discovery.evidence), limit);
            ensure!(
                !evidence.is_empty(),
                "CONTEXT_BUDGET: insufficient room for grounded outline"
            );
            let brief = &discovery.brief;
            ctx.event("outline_planning", json!({"stage":"planning","title":"구현 근거에 맞춰 설명 순서 구성","attempt":attempt+1,"evidence_chunks":evidence.len()})).await?;
            let mut input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "source_brief":brief,"source_anchors":discovery.evidence.iter().map(|e| json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),"project_overview":inventory,"requirements":requirements,"feedback":feedback,"revision":revision,
            "evidence":evidence,"max_diagrams":ctx.snapshot.task.max_diagrams,
            "previous_error":previous_error,"attempt":attempt+1,"instruction":format!("{PLAN} Additionally each section must include owns_requirement_ids (each supplied requirement has exactly ONE owner across the document), key_points (1-12 concrete explanations), and out_of_scope (topics owned elsewhere). Respect user feedback and preserve valid existing section IDs when supplied. Do not remove requirements to hide missing coverage.")});
            repair.apply(&mut input);
            let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
                let mut plan: Outline = repair.decode(&s)?;
                plan.requirements = requirements.clone();
                plan.revision = revision;
                validate_outline(
                    &mut plan,
                    &discovery.evidence,
                    ctx.snapshot.task.max_diagrams,
                )?;
                Ok(plan)
            });
            match result {
                Ok(plan) => {
                    candidate = Some(plan);
                    break;
                }
                Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
                Err(e) => {
                    llm::forget(ctx, system, input).await?;
                    previous_error = editorial::excerpt(&format!("{e:#}"), 1500);
                    if previous_error.contains("CONTEXT_BUDGET") {
                        limit /= 2;
                    }
                    ctx.event(
                        "outline_validation",
                        json!({"stage":"planning","attempt":attempt+1,"error":previous_error}),
                    )
                    .await?;
                }
            }
        }
        let plan = candidate.ok_or_else(|| {
            anyhow::anyhow!(
                "AWAITING_OUTLINE: Invalid outline after three attempts: {previous_error}"
            )
        })?;
        db::checkpoint(
            &ctx.pool,
            &ctx.id,
            "outline_candidate",
            &serde_json::to_value(&plan)?,
        )
        .await?;
        db::checkpoint(
            &ctx.pool,
            &ctx.id,
            &format!("outline_candidate:{revision}"),
            &serde_json::to_value(&plan)?,
        )
        .await?;
        db::checkpoint(
            &ctx.pool,
            &ctx.id,
            "outline_state",
            &json!({"revision":revision,"round":round,"extra_queries":extra_queries}),
        )
        .await?;
        let review = review_outline(ctx, system, &plan, &discovery).await?;
        ctx.event("outline_review", json!({"stage":"outline_review","title":"목차의 누락·중복·순서 검토","revision":revision,"issues":review.issues})).await?;
        if review.issues.iter().all(|i| i.severity != "major") {
            let approved = db::load_checkpoint(&ctx.pool, &ctx.id, "outline_approved")
                .await?
                .and_then(|v| v.as_u64())
                == Some(u64::from(revision));
            if ctx.snapshot.task.preview_outline && !approved {
                bail!("AWAITING_OUTLINE: 목차를 확인하고 본문 작성을 시작하세요");
            }
            db::checkpoint(&ctx.pool, &ctx.id, "outline", &serde_json::to_value(&plan)?).await?;
            return Ok(plan);
        }
        if round >= 2 {
            bail!(
                "AWAITING_OUTLINE: 두 차례 보정 후 주요 구성 문제가 남았습니다. 목차 또는 방향을 수정하세요"
            );
        }
        // Ground requested structural corrections before generating the next plan.
        let mut extra = vec![discovery.evidence.clone()];
        for query in review
            .issues
            .iter()
            .filter(|i| !i.query.trim().is_empty())
            .take(3usize.saturating_sub(extra_queries))
        {
            extra.push(source::retrieve(ctx, &query.query, evidence_budget(ctx) / 3).await?);
            extra_queries += 1;
        }
        discovery.evidence = pack_evidence(&extra, evidence_budget(ctx));
        discovery.brief.findings.retain(|f| {
            f.evidence_ids
                .iter()
                .all(|id| discovery.evidence.iter().any(|e| &e.id == id))
        });
        round += 1;
        feedback = json!({"previous_plan":plan,"issues":review.issues});
        let mut tx = ctx.pool.begin().await?;
        for (step, value) in [
            ("source_understanding", serde_json::to_value(&discovery)?),
            ("outline_feedback", feedback.clone()),
            (
                "outline_state",
                json!({"revision":revision,"round":round,"extra_queries":extra_queries}),
            ),
        ] {
            sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&ctx.id).bind(step).bind(value.to_string()).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM checkpoints WHERE run_id=? AND step='outline_candidate'")
            .bind(&ctx.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
}

async fn requirements(ctx: &RunContext, system: &str) -> Result<Vec<Requirement>> {
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "document_requirements").await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let mut error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        let mut input = json!({"phase":"document_intent","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,"attempt":attempt,"previous_error":error,"instruction":"Extract the required reader questions from the user's purpose. Return JSON {requirements:[{id:string,question:string}]}. Supply 1-12 concise questions that together fulfill the explicit purpose. Do not add unrelated installation, security or operations topics. Each question must be under 1000 characters. Use the requested language."});
        repair.apply(&mut input);
        let parsed = llm::call(ctx, system, input.clone())
            .await
            .and_then(|text| {
                #[derive(Deserialize)]
                struct Intent {
                    requirements: Vec<Requirement>,
                }
                let mut r = repair.decode::<Intent>(&text)?.requirements;
                ensure!(
                    !r.is_empty()
                        && r.len() <= 12
                        && r.iter().all(|v| bounded_text(&v.question, 3000)),
                    "Supply 1-12 bounded reader questions"
                );
                for (i, v) in r.iter_mut().enumerate() {
                    v.id = format!("r{}", i + 1);
                }
                Ok(r)
            });
        match parsed {
            Ok(r) => {
                db::checkpoint(
                    &ctx.pool,
                    &ctx.id,
                    "document_requirements",
                    &serde_json::to_value(&r)?,
                )
                .await?;
                return Ok(r);
            }
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                error = editorial::excerpt(&format!("{e:#}"), 1000);
            }
        }
    }
    bail!("AWAITING_OUTLINE: Could not extract document requirements: {error}")
}

async fn review_outline(
    ctx: &RunContext,
    system: &str,
    plan: &Outline,
    discovery: &Discovery,
) -> Result<OutlineReview> {
    let key = format!("outline_review:{}", plan.revision);
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let mut error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        let mut input = json!({"phase":"outline_review","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,"outline":plan,"source_brief":discovery.brief,"evidence":pack_evidence(std::slice::from_ref(&discovery.evidence),evidence_budget(ctx)),"source_anchors":discovery.evidence.iter().map(|e| json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),"attempt":attempt,"previous_error":error,"instruction":"Review this outline BEFORE writing. Return JSON {issues:[{severity:'major'|'minor',code:string,message:string,section_ids:[string],requirement_ids:[string],query:string}]}. Check missing reader requirements, semantic overlap, prerequisites after use, oversized or empty sections, audience mismatch, unsupported runtime ordering and missing important source branches. A short orientation referencing a detailed section is valid. Sharing evidence is not duplication. Every issue must identify concrete affected IDs and a necessary correction, grounded in the supplied outline or source. For missing evidence query names observed files/symbols. Previously_read anchors support observations already checked in the source_brief. Do not invent defects or infer absence from an excerpt. Empty issues means no concrete defect supported. Use the requested language. At most 12 issues."});
        repair.apply(&mut input);
        let parsed = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let r: OutlineReview = repair.decode(&s)?;
            ensure!(
                r.issues.len() <= 12
                    && r.issues
                        .iter()
                        .all(|i| ["major", "minor"].contains(&i.severity.as_str())
                            && bounded_text(&i.message, 4000)
                            && bounded_text(&i.code, 100)
                            && i.query.len() <= 2000
                            && i.section_ids
                                .iter()
                                .all(|id| plan.sections.iter().any(|s| &s.id == id))
                            && i.requirement_ids
                                .iter()
                                .all(|id| plan.requirements.iter().any(|r| &r.id == id))),
                "Invalid outline review issue or reference"
            );
            Ok(r)
        });
        match parsed {
            Ok(r) => {
                db::checkpoint(&ctx.pool, &ctx.id, &key, &serde_json::to_value(&r)?).await?;
                return Ok(r);
            }
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                error = editorial::excerpt(&format!("{e:#}"), 1000);
            }
        }
    }
    bail!("AWAITING_OUTLINE: Outline review could not complete: {error}")
}

pub async fn section_evidence(ctx: &RunContext, plan: &SectionPlan) -> Result<Vec<Evidence>> {
    if plan.evidence_ids.is_empty() {
        return Ok(vec![]);
    }
    let saved = db::load_checkpoint(&ctx.pool, &ctx.id, "source_understanding")
        .await?
        .context("Missing source understanding for grounded section plan")?;
    let discovery: Discovery = serde_json::from_value(saved)?;
    let evidence: Vec<_> = discovery
        .evidence
        .into_iter()
        .filter(|e| plan.evidence_ids.contains(&e.id))
        .collect();
    ensure!(
        evidence.len() == plan.evidence_ids.len(),
        "Missing planned source evidence"
    );
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(path: &str, content: &str) -> Evidence {
        Evidence {
            id: source::hash(format!("{path}:{content}").as_bytes()),
            path: path.into(),
            start: 1,
            end: 1,
            content: content.into(),
        }
    }

    #[test]
    fn discovery_requires_real_unambiguous_implementation_anchors() -> Result<()> {
        let implementation = evidence("/project/a.py", "def receive(x): return finish(x)");
        let readme = evidence("/project/README.md", "The application returns a result.");
        let mut brief: SourceBrief = serde_json::from_value(json!({
            "findings":[{"topic":"entry","observation":"receive passes input to finish",
                "kind":"runtime","evidence_ids":[&implementation.id[..8]]}],
            "uncertainties":["finish implementation is missing"],"followup_queries":["finish", "", "  "]
        }))?;
        let sources = vec![implementation.clone(), readme.clone()];
        validate_brief(&mut brief, &sources, false)?;
        assert_eq!(brief.findings[0].evidence_ids[0], implementation.id);
        brief
            .followup_queries
            .extend(["input", "output", "cancel"].map(String::from));
        assert!(validate_brief(&mut brief, &sources, false).is_err());
        validate_brief(&mut brief, &sources, true)?;
        assert!(brief.followup_queries.is_empty());
        assert!(brief.uncertainties.iter().any(|s| s == "finish"));
        brief.followup_queries.clear();
        brief.findings[0].evidence_ids = vec![readme.id];
        assert!(validate_brief(&mut brief, &sources, true).is_err());
        brief.findings[0].kind = FindingKind::Context;
        validate_brief(&mut brief, &sources, true)?;
        brief.findings[0].evidence_ids = vec!["ffffffff".into()];
        assert!(validate_brief(&mut brief, &sources, true).is_err());
        let mut collision = implementation.clone();
        collision.id.replace_range(
            8..9,
            if &implementation.id[8..9] == "0" {
                "1"
            } else {
                "0"
            },
        );
        brief.findings[0].evidence_ids = vec![implementation.id[..8].into()];
        assert!(validate_brief(&mut brief, &[implementation, collision], true).is_err());
        Ok(())
    }

    #[test]
    fn plans_reject_forward_prerequisites_duplicate_topics_and_missing_links() -> Result<()> {
        let source = evidence("/project/a.py", "def receive(x): return finish(x)");
        let mut plan: Outline = serde_json::from_value(json!({
            "reader_goal":"Send input and understand the result", "storyline":"Prepare input, then interpret its result", "terminology":[],
            "sections":[
                {"title":"Prepare input","query":"receive","reader_question":"Which input is valid?","handoff":"The validated input can be submitted","diagrams":[],"depends_on":[],"evidence_ids":[&source.id[..8]]},
                {"title":"Interpret the result","query":"finish","reader_question":"What does the result mean?","handoff":"","diagrams":[],"depends_on":[0],"evidence_ids":[&source.id[..8]]}
            ]
        }))?;
        validate_outline(&mut plan, std::slice::from_ref(&source), Some(0))?;
        assert_eq!(plan.sections[0].evidence_ids, vec![source.id.clone()]);
        plan.sections[0].depends_on = vec![1];
        assert!(validate_outline(&mut plan, std::slice::from_ref(&source), None).is_err());
        plan.sections[0].depends_on.clear();
        plan.sections[0].handoff.clear();
        assert!(validate_outline(&mut plan, std::slice::from_ref(&source), None).is_err());
        plan.sections[0].handoff = "Valid input".into();
        plan.sections[1].reader_question = "  Which   input is VALID?  ".into();
        assert!(validate_outline(&mut plan, std::slice::from_ref(&source), None).is_err());
        plan.sections[1].reader_question = "What does the result mean?".into();
        plan.sections[1].evidence_ids.clear();
        assert!(validate_outline(&mut plan, &[source], None).is_err());
        Ok(())
    }

    #[test]
    fn outline_accepts_small_and_32_section_plans_but_rejects_33() -> Result<()> {
        let source = evidence("/project/a.py", "def process(): return 1");
        for count in [0, 1, 8, 9, 16, 32, 33] {
            let sections = (0..count)
                .map(|index| {
                    json!({
                        "title":format!("Topic {index}"),"query":"process",
                        "reader_question":format!("What is step {index}?"),
                        "handoff":if index + 1 < count {"Result for next step"} else {""},
                        "depends_on":if index > 0 {vec![index - 1]} else {vec![]},
                        "diagrams":[],"evidence_ids":[source.id]
                    })
                })
                .collect::<Vec<_>>();
            let mut plan: Outline = serde_json::from_value(json!({
                "reader_goal":"Understand processing","storyline":"Follow distinct steps",
                "terminology":[],"sections":sections
            }))?;
            let result = validate_outline(&mut plan, std::slice::from_ref(&source), Some(0));
            assert_eq!(
                result.is_ok(),
                (1..=32).contains(&count),
                "section count {count}: {result:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn requirement_ownership_rejects_missing_and_duplicate_coverage() -> Result<()> {
        let e = evidence("/project/a.py", "def process(): return 1");
        let mut plan: Outline = serde_json::from_value(
            json!({"reader_goal":"Understand result","storyline":"Prepare and inspect","requirements":[{"id":"r1","question":"What result?"}],"sections":[
                {"title":"Result","query":"process","reader_question":"What is returned?","handoff":"","diagrams":[],"evidence_ids":[e.id],"key_points":["Return value"],"owns_requirement_ids":[]}
            ]}),
        )?;
        assert!(validate_outline(&mut plan, std::slice::from_ref(&e), None).is_err());
        plan.sections[0].owns_requirement_ids = vec!["r1".into()];
        validate_outline(&mut plan, std::slice::from_ref(&e), None)?;
        plan.sections[0].owns_requirement_ids.push("r1".into());
        assert!(validate_outline(&mut plan, std::slice::from_ref(&e), None).is_err());
        Ok(())
    }

    #[test]
    fn source_budget_preserves_whole_passages_and_balances_searches() {
        let a = evidence("/project/a.py", &"준비".repeat(50));
        let b = evidence("/project/b.py", &"결과".repeat(50));
        let c = evidence("/project/c.py", &"오류".repeat(50));
        let size = a.content.len() + a.path.len() + 256;
        let result = pack_evidence(&[vec![a.clone(), c], vec![b.clone(), a.clone()]], size * 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, a.id);
        assert_eq!(result[1].id, b.id);
        assert_eq!(result[0].content, a.content);
        assert!(pack_evidence(&[vec![a]], size - 1).is_empty());
    }
}
