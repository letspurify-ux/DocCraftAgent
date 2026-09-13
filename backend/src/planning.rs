//! Read implementation before choosing the reader's journey. Discovery is bounded
//! and checkpointed independently of the final outline and section drafts.
use crate::{
    db, editorial, llm,
    model::{Evidence, Outline, SectionPlan},
    runner::{RunContext, fatal, is_budget, outline_diagram_error},
    source,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;

const DISCOVERY: &str = "Read source evidence before planning the document. Return ONLY JSON {findings:[{topic:string,observation:string,kind:'runtime'|'context',evidence_ids:[string]}],uncertainties:[string],followup_queries:[string]}. Do not produce a table of contents yet. Infer the intended reader and task from purpose, then read actual source passages to identify the relevant entry, prerequisites, actors, inputs, processing, persisted or returned results, consumer, and important alternative/error paths. Adapt to the supplied project; do not force a web request model onto unrelated code. Each finding must explain a concrete connection or behavior, including conditions and outputs, rather than list symbols. Use at most 12 findings, each with a short topic and observation (maximum 1200 characters), and 1-6 supplied evidence IDs. Runtime findings require implementation evidence; filenames, imports, README, comments and tests alone do not prove execution. Context findings can describe documented setup or intended usage, explicitly distinguished from observed implementation. Inventory is only a sampled navigation aid. Do not infer a call order from names or treat separate alternatives as consecutive steps. Mark missing links in uncertainties (at most 8). Request at most 3 focused followup_queries naming observed files/symbols or unresolved connections most important to the reader; prefer finding missing entry/result/branch evidence over more detail on already understood helpers. Do not invent identifiers. On the final pass, return no followup_queries and retain unresolved links in uncertainties. Rebuild findings from the evidence in THIS request; prior gaps are research questions, not facts. Use the requested language for observations and uncertainties. Empty findings are invalid; if only contextual evidence exists, say so without inventing runtime behavior.";

const PLAN: &str = "Return JSON {sections:[{title:string,query:string,reader_question:string,handoff:string,diagrams:[string],depends_on:[number],evidence_ids:[string]}],reader_goal:string,storyline:string,terminology:[string]}. Design one coherent document for the intended reader using the source_brief AND actual evidence read before this plan. The brief is an evidence-linked analysis, not independently verified truth: resolve contradictions against supplied implementation and respect its uncertainties. inventory_sample is only a navigation map: its paths may be named in section queries but are not evidence and must never appear in evidence_ids. Infer the audience and desired outcome from purpose. Organize 3-8 distinct sections in the order the reader needs to understand or perform the work; a narrow topic may need fewer. Start with orientation and the relevant end-to-end picture, then introduce prerequisites before the actions that need them, show one normal path through to an observable result, and place alternatives/troubleshooting where they help the reader. Adapt the order to the actual source and purpose, not a fixed template or catalog of files/classes/subsystems. Separate reading order from runtime order: conditional branches and independent workflows must not become a fictional single execution trace. reader_goal states what the reader should achieve. storyline explains how the questions connect and why this order helps that goal. Each reader_question is one non-duplicated question this section resolves; handoff identifies the concrete result or decision the next section builds on (empty only for the final section). depends_on lists only earlier zero-based SECTION indices needed to understand this section; it is a reading prerequisite, not a function call graph. Every section must carry 1-8 supplied evidence_ids that anchor its topic. query names concrete implementation files, symbols and actions needed to deepen those anchors during writing. For cross-layer or end-to-end documentation, distribute queries across the relevant entry, orchestration, persistence, maintenance and result-consumer modules visible in inventory_sample instead of repeatedly relying on the same few files. For an end-to-end guide, include the evidenced entry, orchestration and result consumer in the opening section's anchors/query where available. Do not invent missing links to make the story smooth; explain limits or separate paths. Assign each explanation to one section to avoid repeated overviews. terminology contains at most 12 short, consistent definitions supported by evidence. Allocate diagrams across the WHOLE document, at most 4 per section: each diagrams entry is one plain-language objective, NEVER diagram code or an assumed call sequence. An empty array means no diagram. Name diagram types explicitly when purpose requests them. Respect max_diagrams (null means no numeric cap) and the requested global number/types; do not repeat an overall flow diagram in every section. Keep titles under 300 bytes, query under 2000 bytes, reader_question and handoff under 1500 bytes, reader_goal under 2000 bytes, storyline under 4000 bytes and each terminology entry under 500 bytes. Use the requested document language. Coverage is selective; never claim all code was understood.";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FindingKind {
    Runtime,
    Context,
}

#[derive(Clone, Serialize, Deserialize)]
struct Finding {
    topic: String,
    observation: String,
    kind: FindingKind,
    evidence_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct SourceBrief {
    findings: Vec<Finding>,
    uncertainties: Vec<String>,
    followup_queries: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Discovery {
    brief: SourceBrief,
    evidence: Vec<Evidence>,
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

fn validate_brief(brief: &mut SourceBrief, evidence: &[Evidence], final_pass: bool) -> Result<()> {
    ensure!(
        !brief.findings.is_empty() && brief.findings.len() <= 12,
        "Supply 1-12 source findings"
    );
    ensure!(
        brief.uncertainties.len() <= 8 && brief.uncertainties.iter().all(|s| bounded_text(s, 1500)),
        "Invalid uncertainties"
    );
    ensure!(
        brief.followup_queries.len() <= 3
            && brief.followup_queries.iter().all(|s| bounded_text(s, 1500)),
        "Invalid followup_queries"
    );
    ensure!(
        !final_pass || brief.followup_queries.is_empty(),
        "Final source reading must leave unresolved links in uncertainties, not request another pass"
    );
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
                "Runtime finding must cite implementation evidence"
            );
        }
    }
    Ok(())
}

fn validate_outline(
    outline: &mut Outline,
    evidence: &[Evidence],
    maximum: Option<u32>,
) -> Result<()> {
    ensure!(
        !outline.sections.is_empty() && outline.sections.len() <= 8,
        "Supply 1-8 sections"
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
    let count = outline.sections.len();
    for (index, section) in outline.sections.iter_mut().enumerate() {
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
    if let Some(error) = outline_diagram_error(outline, maximum) {
        bail!("{error}");
    }
    Ok(())
}

/// Keep real passages intact (and their hashes valid), distributing space across
/// independent retrieval queries before taking more results from any one query.
fn pack_evidence(groups: &[Vec<Evidence>], limit: usize) -> Vec<Evidence> {
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
    let mut limit = evidence_budget(ctx);
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
        let input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "inventory_sample":editorial::excerpt(inventory, 16000 >> attempt),"evidence":evidence,
            "open_questions":previous.map(|p| &p.brief.uncertainties),"final_pass":final_pass,
            "previous_error":previous_error,"attempt":attempt+1,"instruction":DISCOVERY});
        let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let mut brief: SourceBrief = llm::decode(&s)?;
            validate_brief(&mut brief, &evidence, final_pass)?;
            Ok(brief)
        });
        match result {
            Ok(brief) => return Ok(Discovery { brief, evidence }),
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                previous_error = editorial::excerpt(&e.to_string(), 1500);
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
    bail!("Unable to understand source before planning: {previous_error}")
}

async fn discover(ctx: &RunContext, system: &str, inventory: &str) -> Result<Discovery> {
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "source_understanding").await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let first: Discovery =
        if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "source_reading:0").await? {
            serde_json::from_value(saved)?
        } else {
            let read = read_sources(ctx, system, inventory, None).await?;
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
        read_sources(ctx, system, inventory, Some(&first)).await?
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
    let inventory = source::inventory(ctx).await?;
    let discovery = discover(ctx, system, &inventory).await?;
    let mut limit = evidence_budget(ctx);
    let mut previous_error = String::new();
    for attempt in 0..3 {
        let evidence = pack_evidence(std::slice::from_ref(&discovery.evidence), limit);
        ensure!(
            !evidence.is_empty(),
            "CONTEXT_BUDGET: insufficient room for grounded outline"
        );
        // A reduced request must not retain findings whose source was dropped.
        let mut brief = discovery.brief.clone();
        let all_present = |f: &Finding| {
            f.evidence_ids
                .iter()
                .all(|id| evidence.iter().any(|e| &e.id == id))
        };
        brief.findings.retain(all_present);
        if brief.findings.len() != discovery.brief.findings.len() {
            brief.uncertainties.push("Some source findings were omitted to fit this request; do not infer their behavior.".into());
        }
        ctx.event("outline_planning", json!({"stage":"planning","title":"구현 근거에 맞춰 설명 순서 구성","attempt":attempt+1,"evidence_chunks":evidence.len()})).await?;
        let input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "source_brief":brief,"inventory_sample":editorial::excerpt(&inventory, 8000 >> attempt),
            "evidence":evidence,"max_diagrams":ctx.snapshot.task.max_diagrams,
            "previous_error":previous_error,"attempt":attempt+1,"instruction":PLAN});
        let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let mut plan: Outline = llm::decode(&s)?;
            validate_outline(&mut plan, &evidence, ctx.snapshot.task.max_diagrams)?;
            Ok(plan)
        });
        match result {
            Ok(plan) => {
                db::checkpoint(&ctx.pool, &ctx.id, "outline", &serde_json::to_value(&plan)?)
                    .await?;
                return Ok(plan);
            }
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                previous_error = editorial::excerpt(&e.to_string(), 1500);
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
    bail!(
        "Unable to generate a grounded documentation outline after three attempts: {previous_error}"
    )
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
            "uncertainties":["finish implementation is missing"],"followup_queries":["finish"]
        }))?;
        let sources = vec![implementation.clone(), readme.clone()];
        validate_brief(&mut brief, &sources, false)?;
        assert_eq!(brief.findings[0].evidence_ids[0], implementation.id);
        assert!(validate_brief(&mut brief, &sources, true).is_err());
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
