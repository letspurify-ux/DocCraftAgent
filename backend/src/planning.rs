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
use std::collections::{HashMap, HashSet};

const PLAN: &str = "Return JSON {sections:[{title:string,query:string,reader_question:string,handoff:string,diagrams:[string],prerequisite_titles:[string],evidence_ids:[string],key_points:[string],out_of_scope:[string]}],requirement_owners:[{requirement_id:string,section_title:string}],reader_goal:string,storyline:string,terminology:[string]}. Design one coherent document for the intended reader using the source_brief AND actual evidence read before this plan. Recorded uncertainties are research questions to resolve against newly supplied passages, not confirmed defects. question_analysis preserves findings and unresolved gaps separately for each required question; use it alongside the brief so a compressed overview does not erase a required branch or result contract. Excerpted question findings are navigation aids to the retained source anchors. The brief is an evidence-linked analysis, not independently verified truth: resolve contradictions against supplied implementation and respect its uncertainties. inventory_sample is only a navigation map: its paths may be named in section queries but are not evidence and must never appear in evidence_ids. Infer the audience and desired outcome from purpose. Choose 1-32 distinct sections in the order the reader needs to understand or perform the work. 32 is a hard ceiling, not a target. Choose the smallest section count that covers the requested scope clearly, based on reader goals, source-supported workflows, complexity and distinct reader questions. A narrow topic may need only 1-3 sections. Add a section only when it answers a substantial separate reader question; merge overlapping or thin topics and use subsections for supporting details. Do not create one section per file or module, pad the outline, or split a coherent workflow just to increase the count. Explain briefly in storyline why the chosen scope and grouping suit this document. Start with orientation and the relevant end-to-end picture, then introduce prerequisites before the actions that need them, show one normal path through to an observable result, and place alternatives/troubleshooting where they help the reader. Adapt the order to the actual source and purpose, not a fixed template or catalog of files/classes/subsystems. Separate reading order from runtime order: conditional branches and independent workflows must not become a fictional single execution trace. reader_goal states what the reader should achieve. storyline explains how the questions connect and why this order helps that goal. Each reader_question is one non-duplicated question this section resolves; handoff identifies the concrete result or decision the next section builds on (empty only for the final section). prerequisite_titles lists the exact titles of earlier sections needed to understand this section; use [] when none, always [] for the first section. These are reading prerequisites, not function calls. Use title strings, never section numbers, requirement IDs or evidence IDs. Do not return numeric depends_on; the caller resolves titles to indices. Finish the section order and unique titles before assigning prerequisites. If a required prerequisite appears later, move it before use and update storyline and handoffs consistently; do not discard a real prerequisite just to satisfy ordering. Do not list the current section or repeat a title. Every section must carry 1-8 supplied evidence_ids that anchor its topic. previously_read source_anchors are originals checked during earlier reading and can anchor an existing source_brief finding even if the passage is not repeated in this bounded request; they do not justify inventing new behavior. query names concrete implementation files, symbols and actions needed to deepen those anchors during writing. For cross-layer or end-to-end documentation, distribute queries across the relevant entry, orchestration, persistence, maintenance and result-consumer modules visible in inventory_sample instead of repeatedly relying on the same few files. For an end-to-end guide, include the evidenced entry, orchestration and result consumer in the opening section's anchors/query where available. Do not invent missing links to make the story smooth; explain limits or separate paths. Assign each explanation to one section to avoid repeated overviews. terminology contains at most 12 short, consistent definitions supported by evidence. Allocate diagrams across the WHOLE document, at most 4 per section: each diagrams entry is one plain-language objective, NEVER diagram code or an assumed call sequence. An empty array means no diagram. Name diagram types explicitly when purpose requests them. Respect max_diagrams (null means no numeric cap) and the requested global number/types; do not repeat an overall flow diagram in every section. Keep titles under 300 bytes, query under 2000 bytes, reader_question and handoff under 1500 bytes, reader_goal under 2000 bytes, storyline under 4000 bytes and each terminology entry under 500 bytes. Use the requested document language. Coverage is selective; never claim all code was understood.";

const OWNERSHIP_POLICY: &str = "Assign ownership once per REQUIRED QUESTION, not once per section. Return top-level requirement_owners:[{requirement_id:string,section_title:string}] with exactly one entry for each supplied requirement. Copy requirement_id exactly from requirements; section_title must exactly match one unique title in your final sections array. Choose the section whose reader_question and key_points provide the primary complete answer. Other sections may introduce, reference or expand that topic without also owning it. A section may own zero, one or several requirements: 32 sections do NOT need 32 requirements. Do not repeat an ID to give every section an owner, invent IDs, or delete required questions. Do not return per-section owns_requirement_ids; the caller derives it from requirement_owners. Include key_points (1-12 concrete explanations) and out_of_scope in each section. Both fields must be arrays of strings; use [] for out_of_scope when empty, never a scalar string or null. Align the owner's key_points with its required questions and preserve supported content and user feedback.";

pub(crate) const FINDING_KIND_POLICY: &str = "For each finding, kind must be exactly the JSON string \"runtime\" or \"context\". The word implementation describes source evidence, never a third finding kind. runtime_allowed is a boolean describing whether an evidence anchor can support a runtime finding; it is not the finding kind. Use runtime only with at least one supplied implementation anchor. Use context for declarations, documentation or test intent without asserting execution. Always include findings, uncertainties and followup_queries as arrays; use [] for empty lists, never null. Return one JSON object without Markdown fences.";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingKind {
    // Accept this common source-class label only as a runtime claim.
    // validate_brief must still require actual implementation evidence.
    #[serde(alias = "implementation")]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<crate::purpose::QuestionAnalysis>,
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
    let mut ownership: HashMap<String, (usize, String)> = HashMap::new();
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
                    outline.requirements.iter().any(|r| &r.id == requirement),
                    "sections[{index}] ({:?}).owns_requirement_ids contains unknown requirement {:?}; allowed IDs: {:?}. Requirement IDs are not section indices or evidence IDs",
                    section.title,
                    requirement,
                    outline
                        .requirements
                        .iter()
                        .map(|r| &r.id)
                        .collect::<Vec<_>>()
                );
                if let Some((owner_index, owner_title)) = ownership.get(requirement) {
                    bail!(
                        "Requirement {:?} is already owned by sections[{}] ({:?}) and repeated in sections[{}] ({:?}). Choose exactly one primary owner; other sections may discuss the topic with no ownership. Use requirement_owners once per required question",
                        requirement,
                        owner_index,
                        owner_title,
                        index,
                        section.title
                    );
                }
                ownership.insert(requirement.clone(), (index, section.title.clone()));
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
        for dependency in &section.depends_on {
            ensure!(
                *dependency < index,
                "sections[{index}] ({:?}).depends_on={:?}: index {dependency} is {}; only indices below {index} are allowed (the first section must use []). Use prerequisite_titles with exact earlier titles when generating an outline. Move a real prerequisite before use and update handoffs/storyline; do not guess a different index",
                section.title,
                section.depends_on,
                if *dependency >= count {
                    "outside the section array"
                } else if *dependency == index {
                    "a self-reference"
                } else {
                    "a forward reference"
                }
            );
            ensure!(
                dependencies.insert(*dependency),
                "sections[{index}] ({:?}).depends_on={:?}: repeated index {dependency}; include each prerequisite only once",
                section.title,
                section.depends_on
            );
        }
        ensure!(
            section
                .diagrams
                .as_ref()
                .is_some_and(|d| d.len() <= 4 && d.iter().all(|s| bounded_text(s, 1500))),
            "Every section needs a bounded diagrams array"
        );
        resolve_ids(&mut section.evidence_ids, evidence, 8)?;
    }
    let missing: Vec<_> = outline
        .requirements
        .iter()
        .filter(|r| !ownership.contains_key(&r.id))
        .map(|r| &r.id)
        .collect();
    ensure!(
        missing.is_empty(),
        "Missing required topic ownership for {:?}; assign each to one section that answers the supplied question. Other sections may have no owned requirements",
        missing
    );
    if let Some(error) = outline_diagram_error(outline, maximum) {
        bail!("{error}");
    }
    Ok(())
}

/// Resolve generated title references locally. Numeric dependencies remain supported
/// for old providers/checkpoints; never guess whether their numbering is one-based.
fn decode_generated_outline(
    response: &str,
    repair: &mut llm::JsonRepair,
    requirements: &[Requirement],
) -> Result<Outline> {
    let mut value: serde_json::Value = repair.decode(response)?;
    if let Some(sections) = value.get_mut("sections").and_then(|v| v.as_array_mut()) {
        let titles: Vec<Option<String>> = sections
            .iter()
            .map(|s| s.get("title").and_then(|v| v.as_str()).map(str::to_owned))
            .collect();
        for (index, section) in sections.iter_mut().enumerate() {
            if let Some(raw) = section.get("prerequisite_titles") {
                let references: Vec<String> = serde_json::from_value(raw.clone())
                    .with_context(|| format!("sections[{index}].prerequisite_titles must be an array of exact title strings; use [] when empty"))?;
                let mut dependencies = vec![];
                for title in references {
                    let matches: Vec<_> = titles
                        .iter()
                        .enumerate()
                        .filter_map(|(i, candidate)| {
                            (candidate.as_deref() == Some(title.as_str())).then_some(i)
                        })
                        .collect();
                    ensure!(
                        matches.len() == 1,
                        "sections[{index}] ({:?}).prerequisite_titles: {:?} matches {} sections; use an exact unique section title. Available earlier titles: {:?}",
                        titles[index],
                        title,
                        matches.len(),
                        &titles[..index]
                    );
                    let dependency = matches[0];
                    ensure!(
                        dependency < index,
                        "sections[{index}] ({:?}).prerequisite_titles: {:?} refers to sections[{dependency}], {}; available earlier titles: {:?}. Move a real prerequisite before use and update storyline and handoffs; do not drop it or guess another title",
                        titles[index],
                        title,
                        if dependency == index {
                            "the current section"
                        } else {
                            "a later section"
                        },
                        &titles[..index]
                    );
                    if !dependencies.contains(&dependency) {
                        dependencies.push(dependency);
                    }
                }
                // Explicit named references are authoritative in the generation contract.
                section["depends_on"] = json!(dependencies);
            }
        }
    }
    resolve_generated_ownership(&mut value, requirements)?;
    let mut plan: Outline = llm::decode(&serde_json::to_string(&value)?)?;
    // Repetition carries no additional meaning; keep the first occurrence.
    for section in &mut plan.sections {
        let mut seen = HashSet::new();
        section.depends_on.retain(|d| seen.insert(*d));
        let mut owned = HashSet::new();
        section
            .owns_requirement_ids
            .retain(|id| owned.insert(id.clone()));
    }
    Ok(plan)
}

/// A single owner table avoids asking the model to coordinate duplicate ID lists
/// across up to 32 independently described sections. Resolve only explicit choices.
fn resolve_generated_ownership(
    value: &mut serde_json::Value,
    requirements: &[Requirement],
) -> Result<()> {
    value
        .as_object_mut()
        .context("Outline response must be a JSON object")?
        .insert("requirements".into(), json!(requirements));
    let Some(raw) = value.get("requirement_owners") else {
        return Ok(());
    };
    #[derive(Deserialize)]
    struct Owner {
        requirement_id: String,
        section_title: String,
    }
    let owners: Vec<Owner> = llm::decode(&raw.to_string()).context(
        "requirement_owners must be an array of {requirement_id:string,section_title:string}",
    )?;
    let sections = value
        .get_mut("sections")
        .and_then(|v| v.as_array_mut())
        .context("sections must be an array before assigning requirement_owners")?;
    let mut assignments: HashMap<String, usize> = HashMap::new();
    for owner in owners {
        ensure!(
            requirements.iter().any(|r| r.id == owner.requirement_id),
            "requirement_owners contains unknown requirement {:?}; allowed IDs: {:?}",
            owner.requirement_id,
            requirements.iter().map(|r| &r.id).collect::<Vec<_>>()
        );
        let matches: Vec<_> = sections
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                (s.get("title").and_then(|v| v.as_str()) == Some(owner.section_title.as_str()))
                    .then_some(i)
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "requirement_owners for {:?}: section title {:?} matches {} sections; use an exact unique title from sections",
            owner.requirement_id,
            owner.section_title,
            matches.len()
        );
        let target = matches[0];
        if let Some(prior) = assignments.insert(owner.requirement_id.clone(), target) {
            ensure!(
                prior == target,
                "requirement_owners assigns {:?} to both sections[{}] ({}) and sections[{}] ({}); choose exactly one primary owner. Other sections may reference the topic without owning it",
                owner.requirement_id,
                prior,
                sections[prior]["title"],
                target,
                sections[target]["title"]
            );
        }
    }
    let missing: Vec<_> = requirements
        .iter()
        .filter(|r| !assignments.contains_key(&r.id))
        .map(|r| &r.id)
        .collect();
    ensure!(
        missing.is_empty(),
        "requirement_owners is missing {:?}; assign each supplied question to one primary section without removing requirements",
        missing
    );
    for (index, section) in sections.iter_mut().enumerate() {
        // Use canonical requirement order so equivalent tables have stable checkpoints.
        section
            .as_object_mut()
            .with_context(|| format!("sections[{index}] must be a JSON object"))?
            .insert(
                "owns_requirement_ids".into(),
                json!(
                    requirements
                        .iter()
                        .filter(|r| assignments.get(&r.id) == Some(&index))
                        .map(|r| &r.id)
                        .collect::<Vec<_>>()
                ),
            );
    }
    Ok(())
}

/// Include every section in repair context even when the full response is excerpted.
fn dependency_repair_context(response: &str) -> serde_json::Value {
    let Ok(value) = llm::decode::<serde_json::Value>(response) else {
        return json!([]);
    };
    json!(
        value
            .get("sections")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .take(32)
            .enumerate()
            .map(|(index, s)| json!({"index":index,"title":s.get("title"),
            "prerequisite_titles":s.get("prerequisite_titles"),"depends_on":s.get("depends_on"),"owns_requirement_ids":s.get("owns_requirement_ids")}))
            .collect::<Vec<_>>()
    )
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

pub(crate) fn evidence_budget(ctx: &RunContext) -> usize {
    let l = &ctx.snapshot.settings.llm;
    let available = (l.context_limit.min(l.model_context_limit).min(200_000) as usize)
        .saturating_mul(100usize.saturating_sub(l.safety_percent as usize))
        / 100;
    available
        .saturating_sub(l.max_output_tokens as usize + ctx.snapshot.task.direction.len() + 32_000)
        .min(48_000)
}

pub async fn outline(ctx: &RunContext, system: &str) -> Result<Outline> {
    crate::purpose::prepare(ctx).await?;
    // Old runs retain their outline and numbered drafts. New runs always discover
    // first; no file-name-only fallback is allowed after discovery failures.
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "outline").await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let whole = crate::understanding::analyze(ctx, system).await?;
    let requirements = requirements(ctx, system).await?;
    let discovery = crate::purpose::analyze(ctx, system, &whole, &requirements).await?;
    let inventory = editorial::excerpt(&serde_json::to_string(&whole.brief)?, 8000);
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
            let evidence = crate::purpose::pack(&discovery, limit);
            ensure!(
                !evidence.is_empty(),
                "CONTEXT_BUDGET: insufficient room for grounded outline"
            );
            let brief = &discovery.brief;
            ctx.event("outline_planning", json!({"stage":"planning","title":"구현 근거에 맞춰 설명 순서 구성","attempt":attempt+1,"evidence_chunks":evidence.len()})).await?;
            let mut input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "source_brief":brief,"question_analysis":crate::purpose::context(&discovery),"source_anchors":discovery.evidence.iter().map(|e| json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),"project_overview":inventory,"requirements":requirements,"feedback":feedback,"revision":revision,
            "evidence":evidence,"max_diagrams":ctx.snapshot.task.max_diagrams,
            "previous_error":previous_error,"attempt":attempt+1,"instruction":format!("{PLAN} {OWNERSHIP_POLICY} Respect user feedback and preserve valid existing section IDs when supplied. Do not remove requirements to hide missing coverage.")});
            input["dependency_example"] = json!({"description":"Shape example only; choose titles and prerequisites from the actual document",
                "sections":[{"title":"Prepare input","prerequisite_titles":[]},
                    {"title":"Process input","prerequisite_titles":["Prepare input"]},
                    {"title":"Read results","prerequisite_titles":["Process input"]}]});
            repair.apply(&mut input);
            if let Some(response) = &repair.response {
                input["previous_section_dependencies"] = dependency_repair_context(response);
                if let Ok(value) = llm::decode::<serde_json::Value>(response) {
                    input["previous_requirement_owners"] = value
                        .get("requirement_owners")
                        .cloned()
                        .unwrap_or(json!([]));
                }
            }
            let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
                let mut plan = decode_generated_outline(&s, &mut repair, &requirements)?;
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
        discovery.evidence = crate::purpose::merge_evidence(&extra);
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
    let key = format!(
        "document_requirements:{}",
        crate::purpose::intent_key(&ctx.snapshot.task)
    );
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? {
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
                db::checkpoint(&ctx.pool, &ctx.id, &key, &serde_json::to_value(&r)?).await?;
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
        let mut input = json!({"phase":"outline_review","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,"outline":plan,"source_brief":discovery.brief,"question_analysis":crate::purpose::context(discovery),"evidence":crate::purpose::pack(discovery,evidence_budget(ctx)),"source_anchors":discovery.evidence.iter().map(|e| json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),"attempt":attempt,"previous_error":error,"instruction":"Review this outline BEFORE writing. Return JSON {issues:[{severity:'major'|'minor',code:string,message:string,section_ids:[string],requirement_ids:[string],query:string}]}. Recorded uncertainties are research questions; newly supplied implementation may resolve them. Use question_analysis to check each required question and its unresolved gaps; an omitted excerpt is not proof that the implementation is missing. Check missing reader requirements, semantic overlap, prerequisites after use, oversized or empty sections, audience mismatch, unsupported runtime ordering and missing important source branches. A short orientation referencing a detailed section is valid. Sharing evidence is not duplication. Supporting sections may discuss a required question without owning it; a section with no owned requirements is valid. Check the primary owner against its reader_question and key_points. Every issue must identify concrete affected IDs and a necessary correction, grounded in the supplied outline or source. For missing evidence query names observed files/symbols. Previously_read anchors support observations already checked in the source_brief. Do not invent defects or infer absence from an excerpt. Empty issues means no concrete defect supported. Use the requested language. At most 12 issues."});
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
    let mut evidence: Vec<_> = discovery
        .evidence
        .iter()
        .filter(|e| plan.evidence_ids.contains(&e.id))
        .cloned()
        .collect();
    ensure!(
        evidence.len() == plan.evidence_ids.len(),
        "Missing planned source evidence"
    );
    let question_ids: HashSet<_> = discovery
        .questions
        .iter()
        .filter(|q| plan.owns_requirement_ids.contains(&q.requirement_id))
        .flat_map(|q| q.brief.findings.iter().flat_map(|f| &f.evidence_ids))
        .collect();
    let extra = discovery
        .evidence
        .iter()
        .filter(|e| question_ids.contains(&e.id))
        .cloned()
        .collect();
    evidence = crate::purpose::merge_evidence(&[evidence, extra]);
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
    fn implementation_alias_is_runtime_and_still_requires_implementation_evidence() -> Result<()> {
        let implementation = evidence("/project/main.rs", "fn main() {}");
        let xml = evidence("/project/mapper.xml", "<mapper />");
        for (source, valid) in [(implementation, true), (xml, false)] {
            let mut brief: SourceBrief = llm::decode(
                &json!({
                    "findings":[{"topic":"entry","observation":"A runtime claim",
                        "kind":"implementation","evidence_ids":[source.id]}],
                    "uncertainties":[],"followup_queries":[]
                })
                .to_string(),
            )?;
            assert!(matches!(brief.findings[0].kind, FindingKind::Runtime));
            assert_eq!(
                serde_json::to_value(&brief)?["findings"][0]["kind"],
                "runtime"
            );
            assert_eq!(validate_brief(&mut brief, &[source], true).is_ok(), valid);
        }
        assert!(llm::decode::<FindingKind>(r#""unknown""#).is_err());
        Ok(())
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

    fn generated_plan(count: usize) -> serde_json::Value {
        json!({"reader_goal":"Understand the workflow","storyline":"Prepare, process, inspect",
            "terminology":[],"sections":(0..count).map(|index| json!({
                "title":format!("단계 {}", index + 1),"query":"process",
                "reader_question":format!("What does step {} do?",index + 1),
                "handoff":if index + 1 < count {"Use the result in the next step"} else {""},
                "prerequisite_titles":if index > 0 {vec![format!("단계 {index}")]} else {vec![]},
                "diagrams":[],"evidence_ids":[]
            })).collect::<Vec<_>>()})
    }

    #[test]
    fn generated_title_prerequisites_resolve_through_32_sections() -> Result<()> {
        let source = evidence("/project/main.py", "def process(): return 1");
        let mut value = generated_plan(32);
        for section in value["sections"].as_array_mut().unwrap() {
            section["evidence_ids"] = json!([source.id]);
        }
        value["sections"][31]["prerequisite_titles"] = json!(["단계 1", "단계 31", "단계 1"]);
        let mut repair = llm::JsonRepair::default();
        let mut plan = decode_generated_outline(&value.to_string(), &mut repair, &[])?;
        assert!(plan.sections[0].depends_on.is_empty());
        assert_eq!(plan.sections[1].depends_on, vec![0]);
        assert_eq!(plan.sections[30].depends_on, vec![29]);
        assert_eq!(plan.sections[31].depends_on, vec![0, 30]);
        validate_outline(&mut plan, &[source], Some(0))?;
        let stored = serde_json::to_value(&plan)?;
        assert!(stored["sections"][31].get("prerequisite_titles").is_none());
        assert_eq!(stored["sections"][31]["depends_on"], json!([0, 30]));
        Ok(())
    }

    #[test]
    fn generated_dependencies_report_bad_references_without_guessing() -> Result<()> {
        for (index, titles, expected) in [
            (0, json!(["단계 1"]), "the current section"),
            (0, json!(["단계 2"]), "a later section"),
            (1, json!(["missing"]), "matches 0 sections"),
            (1, json!([0]), "array of exact title strings"),
            (1, json!(null), "array of exact title strings"),
        ] {
            let mut value = generated_plan(3);
            value["sections"][index]["prerequisite_titles"] = titles;
            let result =
                decode_generated_outline(&value.to_string(), &mut llm::JsonRepair::default(), &[]);
            let error = result
                .err()
                .context("Invalid dependency unexpectedly accepted")?
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(error.contains(&format!("sections[{index}]")), "{error}");
        }
        let mut duplicate = generated_plan(3);
        duplicate["sections"][1]["title"] = json!("단계 1");
        duplicate["sections"][1]["prerequisite_titles"] = json!([]);
        duplicate["sections"][2]["prerequisite_titles"] = json!(["단계 1"]);
        let error =
            decode_generated_outline(&duplicate.to_string(), &mut llm::JsonRepair::default(), &[])
                .err()
                .context("Ambiguous titles unexpectedly accepted")?;
        assert!(error.to_string().contains("matches 2 sections"));
        Ok(())
    }

    #[test]
    fn legacy_generated_dependencies_deduplicate_but_do_not_shift_numbering() -> Result<()> {
        let source = evidence("/project/main.py", "def process(): return 1");
        let mut value = generated_plan(3);
        for section in value["sections"].as_array_mut().unwrap() {
            section
                .as_object_mut()
                .unwrap()
                .remove("prerequisite_titles");
            section["evidence_ids"] = json!([source.id]);
        }
        value["sections"][1]["depends_on"] = json!([0, 0]);
        let mut plan =
            decode_generated_outline(&value.to_string(), &mut llm::JsonRepair::default(), &[])?;
        assert_eq!(plan.sections[1].depends_on, vec![0]);
        validate_outline(&mut plan, std::slice::from_ref(&source), Some(0))?;
        for (reference, reason) in [
            (1, "a self-reference"),
            (2, "a forward reference"),
            (3, "outside the section array"),
        ] {
            plan.sections[1].depends_on = vec![reference];
            let error = validate_outline(&mut plan, std::slice::from_ref(&source), Some(0))
                .unwrap_err()
                .to_string();
            assert!(error.contains("sections[1]"), "{error}");
            assert!(error.contains(reason), "{error}");
        }
        Ok(())
    }

    #[test]
    fn dependency_repair_includes_middle_sections_of_long_responses() {
        let mut value = generated_plan(32);
        for section in value["sections"].as_array_mut().unwrap() {
            section["key_points"] = json!(["detail".repeat(1000)]);
        }
        let context = dependency_repair_context(&value.to_string());
        assert_eq!(context.as_array().unwrap().len(), 32);
        assert_eq!(context[16]["title"], "단계 17");
        assert_eq!(context[16]["prerequisite_titles"], json!(["단계 16"]));
        assert!(context[16].get("key_points").is_none());
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

    fn ownership_requirements() -> Vec<Requirement> {
        vec![
            Requirement {
                id: "r1".into(),
                question: "How is input validated?".into(),
            },
            Requirement {
                id: "r2".into(),
                question: "How is the result returned?".into(),
            },
        ]
    }

    #[test]
    fn generated_ownership_supports_many_sections_and_multiple_questions_per_owner() -> Result<()> {
        let source = evidence("/project/a.py", "def process(): return 1");
        let requirements = ownership_requirements();
        for count in [1, 32] {
            let mut value = generated_plan(count);
            for section in value["sections"].as_array_mut().unwrap() {
                section["evidence_ids"] = json!([source.id]);
                section["key_points"] = json!(["Validate input and return the result"]);
                // A named owner table is authoritative over stale per-section lists.
                section["owns_requirement_ids"] = json!(["r1", "r2"]);
            }
            value["requirements"] = json!([]); // The response cannot remove supplied questions.
            let owner = format!("단계 {count}");
            value["requirement_owners"] = json!([
                {"requirement_id":"r2","section_title":owner},
                {"requirement_id":"r1","section_title":owner},
                {"requirement_id":"r1","section_title":owner}
            ]);
            let mut plan = decode_generated_outline(
                &value.to_string(),
                &mut llm::JsonRepair::default(),
                &requirements,
            )?;
            assert_eq!(plan.requirements.len(), 2);
            assert_eq!(
                plan.sections[count - 1].owns_requirement_ids,
                vec!["r1", "r2"]
            );
            assert!(
                plan.sections[..count - 1]
                    .iter()
                    .all(|s| s.owns_requirement_ids.is_empty())
            );
            validate_outline(&mut plan, std::slice::from_ref(&source), Some(0))?;
        }
        Ok(())
    }

    #[test]
    fn generated_owner_conflicts_unknown_ids_missing_questions_and_titles_are_explicit()
    -> Result<()> {
        let requirements = ownership_requirements();
        for (owners, expected) in [
            (
                json!([
                    {"requirement_id":"r1","section_title":"단계 1"},
                    {"requirement_id":"r1","section_title":"단계 2"},
                    {"requirement_id":"r2","section_title":"단계 2"}
                ]),
                "assigns \"r1\" to both sections[0]",
            ),
            (
                json!([{"requirement_id":"r9","section_title":"단계 1"}]),
                "unknown requirement \"r9\"",
            ),
            (
                json!([{"requirement_id":"r1","section_title":"단계 1"}]),
                "missing [\"r2\"]",
            ),
            (
                json!([{"requirement_id":"r1","section_title":"unknown"}]),
                "matches 0 sections",
            ),
            (json!(null), "requirement_owners must be an array"),
        ] {
            let mut value = generated_plan(2);
            value["requirement_owners"] = owners;
            let error = decode_generated_outline(
                &value.to_string(),
                &mut llm::JsonRepair::default(),
                &requirements,
            )
            .err()
            .context("Invalid ownership unexpectedly accepted")?
            .to_string();
            assert!(error.contains(expected), "{error}");
        }
        for malformed in [
            json!([]),
            json!(null),
            json!({"sections":[null],"requirement_owners":[]}),
        ] {
            assert!(
                decode_generated_outline(
                    &malformed.to_string(),
                    &mut llm::JsonRepair::default(),
                    &[]
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn legacy_owner_repetition_is_normalized_only_within_a_section() -> Result<()> {
        let source = evidence("/project/a.py", "def process(): return 1");
        let requirements = ownership_requirements();
        let mut value = generated_plan(2);
        for section in value["sections"].as_array_mut().unwrap() {
            section["evidence_ids"] = json!([source.id]);
            section["key_points"] = json!(["Explain the required question"]);
        }
        value["sections"][0]["owns_requirement_ids"] = json!(["r1", "r1"]);
        value["sections"][1]["owns_requirement_ids"] = json!(["r2"]);
        let mut plan = decode_generated_outline(
            &value.to_string(),
            &mut llm::JsonRepair::default(),
            &requirements,
        )?;
        validate_outline(&mut plan, std::slice::from_ref(&source), Some(0))?;
        plan.sections[1].owns_requirement_ids.push("r1".into());
        let error = validate_outline(&mut plan, std::slice::from_ref(&source), Some(0))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("r1") && error.contains("sections[0]") && error.contains("sections[1]"),
            "{error}"
        );
        plan.sections[1].owns_requirement_ids = vec!["unknown".into()];
        let error = validate_outline(&mut plan, &[source], Some(0))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unknown requirement") && error.contains("sections[1]"),
            "{error}"
        );
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
