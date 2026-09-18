//! Read implementation before choosing the reader's journey. Discovery is bounded
//! and checkpointed independently of the final outline and section drafts.
use crate::{
    db, editorial, llm,
    model::{BranchExclusion, Evidence, Outline, OutlineReview, SectionPlan},
    runner::{RunContext, fatal, is_budget, outline_diagram_error},
    source,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};

const PLAN_TEMPLATE: &str = "Return JSON {sections:[{title:string,key_points:[string],query:string,evidence_ids:[string],diagrams:[string],branches:[string]}],excluded_branches:[{branch:string,reason:string}],reader_goal:string,storyline:string}. Organize the code into clear sections and summarize its important behavior according to purpose. Choose the smallest number of sections that serves the purpose. The three views of the source are one thing at three depths, not three lists to merge: source_brief is the through-line across the whole project, source_branches are the parts that through-line runs through - each with `where` it lives, the topics it covers and how many files it holds - and supporting_findings are earlier observations that may belong to any of them. A topic appearing in more than one view is the same topic seen from further away, so say it once, in the section whose branch owns it. The document's spine is source_brief's through-line, ordered the way a reader should meet it; the branches supply the parts that sit under it, and a section belongs to the through-line finding it serves. source_branches arrive in the order the source was read, which follows how the code is laid out: useful for ordering sections within one theme, but a file layout is not a reader's journey and must not become the document's structure. source_branches is what the source contains, not what this document owes a section. Judge each branch against the purpose: a branch the purpose does not ask about gets no section, however many files it holds. Account for every branch by its label: name it in the branches of each section that covers it, or list it once in excluded_branches with a short reason tied to the purpose; a branch left in neither reads as something the document forgot. When a large branch lists parts and several sections share it, give each section the parts it explains (labels like B3.2) instead of the whole branch, and account for every part the same way, so no part is left for nobody. What the branches do change is the ceiling: where the purpose does reach many branches, give a branch with more relevant topics more sections than one with few, and do not compress a large relevant branch into one section because a smaller number looks tidier. A large source with a narrow purpose is a short document, and that is the correct answer rather than a failure to fill the room. Group related responsibilities and workflows, merging thin or overlapping topics. Smallest does not mean fewest at any cost: a section a reader follows as one explanation covers one workflow, so when the purpose needs several branches explained, give branches that serve different workflows their own sections rather than one section that lists them. Do not force one section per file or impose a fixed template. Use an order that makes the actual code easy to follow: establish needed context before explaining processing, outputs and important alternative/error paths. Respect the user's explicit audience, scope, section count and diagram instructions. reader_goal briefly states what the document explains; storyline briefly explains the grouping and order. Each section needs a unique title, 1-12 concrete key_points, a query naming observed files/symbols for deeper reading, branches naming the source_branches labels - whole branches or their parts - it covers ([] when source_branches is empty), 0-{MAX_SECTION_ANCHORS} supplied evidence_ids where its reading should start - at least one when branches is empty, and [] rather than a passage from another part of the source when its branches carry it - and diagrams as an array of diagram objectives (use [] if none). Share source evidence across sections when useful, but avoid repeating the same explanation. source_brief and supporting_findings contain previously checked observations, with uncertainties; use original evidence to resolve contradictions or add connections. Previously_read source anchors may support those existing observations when the original is omitted from this request. Missing excerpts and old uncertainties do not prove absent implementation. Do not invent runtime order, join independent workflows, or turn conditional paths into an unconditional sequence. Outline descriptions guide later writing and are not proof of execution. Keep titles under 300 UTF-8 bytes, each key point under 1500 bytes, query under 2000 bytes, reader_goal under 2000 bytes and storyline under 4000 bytes. Allocate at most 4 diagrams per section and respect max_diagrams across the whole document; do not repeat the overall diagram in each section. Use the requested language. Do not generate reader questions, requirement IDs, ownership tables or mandatory handoffs. Detailed transitions belong in the section prose.";
pub(crate) static PLAN: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| with_limits(PLAN_TEMPLATE));

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
    pub details: Vec<Finding>,
    #[serde(default)]
    pub validation_unresolved: bool,
}

/// A brief carries at most `MAX_FINDINGS` findings of at most
/// `MAX_EVIDENCE_IDS` citations, so one brief can name at most their product in
/// distinct passages. Callers that must have every supplied passage cited size
/// their requests against that ceiling.
///
/// The citation cap was six, and the readings that hit it were not wrong: a
/// leaf is told to stay inside its byte budget "by grouping related passages
/// under one observation and citing all of their IDs together", and a finding
/// about a fixture set or a migration directory genuinely rests on seven or
/// eight passages. Rejecting those cost a whole retry and bought nothing, since
/// the repair only split or dropped the grouping the instruction had asked for.
/// Eight and not more because a resolved ID costs 66 bytes in the brief that is
/// itself checked against `summary_budget_bytes`, and those checks already fail
/// on length.
pub(crate) const MAX_FINDINGS: usize = 12;
pub(crate) const MAX_EVIDENCE_IDS: usize = 12;

/// How many findings a reading may return before it is rejected, as opposed to
/// how many `MAX_FINDINGS` asks it for.
///
/// The count is a target, not the budget: what a brief must fit is
/// `summary_budget_bytes`, and that check runs on the reading that comes back.
/// A reading that says fourteen true things inside the budget was rejected for
/// the number alone and the retry wrote the same reading again, having been
/// given nothing else to change - measured in one run as seven of its retries.
/// So a response that is valid in every other way is accepted past the target,
/// and only the bytes decide. The ceiling stays finite because each finding
/// carries citations and a floor of text; a reading that doubles the target has
/// stopped grouping, which is what the target asks for.
pub(crate) const ACCEPTED_FINDINGS: usize = MAX_FINDINGS * 2;

/// The same split for citations, and for the same reason. Told that its summary
/// was over budget, a reading did what the instruction asks - grouped related
/// passages under one observation and cited them together - and came back with
/// sixteen IDs on one finding, which the target rejected. The two rules were
/// pushing opposite ways on one node. What the citations actually cost is
/// bytes, 66 to a resolved ID, and `summary_budget_bytes` already charges them.
pub(crate) const ACCEPTED_EVIDENCE_IDS: usize = MAX_EVIDENCE_IDS * 2;

/// Unresolved links a reading is asked to record, and the number that is
/// still taken. Their bytes are charged with the rest of the brief, so the
/// count is the same kind of target as the others: a reading that found ten
/// genuine gaps should not have to drop two of them to be read at all.
pub(crate) const MAX_UNCERTAINTIES: usize = 8;
pub(crate) const ACCEPTED_UNCERTAINTIES: usize = MAX_UNCERTAINTIES * 2;

/// How many source anchors one planned section may carry.
///
/// Deliberately not `MAX_EVIDENCE_IDS`: a section names where a reader should
/// start, a finding names what one observation rests on, and the two have moved
/// independently. Named so the next change to either does not sweep up the other.
pub(crate) const MAX_SECTION_ANCHORS: usize = 8;

/// Render a prompt's limits from the constants the checker enforces.
///
/// A prompt that repeats a limit as a literal drifts from the check silently,
/// and the reading is then rejected for obeying what it was told. Twice this
/// session a cap moved and a prompt did not, so prompts carry the placeholder
/// and never the number.
pub(crate) fn with_limits(template: &str) -> String {
    template
        .replace("{MAX_EVIDENCE_IDS}", &MAX_EVIDENCE_IDS.to_string())
        .replace("{MAX_FINDINGS}", &MAX_FINDINGS.to_string())
        .replace("{MAX_SECTION_ANCHORS}", &MAX_SECTION_ANCHORS.to_string())
        .replace("{MAX_UNCERTAINTIES}", &MAX_UNCERTAINTIES.to_string())
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}

/// Resolve only unambiguous prefixes from THIS request, then persist full hashes.
///
/// `subject` names the finding or section being checked. Every other rule here
/// says which item broke it; a bare "supply 1-N evidence_ids" leaves a repair
/// attempt guessing which of a dozen items was empty, so it repeats the mistake.
fn resolve_ids(
    ids: &mut [String],
    evidence: &[Evidence],
    requested: usize,
    accepted: usize,
    subject: &str,
) -> Result<()> {
    ensure!(
        !ids.is_empty() && ids.len() <= accepted,
        "{subject:?} supplied {} evidence_ids; cite 1-{accepted} of the evidence IDs in this request{}, or drop the item when nothing supplied supports it. Names and line numbers from source_graph are not evidence IDs",
        ids.len(),
        if accepted > requested {
            format!(
                " (it asks for at most {requested}, so passages supporting a different claim belong in their own item)"
            )
        } else {
            String::new()
        }
    );
    let mut seen = HashSet::new();
    for id in ids {
        ensure!(
            id.len() >= 8 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "{subject:?} cites {id:?}, which is not a supplied evidence ID"
        );
        let matches: Vec<_> = evidence
            .iter()
            .filter(|e| e.id.starts_with(id.as_str()))
            .collect();
        ensure!(matches.len() == 1, "Unknown or ambiguous evidence ID: {id}");
        *id = matches[0].id.clone();
        ensure!(
            seen.insert(id.clone()),
            "{subject:?} repeats evidence ID {id:?}"
        );
    }
    Ok(())
}

pub(crate) fn validate_brief(
    brief: &mut SourceBrief,
    evidence: &[Evidence],
    final_pass: bool,
) -> Result<()> {
    ensure!(
        !brief.findings.is_empty() && brief.findings.len() <= ACCEPTED_FINDINGS,
        "Supply 1-{ACCEPTED_FINDINGS} source findings; the request asks for at most {MAX_FINDINGS}, so group related passages under one observation rather than listing {} of them",
        brief.findings.len()
    );
    ensure!(
        brief.uncertainties.len() <= ACCEPTED_UNCERTAINTIES
            && brief.uncertainties.iter().all(|s| bounded_text(s, 1500)),
        "Supply at most {ACCEPTED_UNCERTAINTIES} nonempty uncertainties of at most 1500 bytes each; the request asks for {MAX_UNCERTAINTIES} (received {} items; lengths {:?})",
        brief.uncertainties.len(),
        brief
            .uncertainties
            .iter()
            .map(String::len)
            .collect::<Vec<_>>()
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
        let subject = finding.topic.clone();
        resolve_ids(
            &mut finding.evidence_ids,
            evidence,
            MAX_EVIDENCE_IDS,
            ACCEPTED_EVIDENCE_IDS,
            &subject,
        )?;
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
    branches: Option<usize>,
) -> Result<()> {
    let ceiling = section_ceiling(branches);
    ensure!(
        !outline.sections.is_empty() && outline.sections.len() <= ceiling,
        "Supply 1-{ceiling} sections"
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
    let mut ids = HashSet::new();
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
        ensure!(
            section.key_points.len() <= 12
                && section.key_points.iter().all(|p| bounded_text(p, 1500))
                && (!section.key_points.is_empty() || !section.reader_question.trim().is_empty()),
            "Each section needs concrete key_points"
        );
        ensure!(
            bounded_text(&section.title, 299) && bounded_text(&section.query, 1999),
            "Each section needs a bounded title and source query"
        );
        // Optional fields from older outlines remain readable and editable.
        ensure!(
            section.reader_question.len() <= 1500
                && section.handoff.len() <= 1500
                && section.out_of_scope.len() <= 12
                && section.out_of_scope.iter().all(|p| bounded_text(p, 1500)),
            "Invalid optional section context"
        );
        let normalize = |s: &str| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
        };
        ensure!(
            titles.insert(normalize(&section.title)),
            "Sections must have distinct titles"
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
        ensure!(
            section.branches.len() <= SECTIONS_MAX * 2
                && section.branches.iter().all(|b| bounded_text(b, 200)),
            "Invalid section branches"
        );
        // A section planned from branches draws its source from them, so an
        // anchor is optional there; attaching a passage from another part of the
        // source only to satisfy a count misleads the writer.
        if !section.evidence_ids.is_empty() || section.branches.is_empty() {
            let subject = section.title.clone();
            resolve_ids(
                &mut section.evidence_ids,
                evidence,
                MAX_SECTION_ANCHORS,
                MAX_SECTION_ANCHORS,
                &subject,
            )?;
        }
    }
    ensure!(
        outline.excluded_branches.len() <= 1024
            && outline
                .excluded_branches
                .iter()
                .all(|e| bounded_text(&e.branch, 200) && bounded_text(&e.reason, 1500)),
        "Invalid excluded_branches"
    );
    if let Some(error) = outline_diagram_error(outline, maximum) {
        bail!("{error}");
    }
    Ok(())
}

/// Resolve generated title references locally. Numeric dependencies remain supported
/// for old providers/checkpoints; never guess whether their numbering is one-based.
/// `labels` maps the branch labels the request showed to node keys; a plan made
/// without a branch view has none, and any branches it names are dropped.
fn decode_generated_outline(
    response: &str,
    repair: &mut llm::JsonRepair,
    labels: &HashMap<String, String>,
) -> Result<Outline> {
    let mut value: serde_json::Value = repair.decode(response)?;
    if let Some(sections) = value.get_mut("sections").and_then(|v| v.as_array_mut()) {
        for (index, section) in sections.iter_mut().enumerate() {
            let subject = format!(
                "sections[{index}] ({:?})",
                section
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
            );
            if let Some(branches) = section.get_mut("branches") {
                if labels.is_empty() || branches.is_null() {
                    *branches = json!([]);
                } else {
                    resolve_labels(branches, labels, &subject)?;
                }
            }
        }
    }
    resolve_exclusions(&mut value, labels)?;
    reject_covered_exclusions(&value, labels)?;
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
    // Ownership metadata is no longer part of the generated outline contract.
    if let Some(object) = value.as_object_mut() {
        object.remove("requirements");
    }
    if let Some(sections) = value.get_mut("sections").and_then(|v| v.as_array_mut()) {
        for section in sections {
            if let Some(object) = section.as_object_mut() {
                object.remove("owns_requirement_ids");
            }
        }
    }
    let mut plan: Outline = llm::decode(&serde_json::to_string(&value)?)?;
    plan.requirements.clear();
    // Repetition carries no additional meaning; keep the first occurrence.
    for section in &mut plan.sections {
        let mut seen = HashSet::new();
        section.depends_on.retain(|d| seen.insert(*d));
        section.owns_requirement_ids.clear();
    }
    Ok(plan)
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
            .take(64)
            .enumerate()
            .map(|(index, s)| json!({"index":index,"title":s.get("title"),
            "prerequisite_titles":s.get("prerequisite_titles"),"depends_on":s.get("depends_on"),"owns_requirement_ids":s.get("owns_requirement_ids")}))
            .collect::<Vec<_>>()
    )
}

/// Supplied passages with their runtime class attached.
///
/// The class used to travel in a separate table that repeated every passage's id
/// and path; on a reduction that table and the anchor list named each original
/// twice, which narrowed how many children fit one request.
pub(crate) fn classified(evidence: &[Evidence]) -> Vec<serde_json::Value> {
    evidence
        .iter()
        .map(|e| {
            json!({"id":e.id,"path":e.path,"start":e.start,"end":e.end,"content":e.content,
                "runtime_allowed":source::is_implementation(&e.path)})
        })
        .collect()
}

/// Previously read passages a request may cite without their text, leaving out
/// the ones whose text the request already carries.
pub(crate) fn anchors(retained: &[Evidence], shown: &[Evidence]) -> Vec<serde_json::Value> {
    let shown: HashSet<&str> = shown.iter().map(|e| e.id.as_str()).collect();
    let mut seen = HashSet::new();
    retained
        .iter()
        .filter(|e| !shown.contains(e.id.as_str()) && seen.insert(e.id.as_str()))
        .map(|e| {
            json!({"id":e.id,"path":e.path,"previously_read":true,
                "runtime_allowed":source::is_implementation(&e.path)})
        })
        .collect()
}

/// A brief without its citations, for a request that judges structure and
/// never cites a passage.
pub(crate) fn uncited(brief: &SourceBrief) -> serde_json::Value {
    json!({"findings":brief.findings.iter().map(|f| json!({"topic":f.topic,"observation":f.observation,"kind":f.kind})).collect::<Vec<_>>(),
        "uncertainties":brief.uncertainties})
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

/// The most sections one document may hold, however large the source.
///
/// A file a reader opens has to end somewhere. Sixty-four was that end while
/// every section request carried the whole outline, which made each request
/// grow with the document. Section, review and correction requests now carry a
/// bounded view of the plan, and a source large enough to come near this is
/// planned in chapters of at most sixteen branches a request, so the document
/// can keep growing with the source well past where it used to stop.
const SECTIONS_MAX: usize = 256;
/// What the ceiling was before it followed the branches, and its floor now.
const SECTIONS_WITHOUT_BRANCHES: usize = 32;

/// How many sections this document may hold.
///
/// Thirty-two was flat, so a project ten times the size was allowed exactly as
/// much document, only coarser. The tree's branches are where the source's size
/// becomes visible, so the ceiling follows them; the floor keeps a small project
/// from being squeezed, and the model is still told to choose the smallest
/// useful number, so this bounds the answer rather than setting it.
/// `None` where the branches are not known - re-validating a stored outline,
/// say. An unknown count must not mean a small one, or an outline that was
/// valid when planned would be rejected the next time it is read.
pub(crate) fn section_ceiling(branches: Option<usize>) -> usize {
    match branches {
        // Floored at what it replaced. Raising the ceiling for a large source
        // must not lower it for a small one: five branches would otherwise cap
        // a project at fifteen sections where a flat thirty-two had allowed it
        // twice that, and an outline that was fine before would fail planning.
        Some(n) => n
            .saturating_mul(3)
            .clamp(SECTIONS_WITHOUT_BRANCHES, SECTIONS_MAX),
        None => SECTIONS_MAX,
    }
}

/// Whether descending from `here` to `next` lands closer to `target`.
fn closer_level(here: usize, next: usize, target: usize) -> bool {
    next <= target || next - target <= target.saturating_sub(here)
}

/// The widest level of the reading tree the planner is shown as branches.
///
/// Sixty-four capped the document at the same breadth for every source past a
/// few megabytes. Chapter planning shows no request more than sixteen branches,
/// so the level can follow the source further.
const BRANCHES_MAX: usize = 256;

/// How many branches the planner is shown.
///
/// An absolute threshold picked the level wrongly: the tree fans in by four, so
/// the levels below the root measure roughly 1, 4, 16, 64 whatever the source,
/// and the first level past a fixed eight lands somewhere in [8, 32) with no
/// relation to how much was read. A source ten times larger only made the tree
/// one level deeper and could be described by *fewer* branches - 52 leaves
/// stopped at 13, 520 leaves at 9. The target follows the leaves instead, so
/// the level chosen widens as the source does.
fn branch_target(leaves: usize) -> usize {
    (leaves / 4).clamp(8, BRANCHES_MAX)
}
/// Bytes the branch view may spend. Split evenly, so more branches each say
/// less rather than the last ones saying nothing.
const BRANCH_VIEW_BYTES: usize = 24_000;

/// Past this many branches one planning request can describe each of them only
/// in a sentence, so the document is planned in chapters instead: one request
/// divides the branches into chapters, and each chapter is planned from its
/// branches, at most this many to a request, described in full.
const CHAPTER_BRANCHES: usize = 16;
/// The most chapters a document is divided into.
const MAX_CHAPTERS: usize = 16;

/// A child of a large branch, which a section may take on its own.
///
/// A branch big enough to need several sections used to be named whole by each
/// of them, so they all drew the same observations from it. Naming its parts
/// lets the plan say which section explains which part.
struct Part {
    label: String,
    key: String,
    topics: Vec<String>,
    files: usize,
}

/// One part of the source as the reading tree grouped it.
pub(crate) struct Branch {
    /// What requests call it; node keys never leave the server.
    label: String,
    key: String,
    findings: Vec<Finding>,
    files: Vec<String>,
    /// False when the walk could not reach every leaf, so `file_count` is a
    /// floor rather than the number. Silence there would read as a small branch.
    whole: bool,
    parts: Vec<Part>,
}

/// A run of branches under one node above them, for dividing a large source
/// into chapters.
struct Group {
    label: String,
    topics: Vec<String>,
    members: Vec<usize>,
}

/// The reduction tree's branches, as the planner is shown them.
#[derive(Default)]
pub(crate) struct BranchView {
    branches: Vec<Branch>,
    groups: Vec<Group>,
}

impl BranchView {
    /// `None` for a tree that could not be read: an unknown count must not
    /// tighten the ceiling below where it sat before branches were consulted.
    fn count(&self) -> Option<usize> {
        (!self.branches.is_empty()).then_some(self.branches.len())
    }
    fn chapters_needed(&self) -> bool {
        self.branches.len() > CHAPTER_BRANCHES && !self.groups.is_empty()
    }
    /// Every label a plan may name - branches and their parts - by node key.
    fn keys_by_label(&self) -> HashMap<String, String> {
        self.branches
            .iter()
            .flat_map(|b| {
                std::iter::once((b.label.clone(), b.key.clone()))
                    .chain(b.parts.iter().map(|p| (p.label.clone(), p.key.clone())))
            })
            .collect()
    }
    fn labels_by_key(&self) -> HashMap<String, String> {
        self.keys_by_label()
            .into_iter()
            .map(|(label, key)| (key, label))
            .collect()
    }
    fn get(&self, key: &str) -> Option<&Branch> {
        self.branches.iter().find(|b| b.key == key)
    }
    /// The branch a key names: the branch itself or one of its parts.
    fn owner(&self, key: &str) -> Option<&Branch> {
        self.branches
            .iter()
            .find(|b| b.key == key || b.parts.iter().any(|p| p.key == key))
    }
    fn index(&self, key: &str) -> usize {
        self.branches
            .iter()
            .position(|b| b.key == key)
            .unwrap_or(usize::MAX)
    }
}

/// The reduction tree's branches, as the topics each one covers.
///
/// An outline used to be planned from the root summary alone: twelve findings
/// inside one summary budget, whatever the size of the source. The document's
/// breadth was therefore fixed by a byte budget rather than by how much the
/// project does, and a larger project produced the same number of sections,
/// only coarser. Section writing never had that limit - `section_memory`
/// rehydrates leaf observations the root never carried - so the ceiling was on
/// what sections could exist, not on what one could say.
///
/// The tree already groups the source by locality. Showing the planner the
/// branches lets the number of sections follow the number of things the project
/// does. This reads checkpoints only; it asks the model nothing.
pub(crate) async fn branch_topics(ctx: &RunContext) -> Result<BranchView> {
    let tree = crate::understanding::tree(ctx).await?;
    let levels = tree.levels();
    let Some(chosen) = branch_level(&levels, branch_target(tree.leaves.len())) else {
        return Ok(BranchView::default());
    };
    let roots = &ctx.snapshot.task.sources;
    let branches: Vec<Branch> = levels[chosen]
        .iter()
        .enumerate()
        .filter_map(|(index, key)| {
            let node = tree.nodes.get(key)?;
            let (files, whole) = covered_files(&tree, key, roots);
            let parts = if node.children.len() >= 2 {
                node.children
                    .iter()
                    .enumerate()
                    .filter_map(|(child_index, child)| {
                        let child_node = tree.nodes.get(child)?;
                        Some(Part {
                            label: format!("B{}.{}", index + 1, child_index + 1),
                            key: child.clone(),
                            topics: child_node
                                .findings
                                .iter()
                                .map(|f| f.topic.clone())
                                .collect(),
                            files: covered_files(&tree, child, roots).0.len(),
                        })
                    })
                    .collect()
            } else {
                vec![]
            };
            Some(Branch {
                label: format!("B{}", index + 1),
                key: key.clone(),
                findings: node.findings.clone(),
                files,
                whole,
                parts,
            })
        })
        .collect();
    let groups = if branches.len() > CHAPTER_BRANCHES {
        branch_groups(&tree, &levels, chosen, &branches)
    } else {
        vec![]
    };
    Ok(BranchView { branches, groups })
}

/// The index of the level the planner is shown, or `None` when no level has at
/// least two nodes.
fn branch_level(levels: &[Vec<String>], target: usize) -> Option<usize> {
    let mut chosen = 0;
    while chosen + 1 < levels.len()
        && levels[chosen].len() < target
        // Levels step by the fan-in, so the first one past the target can
        // overshoot it several times over - 21 then 82 against a target of 64.
        // Whichever sits closer to the target is the better description.
        && closer_level(levels[chosen].len(), levels[chosen + 1].len(), target)
    {
        chosen += 1;
    }
    (levels.get(chosen)?.len() >= 2).then_some(chosen)
}

/// Group the chosen branches under the deepest level above them that is narrow
/// enough to plan chapters from; runs of eight when no such level exists.
fn branch_groups(
    tree: &crate::understanding::TreeIndex,
    levels: &[Vec<String>],
    chosen: usize,
    branches: &[Branch],
) -> Vec<Group> {
    let above = (0..chosen)
        .rev()
        .find(|&i| (2..=MAX_CHAPTERS).contains(&levels[i].len()));
    let Some(above) = above else {
        return branches
            .chunks(8)
            .enumerate()
            .map(|(index, run)| Group {
                label: format!("G{}", index + 1),
                topics: vec![],
                members: run
                    .iter()
                    .filter_map(|b| branches.iter().position(|x| x.key == b.key))
                    .collect(),
            })
            .collect();
    };
    // Each node of a level is its own parent one level down when it passed
    // through, and its children's parent otherwise.
    let mut parent: HashMap<&str, &str> = HashMap::new();
    for level in &levels[above..chosen] {
        for key in level {
            match tree.nodes.get(key) {
                Some(node) if !node.children.is_empty() => {
                    for child in &node.children {
                        parent.insert(child, key);
                    }
                }
                _ => {
                    parent.insert(key, key);
                }
            }
        }
    }
    let ancestor = |key: &str| {
        let mut key = key;
        for _ in above..chosen {
            key = parent.get(key).copied().unwrap_or(key);
        }
        key.to_string()
    };
    levels[above]
        .iter()
        .enumerate()
        .filter_map(|(index, group)| {
            let members: Vec<usize> = branches
                .iter()
                .enumerate()
                .filter(|(_, b)| ancestor(&b.key) == *group)
                .map(|(i, _)| i)
                .collect();
            (!members.is_empty()).then(|| Group {
                label: format!("G{}", index + 1),
                topics: tree
                    .nodes
                    .get(group)
                    .map(|n| n.findings.iter().map(|f| f.topic.clone()).collect())
                    .unwrap_or_default(),
                members,
            })
        })
        .collect()
}

/// Every file a branch was read from, and whether the walk saw all of them.
///
/// A node's own `files` are the paths of the passages that reduction carried,
/// and a reduction packs its children's originals into whatever budget the
/// summaries leave over - so a branch covering sixteen files reports the six
/// that happened to fit. The count decides how many sections a branch earns and
/// the shared directory is its only identity, so both come from the leaves,
/// where `files` is what was actually read.
fn covered_files(
    tree: &crate::understanding::TreeIndex,
    key: &str,
    roots: &[String],
) -> (Vec<String>, bool) {
    let mut seen = HashSet::new();
    let mut whole = true;
    let mut pending = vec![key.to_string()];
    while let Some(key) = pending.pop() {
        match tree.nodes.get(&key) {
            Some(node) if node.children.is_empty() => seen.extend(node.files.iter().cloned()),
            Some(node) => pending.extend(node.children.iter().cloned()),
            None => whole = false,
        }
    }
    // Shown against the source the user named, not from the filesystem root:
    // every branch otherwise repeats the same long prefix in `where` and in
    // every path it lists, and the reader already knows where their source is.
    let mut files: Vec<String> = seen.iter().map(|path| relative(path, roots)).collect();
    files.sort();
    (files, whole)
}

fn relative(path: &str, roots: &[String]) -> String {
    roots
        .iter()
        .filter_map(|root| path.strip_prefix(&format!("{}/", root.trim_end_matches('/'))))
        .min_by_key(|rest| rest.len())
        .unwrap_or(path)
        .to_string()
}

/// One branch each, inside a shared byte budget.
///
/// Split evenly rather than first-come: a project with many parts should have
/// every part described briefly, not the first few described fully and the rest
/// left out of the document entirely.
fn branch_view(level: &[&Branch]) -> Vec<serde_json::Value> {
    // Measured, not divided and trusted. Dividing the budget by the branches and
    // flooring each share spent five times the allowance on a wide tree, and
    // trading topics for room keeps their product the same, so shrinking the
    // allowance is what actually converges.
    // Parts are what lets sections sharing a large branch take different pieces
    // of it, so while they take no more than half the budget they stay in every
    // attempt and the topics shrink around them; past that they go with the
    // file lists.
    let parts_bytes: usize = render_branches(level, 0, 0, 0, true)
        .iter()
        .map(|record| serde_json::to_vec(&record["parts"]).map_or(0, |v| v.len()))
        .sum();
    let keep_parts = parts_bytes <= BRANCH_VIEW_BYTES / 2;
    let mut allowance = BRANCH_VIEW_BYTES;
    for _ in 0..24 {
        let per = allowance / level.len().max(1);
        let topics = (per / OBSERVATION_FLOOR_BYTES).clamp(1, MAX_FINDINGS);
        let view = render_branches(
            level,
            topics,
            (per / topics).max(OBSERVATION_FLOOR_BYTES),
            (per / 600).min(12),
            keep_parts || per >= 400,
        );
        if fits(&view) {
            return view;
        }
        allowance = allowance * 4 / 5;
    }
    // Past that the branches themselves do not fit. Carry as many as do and say
    // how many were left out, rather than silently describing a prefix as if it
    // were the whole source.
    let mut shown = level.len();
    while shown > 1 {
        shown = shown * 4 / 5;
        let mut view = render_branches(&level[..shown], 1, OBSERVATION_FLOOR_BYTES, 0, false);
        view.push(json!({"branches_not_shown": level.len() - shown}));
        if fits(&view) {
            return view;
        }
    }
    vec![json!({"branches_not_shown": level.len()})]
}

fn fits(view: &[serde_json::Value]) -> bool {
    serde_json::to_vec(view).is_ok_and(|v| v.len() <= BRANCH_VIEW_BYTES)
}

/// The directory a branch lives in, as the one thing that says what it is.
///
/// A branch arrived as a file list and two counts, so the planner had to guess
/// what the group was for from filenames. Its shared directory is the part of
/// the tree it covers, and the tree was grouped by locality in the first place.
fn shared_root(files: &[String]) -> String {
    let Some(first) = files.first() else {
        return String::new();
    };
    let mut root = first.rsplit_once('/').map_or("", |(dir, _)| dir);
    for path in files.iter().skip(1) {
        while !root.is_empty() && !path.starts_with(&format!("{root}/")) {
            root = root.rsplit_once('/').map_or("", |(dir, _)| dir);
        }
    }
    root.to_string()
}

/// The directories holding most of a branch's files, when they are not all in
/// the one `where` names.
///
/// Reading order follows the call graph, so a branch can span directories whose
/// only shared ancestor is the project root, and `where` then says nothing.
fn main_dirs(files: &[String], root: &str) -> Vec<serde_json::Value> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for path in files {
        *counts
            .entry(path.rsplit_once('/').map_or("", |(dir, _)| dir))
            .or_default() += 1;
    }
    if counts.len() < 2 && counts.keys().all(|dir| *dir == root) {
        return vec![];
    }
    let mut dirs: Vec<(&str, usize)> = counts.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    dirs.into_iter()
        .take(3)
        .map(|(dir, count)| json!([dir, count]))
        .collect()
}

/// The shortest observation still worth reading; below this a topic is a title.
const OBSERVATION_FLOOR_BYTES: usize = 120;

fn render_branches(
    level: &[&Branch],
    topics: usize,
    room: usize,
    files: usize,
    parts: bool,
) -> Vec<serde_json::Value> {
    level
        .iter()
        .map(|branch| {
            let shown: Vec<serde_json::Value> = branch
                .findings
                .iter()
                .take(topics)
                .map(|f| {
                    json!({"topic":editorial::excerpt(&f.topic,200),
                        "observation":editorial::excerpt(&f.observation, room)})
                })
                .collect();
            let root = shared_root(&branch.files);
            // The counts travel even when the lists are trimmed, so a branch
            // that holds a lot is still recognisable as one that does.
            let mut record = json!({"branch":branch.label,"where":root,
                "files":branch.files.iter().take(files).collect::<Vec<_>>(),
                "file_count":branch.files.len(),"topic_count":branch.findings.len(),
                "topics":shown});
            if files > 0 {
                let dirs = main_dirs(&branch.files, &root);
                if !dirs.is_empty() {
                    record["main_dirs"] = json!(dirs);
                }
            }
            {
                if parts && !branch.parts.is_empty() {
                    record["parts"] = json!(
                        branch
                            .parts
                            .iter()
                            .map(|p| json!({"branch":p.label,"file_count":p.files,
                                "topics":p.topics.iter().take(2).map(|t| editorial::excerpt(t, 60)).collect::<Vec<_>>()}))
                            .collect::<Vec<_>>()
                    );
                }
            }
            if !branch.whole {
                record["file_count_is_a_floor"] = json!(true);
            }
            record
        })
        .collect()
}

/// The groups a large source's branches sit in, with each branch reduced to its
/// label, place, size and topic titles, for dividing the document into chapters.
///
/// Past what the budget can describe, each group lists only its branch labels:
/// a chapter can take a whole group by its label, so the group's own topics are
/// what the division is judged by.
fn group_view(view: &BranchView) -> Vec<serde_json::Value> {
    let render = |group_topics: usize, branch_topics: Option<usize>, title: usize| {
        view.groups
            .iter()
            .map(|group| {
                let members: Vec<&Branch> =
                    group.members.iter().map(|i| &view.branches[*i]).collect();
                let files: Vec<String> =
                    members.iter().flat_map(|b| b.files.iter().cloned()).collect();
                let branches = match branch_topics {
                    Some(topics) => json!(members.iter().map(|b| json!({"branch":b.label,"where":shared_root(&b.files),
                        "file_count":b.files.len(),
                        "topics":b.findings.iter().take(topics).map(|f| editorial::excerpt(&f.topic, title)).collect::<Vec<_>>()})).collect::<Vec<_>>()),
                    None => json!(members.iter().map(|b| b.label.as_str()).collect::<Vec<_>>()),
                };
                json!({"group":group.label,"where":shared_root(&files),"file_count":files.len(),
                    "topics":group.topics.iter().take(group_topics).map(|t| editorial::excerpt(t, title.max(80))).collect::<Vec<_>>(),
                    "branches":branches})
            })
            .collect::<Vec<_>>()
    };
    let mut result = vec![];
    for (group_topics, branch_topics, title) in [
        (6, Some(4), 160),
        (4, Some(3), 120),
        (3, Some(2), 80),
        (2, Some(1), 60),
        (6, None, 120),
        (3, None, 80),
        (0, None, 0),
    ] {
        result = render(group_topics, branch_topics, title);
        if fits(&result) {
            break;
        }
    }
    result
}

/// Which branches the plan covers, excludes, or leaves in neither.
///
/// Computed rather than asked for, so outline review judges an omission it is
/// shown instead of one it has to notice in a list of titles. A branch whose
/// parts the plan names part by part is judged part by part, so a large branch
/// split across sections cannot hide the part none of them took.
pub(crate) fn branch_coverage(plan: &Outline, view: &BranchView) -> serde_json::Value {
    if view.branches.is_empty() {
        return serde_json::Value::Null;
    }
    let covered: HashSet<&str> = plan
        .sections
        .iter()
        .flat_map(|s| s.branches.iter().map(String::as_str))
        .collect();
    let excluded: HashMap<&str, &str> = plan
        .excluded_branches
        .iter()
        .map(|e| (e.branch.as_str(), e.reason.as_str()))
        .collect();
    let mut unassigned = vec![];
    let mut exclusions = vec![];
    let mut covered_branches = 0;
    for branch in &view.branches {
        let place = shared_root(&branch.files);
        if covered.contains(branch.key.as_str()) {
            covered_branches += 1;
            continue;
        }
        let by_parts = branch
            .parts
            .iter()
            .any(|p| covered.contains(p.key.as_str()) || excluded.contains_key(p.key.as_str()));
        if by_parts {
            if branch
                .parts
                .iter()
                .any(|p| covered.contains(p.key.as_str()))
            {
                covered_branches += 1;
            }
            for part in &branch.parts {
                if covered.contains(part.key.as_str()) {
                    continue;
                }
                match excluded.get(part.key.as_str()) {
                    Some(reason) => exclusions
                        .push(json!({"branch":part.label,"where":place,"reason":reason})),
                    None => unassigned.push(json!({"branch":part.label,"where":place,
                        "file_count":part.files,
                        "topics":part.topics.iter().take(6).map(|t| editorial::excerpt(t, 120)).collect::<Vec<_>>()})),
                }
            }
            continue;
        }
        match excluded.get(branch.key.as_str()) {
            Some(reason) => {
                exclusions.push(json!({"branch":branch.label,"where":place,"reason":reason}))
            }
            None => unassigned.push(json!({"branch":branch.label,"where":place,
                "file_count":branch.files.len(),
                "topics":branch.findings.iter().take(6).map(|f| editorial::excerpt(&f.topic, 120)).collect::<Vec<_>>()})),
        }
    }
    // Sections that took many branches at once. Planning asked for the
    // smallest number of sections, and a model answered a three-megabyte source
    // with seven, one of them holding ten branches; review is shown where that
    // happened so it can judge whether they are one workflow or several.
    let labels = view.labels_by_key();
    let crowded: Vec<serde_json::Value> = plan
        .sections
        .iter()
        .filter(|section| section.branches.len() >= CROWDED_BRANCHES)
        .map(|section| {
            let files: usize = section
                .branches
                .iter()
                .filter_map(|key| view.owner(key).map(|b| (b.key.as_str(), b)))
                .collect::<HashMap<_, _>>()
                .values()
                .map(|b| b.files.len())
                .sum();
            json!({"id":short_id(&section.id),"title":section.title,
                "branches":section.branches.iter().map(|k| labels.get(k).cloned().unwrap_or_else(|| "unlisted".into())).collect::<Vec<_>>(),
                "file_count":files})
        })
        .collect();
    json!({"branches":view.branches.len(),"covered":covered_branches,
        "excluded":exclusions,"unassigned":unassigned,"crowded_sections":crowded})
}

/// How many branches or parts one section may name before review is asked
/// whether it should be split.
const CROWDED_BRANCHES: usize = 4;

/// The branches the plan accounted for in neither way, as issues of their own.
///
/// Which branches those are is computed rather than judged, and the review
/// request already shows them under a rule that calls an unassigned branch the
/// purpose needs a major issue. A real review answered `issues: []` with four
/// parts unassigned, two of them the agent loop the document was asked to
/// explain, and the plan was approved as it stood. Whether the purpose needs a
/// branch is still the model's judgement - it can answer by excluding the
/// branch with a reason - but that the question is answered at all is not.
/// Whether `message` names the branch `label` rather than the start of a longer
/// one. `B1` reads as a substring of `B12` and of `B1.2`, so a review that
/// named the twelfth branch silently answered for the first as well.
fn names_label(message: &str, label: &str) -> bool {
    message.match_indices(label).any(|(at, _)| {
        let before = message[..at].chars().next_back();
        let after = message[at + label.len()..].chars().next();
        before.is_none_or(|c| !c.is_ascii_alphanumeric())
            && after.is_none_or(|c| !c.is_ascii_digit() && c != '.')
    })
}

/// Sections holding more than their writer can be shown.
///
/// `crowded_sections` tells review how many branches a section took and asks
/// whether they are one workflow; a real review answered that with silence
/// while five sections of six were crowded, one of them 155 files. Whether
/// branches belong together is a judgement, but whether the section's scope
/// reaches the writer is not: `branch_memory` carries one observation per leaf
/// inside a fixed budget and counts what it leaves out. A section whose
/// checklist is mostly deferred gets written without most of what the whole
/// reading found about it, however well its branches go together.
async fn unshowable_sections(
    ctx: &RunContext,
    plan: &Outline,
    reported: &[crate::model::OutlineIssue],
) -> Result<Vec<crate::model::OutlineIssue>> {
    if plan.sections.iter().all(|s| s.branches.is_empty()) {
        return Ok(vec![]);
    }
    let counts =
        crate::purpose::scope_counts(ctx, plan, crate::runner::BRANCH_MEMORY_BYTES).await?;
    let mut issues = vec![];
    for (index, section) in plan.sections.iter().enumerate() {
        if section.branches.is_empty()
            || reported
                .iter()
                .any(|issue| issue.section_ids.contains(&section.id))
        {
            continue;
        }
        let (shown, deferred) = counts.get(index).copied().unwrap_or_default();
        issues.extend(scope_issue(section, shown, deferred));
    }
    Ok(issues)
}

/// The issue a section earns when most of its own scope stays behind.
fn scope_issue(
    section: &SectionPlan,
    shown: usize,
    deferred: usize,
) -> Option<crate::model::OutlineIssue> {
    (deferred > shown).then(|| crate::model::OutlineIssue {
        severity: "major".into(),
        code: "section_scope_not_shown".into(),
        message: format!(
            "Section {} ({:?}) covers {} branches, and the reading found more in its scope than its writer can be shown: {shown} observations fit the checklist a section is written from and {deferred} are left out. Split it so that each section carries a scope that fits, or give some of its branches to the sections that explain them.",
            short_id(&section.id),
            section.title,
            section.branches.len()
        ),
        section_ids: vec![section.id.clone()],
        requirement_ids: vec![],
        query: String::new(),
    })
}

fn unassigned_issues(
    coverage: &serde_json::Value,
    reported: &[crate::model::OutlineIssue],
) -> Vec<crate::model::OutlineIssue> {
    coverage["unassigned"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let label = item.get("branch").and_then(|v| v.as_str())?;
            // A review that already named the branch has judged it; saying it
            // twice would spend a correction round on agreeing.
            if reported
                .iter()
                .any(|issue| names_label(&issue.message, label))
            {
                return None;
            }
            let topics: Vec<&str> = item
                .get("topics")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .collect();
            Some(crate::model::OutlineIssue {
                severity: "major".into(),
                code: "unassigned_branch".into(),
                message: format!(
                    "Branch {label} ({} files) is covered by no section and named in no exclusion, so the plan does not say whether the document explains it. Its topics are: {}. Name it in the branches of the section that explains it, add a section for it, or list it once in excluded_branches with a reason tied to the purpose.",
                    item.get("file_count").and_then(|v| v.as_u64()).unwrap_or_default(),
                    topics.join("; ")
                ),
                section_ids: vec![],
                requirement_ids: vec![],
                query: String::new(),
            })
        })
        .collect()
}

/// Bytes branch coverage may take in an outline review.
const COVERAGE_VIEW_BYTES: usize = 12_000;

/// Coverage as a review request carries it: every unassigned or excluded part
/// is still named, with fewer topics and shorter reasons as the list grows.
fn coverage_view(coverage: &serde_json::Value) -> serde_json::Value {
    if coverage.is_null() {
        return coverage.clone();
    }
    let mut view = coverage.clone();
    for (topics, reason) in [(6, 400), (3, 200), (1, 120), (0, 60)] {
        view = coverage.clone();
        for item in view["unassigned"].as_array_mut().into_iter().flatten() {
            if let Some(list) = item["topics"].as_array_mut() {
                list.truncate(topics);
            }
        }
        for item in view["excluded"].as_array_mut().into_iter().flatten() {
            let text = item["reason"].as_str().unwrap_or_default().to_string();
            item["reason"] = json!(editorial::excerpt(&text, reason));
        }
        if serde_json::to_vec(&view).is_ok_and(|v| v.len() <= COVERAGE_VIEW_BYTES) {
            return view;
        }
    }
    view
}

/// Bytes a plan may take when a review or a correction round is shown it.
///
/// Both used to carry the whole outline, queries and full evidence hashes
/// included. At the default context an outline review stopped fitting at about
/// forty-eight sections and a correction round at about thirty-two, and each
/// failed three times at the same size before leaving the run waiting.
const OUTLINE_VIEW_BYTES: usize = 32_000;
/// Characters of a section id a view shows. Ids a model returns are resolved
/// back by prefix, so a view spends twelve bytes on each rather than sixty-four.
const SHORT_ID: usize = 12;

pub(crate) fn short_id(id: &str) -> &str {
    id.get(..SHORT_ID).unwrap_or(id)
}

/// Resolve section ids a model returned from a view that shortened them.
/// False when any names no section or more than one.
pub(crate) fn resolve_section_ids(ids: &mut [String], plan: &Outline) -> bool {
    ids.iter_mut().all(|id| {
        let wanted = id.trim().to_string();
        let matches: Vec<&str> = plan
            .sections
            .iter()
            .filter(|s| !wanted.is_empty() && s.id.starts_with(&wanted))
            .map(|s| s.id.as_str())
            .collect();
        match matches.as_slice() {
            [only] => {
                *id = only.to_string();
                true
            }
            _ => false,
        }
    })
}

/// A plan as a review or a correction round sees it: titles, key points,
/// branch labels and prerequisites, without the queries and evidence hashes
/// only writing needs, shortened until it fits.
fn outline_view(
    plan: &Outline,
    labels: &HashMap<String, String>,
    include: &dyn Fn(usize, &SectionPlan) -> bool,
    budget: usize,
) -> serde_json::Value {
    let label = |key: &String| {
        labels
            .get(key)
            .cloned()
            .unwrap_or_else(|| "unlisted".into())
    };
    let render = |points: usize, story: usize, title: usize| {
        let sections: Vec<serde_json::Value> = plan
            .sections
            .iter()
            .enumerate()
            .filter(|(index, section)| include(*index, section))
            .map(|(index, section)| {
                let mut record = json!({"index":index,"id":short_id(&section.id),
                    "title":editorial::excerpt(&section.title, title),
                    "branches":section.branches.iter().map(label).collect::<Vec<_>>()});
                if points > 0 {
                    record["key_points"] = json!(
                        section
                            .key_points
                            .iter()
                            .map(|p| editorial::excerpt(p, points))
                            .collect::<Vec<_>>()
                    );
                }
                let diagrams = section.diagrams.as_ref().map_or(0, Vec::len);
                if diagrams > 0 && title > 60 {
                    record["diagrams"] = json!(diagrams);
                }
                if !section.depends_on.is_empty() && title > 60 {
                    record["depends_on"] = json!(section.depends_on);
                }
                record
            })
            .collect();
        json!({"revision":plan.revision,"reader_goal":editorial::excerpt(&plan.reader_goal, story / 2),
            "storyline":editorial::excerpt(&plan.storyline, story),"sections":sections,
            "excluded_branches":plan.excluded_branches.iter().map(|e| json!({"branch":label(&e.branch),"reason":editorial::excerpt(&e.reason, 300)})).collect::<Vec<_>>()})
    };
    let mut view = serde_json::Value::Null;
    for (points, story, title) in [
        (400, 4000, 300),
        (200, 2000, 300),
        (100, 1000, 300),
        (0, 600, 300),
        (0, 300, 60),
        (0, 200, 36),
    ] {
        view = render(points, story, title);
        if serde_json::to_vec(&view).is_ok_and(|v| v.len() <= budget) {
            break;
        }
    }
    view
}

/// Correction feedback as a planning request carries it.
///
/// The stored feedback keeps the whole previous plan. A request gets a bounded
/// view of it, and a chapter request only the sections that took its branches
/// and the issues about them, rather than every chapter repeating all of it.
/// `scope` holds the branch keys a chapter request plans; `None` is the whole
/// document.
fn feedback_view(
    feedback: &serde_json::Value,
    view: &BranchView,
    scope: Option<&HashSet<String>>,
    budget: usize,
) -> serde_json::Value {
    let Some(stored) = feedback.as_object() else {
        return feedback.clone();
    };
    if stored.is_empty() {
        return feedback.clone();
    }
    let labels = view.labels_by_key();
    let mut shown = serde_json::Map::new();
    let mut chosen: HashSet<String> = HashSet::new();
    let mut indices: HashSet<usize> = HashSet::new();
    match stored
        .get("previous_plan")
        .map(|p| serde_json::from_value::<Outline>(p.clone()))
    {
        Some(Ok(plan)) => {
            let include = |_: usize, section: &SectionPlan| {
                scope.is_none_or(|keys| {
                    section
                        .branches
                        .iter()
                        .any(|b| view.owner(b).is_some_and(|o| keys.contains(&o.key)))
                })
            };
            for (index, section) in plan.sections.iter().enumerate() {
                if include(index, section) {
                    chosen.insert(section.id.clone());
                    indices.insert(index);
                }
            }
            shown.insert(
                "previous_plan".into(),
                outline_view(&plan, &labels, &include, budget),
            );
        }
        Some(Err(_)) => {
            let text = stored
                .get("previous_plan")
                .map(|p| p.to_string())
                .unwrap_or_default();
            shown.insert(
                "previous_plan".into(),
                json!(editorial::excerpt(&text, budget / 4)),
            );
        }
        None => {}
    }
    if let Some(issues) = stored.get("issues").and_then(|v| v.as_array()) {
        let kept: Vec<serde_json::Value> = issues
            .iter()
            .filter(|issue| {
                let ids: Vec<&str> = issue["section_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .collect();
                scope.is_none() || ids.is_empty() || ids.iter().any(|id| chosen.contains(*id))
            })
            .take(12)
            .map(|issue| {
                json!({"severity":issue["severity"],"code":issue["code"],
                    "message":editorial::excerpt(issue["message"].as_str().unwrap_or_default(), 1000),
                    "section_ids":issue["section_ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(short_id).collect::<Vec<_>>(),
                    "query":editorial::excerpt(issue["query"].as_str().unwrap_or_default(), 300)})
            })
            .collect();
        shown.insert("issues".into(), json!(kept));
    }
    if let Some(user) = stored.get("user_feedback") {
        shown.insert("user_feedback".into(), user.clone());
    }
    if let Some(draft) = stored.get("draft_context").and_then(|v| v.as_array()) {
        let kept: Vec<serde_json::Value> = draft
            .iter()
            .filter(|item| {
                scope.is_none()
                    || item["section"]
                        .as_u64()
                        .is_some_and(|i| indices.contains(&(i as usize)))
            })
            .cloned()
            .collect();
        shown.insert("draft_context".into(), json!(kept));
    }
    serde_json::Value::Object(shown)
}

// The brief, supporting findings, anchors, feedback, the branch view and the
// planning instruction that ride along with the packed evidence. Derived so
// that widening the branch view cannot quietly overrun the request it shares
// with the evidence.
const PLAN_REQUEST_OVERHEAD_BYTES: usize = 32_000 + BRANCH_VIEW_BYTES;

pub(crate) fn evidence_budget(ctx: &RunContext) -> usize {
    ctx.packing_limit(PLAN_REQUEST_OVERHEAD_BYTES.saturating_add(ctx.snapshot.task.direction.len()))
        .min(48_000)
}

/// Pack a planning request's evidence into the room the rest of it leaves.
///
/// The rest was reserved as a fixed estimate, but feedback from a correction
/// round and a chapter request's list of chapters and earlier titles are not in
/// it, and a request built past the gate fails before it is sent. `input` is
/// measured with every anchor and no evidence, which is its largest shape
/// without passages. Returns how many passages were packed. A chapter request
/// whose files no purpose passage touches may go without: its sections plan
/// from their branches.
fn pack_planning_evidence(
    ctx: &RunContext,
    discovery: &Discovery,
    input: &mut serde_json::Value,
    reductions: u32,
    required: bool,
) -> Result<usize> {
    input["source_anchors"] = json!(anchors(&discovery.evidence, &[]));
    input["evidence"] = json!([]);
    let carried = serde_json::to_vec(input)?.len();
    let limit = ctx.packing_limit(carried).min(48_000) >> reductions.min(16);
    let evidence = crate::purpose::pack(discovery, limit);
    ensure!(
        !required || !evidence.is_empty(),
        "CONTEXT_BUDGET: insufficient room for grounded outline"
    );
    input["source_anchors"] = json!(anchors(&discovery.evidence, &evidence));
    let count = evidence.len();
    input["evidence"] = json!(evidence);
    Ok(count)
}

/// The purpose reading narrowed to the files a chapter covers.
///
/// Every chapter request used to carry the whole document's brief, supporting
/// observations and packed passages - about ninety kilobytes, mostly about
/// other chapters, repeated once per chapter.
fn scope_discovery(discovery: &Discovery, files: &HashSet<String>, roots: &[String]) -> Discovery {
    let evidence: Vec<Evidence> = discovery
        .evidence
        .iter()
        .filter(|e| files.contains(&relative(&e.path, roots)))
        .cloned()
        .collect();
    let ids: HashSet<String> = evidence.iter().map(|e| e.id.clone()).collect();
    let keep = |f: &&Finding| f.evidence_ids.iter().any(|id| ids.contains(id));
    Discovery {
        brief: SourceBrief {
            findings: discovery
                .brief
                .findings
                .iter()
                .filter(keep)
                .cloned()
                .collect(),
            uncertainties: vec![],
            followup_queries: vec![],
        },
        details: discovery.details.iter().filter(keep).cloned().collect(),
        evidence,
        validation_unresolved: discovery.validation_unresolved,
    }
}

pub async fn outline(ctx: &RunContext, system: &str) -> Result<Outline> {
    crate::purpose::prepare(ctx).await?;
    // Old runs retain their outline and numbered drafts. New runs always discover
    // first; no file-name-only fallback is allowed after discovery failures.
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, "outline").await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let whole = crate::understanding::analyze(ctx, system).await?;
    let discovery = crate::purpose::analyze(ctx, system, &whole).await?;
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
    // Read once: the tree does not change while an outline is being planned.
    let view = branch_topics(ctx).await?;
    loop {
        let pending = db::load_checkpoint(&ctx.pool, &ctx.id, "outline_candidate").await?;
        if pending.is_none() {
            revision += 1;
        }
        let plan = match pending.map(serde_json::from_value::<Outline>).transpose()? {
            Some(plan) => plan,
            None if view.chapters_needed() => {
                plan_in_chapters(ctx, system, &discovery, &view, &feedback, revision).await?
            }
            None => plan_whole(ctx, system, &discovery, &view, &feedback, revision).await?,
        };
        // One statement for the candidate and the number that identifies it. As
        // separate writes, a process that died between them came back with a
        // plan from one revision and a state from the one before, and an
        // approval written for the plan was then compared against the state.
        let stored = serde_json::to_value(&plan)?;
        let mut tx = ctx.pool.begin().await?;
        for (step, value) in [
            ("outline_candidate".to_string(), stored.clone()),
            (format!("outline_candidate:{revision}"), stored),
            (
                "outline_state".to_string(),
                json!({"revision":revision,"round":round,"extra_queries":extra_queries}),
            ),
        ] {
            sqlx::query("INSERT INTO checkpoints(run_id,step,data) VALUES(?,?,?) ON DUPLICATE KEY UPDATE data=VALUES(data)").bind(&ctx.id).bind(step).bind(value.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let coverage = branch_coverage(&plan, &view);
        if !coverage.is_null() {
            ctx.event("outline_coverage", json!({"stage":"planning","title":"목차의 소스 브랜치 배정 확인","revision":revision,
                "branches":coverage["branches"],"covered":coverage["covered"],
                "excluded":coverage["excluded"].as_array().map_or(0, Vec::len),
                "unassigned":coverage["unassigned"],"crowded_sections":coverage["crowded_sections"]})).await?;
        }
        let mut review = review_outline(ctx, system, &plan, &discovery, &view, &coverage).await?;
        let reviewed = review.issues.len();
        review.issues.extend(unassigned_issues(&coverage, &review.issues));
        let unshowable = unshowable_sections(ctx, &plan, &review.issues).await?;
        review.issues.extend(unshowable);
        if review.issues.len() != reviewed {
            // What the server found belongs to the stored review, not only to
            // this loop: the screen reads that checkpoint, and so does the
            // approval that starts writing. Re-reading it adds nothing, because
            // both additions skip an issue already naming what they found.
            db::checkpoint(
                &ctx.pool,
                &ctx.id,
                &format!("outline_review:{}", plan.revision),
                &serde_json::to_value(&review)?,
            )
            .await?;
        }
        ctx.event("outline_review", json!({"stage":"outline_review","title":"목차의 누락·중복·순서 검토","revision":revision,"issues":review.issues})).await?;
        if review.issues.iter().all(|i| i.severity != "major") {
            // The approval names the plan's own revision, which is what the
            // API wrote and what a legacy candidate without the field reads as.
            let approved = db::load_checkpoint(&ctx.pool, &ctx.id, "outline_approved")
                .await?
                .and_then(|v| v.as_u64())
                == Some(u64::from(plan.revision));
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

/// Plan the whole document in one request.
async fn plan_whole(
    ctx: &RunContext,
    system: &str,
    discovery: &Discovery,
    view: &BranchView,
    feedback: &serde_json::Value,
    revision: u32,
) -> Result<Outline> {
    let mut reductions = 0u32;
    let mut previous_error = String::new();
    let mut repair = llm::JsonRepair::default();
    let branches = branch_view(&view.branches.iter().collect::<Vec<_>>());
    let labels = view.keys_by_label();
    let feedback = feedback_view(feedback, view, None, OUTLINE_VIEW_BYTES);
    for attempt in 0..3 {
        let brief = &discovery.brief;
        let mut input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
        "source_brief":brief,"supporting_findings":crate::purpose::context(discovery, true),"source_branches":branches,"feedback":feedback,"revision":revision,
        "max_diagrams":ctx.snapshot.task.max_diagrams,
        "previous_error":previous_error,"attempt":attempt+1,"instruction":format!("{} At most {} sections for this document. Respect user feedback and preserve valid existing section IDs when supplied.", *PLAN, section_ceiling(view.count()))});
        repair.apply(&mut input);
        if let Some(response) = &repair.response {
            input["previous_section_dependencies"] = dependency_repair_context(response);
        }
        let packed = pack_planning_evidence(ctx, discovery, &mut input, reductions, true)?;
        ctx.event("outline_planning", json!({"stage":"planning","title":"구현 근거에 맞춰 설명 순서 구성","attempt":attempt+1,"evidence_chunks":packed,"branches":branches.len(),"branch_topics":branches.iter().map(|b| b["topics"].as_array().map_or(0,Vec::len)).sum::<usize>()})).await?;
        let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let mut plan = decode_generated_outline(&s, &mut repair, &labels)?;
            plan.revision = revision;
            validate_outline(
                &mut plan,
                &discovery.evidence,
                ctx.snapshot.task.max_diagrams,
                view.count(),
            )?;
            Ok(plan)
        });
        match result {
            Ok(plan) => return Ok(plan),
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                previous_error = editorial::excerpt(&format!("{e:#}"), 1500);
                if previous_error.contains("CONTEXT_BUDGET") {
                    reductions += 1;
                }
                ctx.event(
                    "outline_validation",
                    json!({"stage":"planning","attempt":attempt+1,"error":previous_error}),
                )
                .await?;
            }
        }
    }
    bail!("AWAITING_OUTLINE: Invalid outline after three attempts: {previous_error}")
}

const CHAPTERS_TEMPLATE: &str = "Return JSON {chapters:[{title:string,summary:string,groups:[string],branches:[string]}],excluded_branches:[{branch:string,reason:string}],reader_goal:string,storyline:string}. The source is too large to plan section by section in one request, so first divide the document into chapters; each chapter's sections are planned next, from its own branches described in full. source_groups lists the parts of the source as the reading tree grouped them: each group has a label, where it lives, how many files it holds, its topic titles and its branches - with their place, size and topics while they fit, otherwise by label alone. A chapter takes whole groups by label in groups, and single branches by label in branches; a branch named in a chapter's branches belongs to that chapter even when its group is taken by another. Every branch must end up in exactly one chapter or be excluded: list a branch or a whole group in excluded_branches, as {branch: its label, reason}, with a short reason tied to the purpose. Judge each part against the purpose: one the purpose does not ask about is excluded however many files it holds, and a large source with a narrow purpose is a short document. A chapter gathers what a reader should meet together; it need not follow source_groups, which follow how the code is laid out rather than a reader's journey. Order chapters along source_brief's through-line, the way a reader should meet them, and keep them at one level of abstraction and distinct from each other. Use at most {MAX_CHAPTERS} chapters and fewer when the purpose is narrow; a chapter should be worth several sections. summary says in one to three sentences what the chapter explains and why it sits where it does. reader_goal briefly states what the document explains; storyline briefly explains the chapter order. Respect the user's explicit audience and scope. Keep titles under 300 UTF-8 bytes, summaries under 1500, reader_goal under 2000 and storyline under 4000. Use the requested language.";
static CHAPTERS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    CHAPTERS_TEMPLATE.replace("{MAX_CHAPTERS}", &MAX_CHAPTERS.to_string())
});
const CHAPTER_NOTE: &str = "This request plans one part of one chapter of a larger document. chapter names it, part says which of the chapter's parts this is, and document_chapters lists every chapter in reading order: plan sections only for the branches in source_branches, and leave everything else to the chapters and parts that own it. source_brief, supporting_findings and evidence hold only what the purpose reading found in these branches' files, and may be empty; the document's through-line is document_chapters. Every section names the branches or parts it covers. Account for every branch in source_branches in a section's branches or in excluded_branches. Do not reuse any title in earlier_section_titles. reader_goal and storyline describe this part.";

#[derive(Clone, Serialize, Deserialize)]
struct Chapter {
    title: String,
    summary: String,
    /// Branch node keys, in reading order.
    branches: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Chapters {
    chapters: Vec<Chapter>,
    #[serde(default)]
    excluded_branches: Vec<BranchExclusion>,
    reader_goal: String,
    storyline: String,
}

/// Resolve a list of branch labels in place, naming the item that used one the
/// request never supplied.
fn resolve_labels(
    value: &mut serde_json::Value,
    labels: &HashMap<String, String>,
    subject: &str,
) -> Result<()> {
    let Some(items) = value.as_array_mut() else {
        bail!("{subject}.branches must be an array of branch labels; use [] when empty");
    };
    let mut seen = HashSet::new();
    let mut resolved = vec![];
    for item in items.iter() {
        let label = item
            .as_str()
            .with_context(|| format!("{subject}.branches must hold branch label strings"))?
            .trim();
        let key = labels.get(label).with_context(|| {
            let mut known: Vec<&String> = labels.keys().collect();
            known.sort();
            format!(
                "{subject} names branch {label:?}, which is not a label in source_branches; use labels such as {:?}",
                known.iter().take(8).collect::<Vec<_>>()
            )
        })?;
        if seen.insert(key.clone()) {
            resolved.push(json!(key));
        }
    }
    *items = resolved;
    Ok(())
}

/// Resolve `excluded_branches` in place.
/// A branch a section covers may not also be excluded.
///
/// The plan accounts for every branch exactly one way: named in the sections
/// that cover it, or listed once in `excluded_branches` with a reason. A real
/// plan did both to one branch and nothing rejected it: coverage tests covered
/// first and never reads that branch's exclusion, so the section wrote what the
/// exclusion said the document would leave out, and the reason stayed in the
/// plan as a false statement about the document.
fn reject_covered_exclusions(
    value: &serde_json::Value,
    labels: &HashMap<String, String>,
) -> Result<()> {
    let by_key: HashMap<&str, &str> = labels
        .iter()
        .map(|(label, key)| (key.as_str(), label.as_str()))
        .collect();
    let covered: HashMap<&str, &str> = value
        .get("sections")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .flat_map(|section| {
            let title = section
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            section
                .get("branches")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(move |branch| branch.as_str().map(|branch| (branch, title)))
        })
        .collect();
    for item in value
        .get("excluded_branches")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let Some(branch) = item.get("branch").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(title) = covered.get(branch) {
            bail!(
                "Branch {:?} is both covered by section {title:?} and listed in excluded_branches; account for a branch one way only - keep it in that section and drop the exclusion, or drop it from that section's branches and keep the reason",
                by_key.get(branch).copied().unwrap_or(branch)
            );
        }
    }
    Ok(())
}

fn resolve_exclusions(
    value: &mut serde_json::Value,
    labels: &HashMap<String, String>,
) -> Result<()> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    let Some(items) = object.get_mut("excluded_branches") else {
        return Ok(());
    };
    if labels.is_empty() {
        *items = json!([]);
        return Ok(());
    }
    let Some(items) = items.as_array_mut() else {
        bail!(
            "excluded_branches must be an array of {{branch, reason}} objects; use [] when empty"
        );
    };
    for (index, item) in items.iter_mut().enumerate() {
        let subject = format!("excluded_branches[{index}]");
        let reason_ok = item
            .get("reason")
            .and_then(|v| v.as_str())
            .is_some_and(|r| bounded_text(r, 1500));
        ensure!(
            reason_ok,
            "{subject} needs a short reason tied to the purpose"
        );
        let mut branch = json!([item.get("branch").cloned().unwrap_or_default()]);
        resolve_labels(&mut branch, labels, &subject)?;
        item["branch"] = branch[0].clone();
    }
    Ok(())
}

/// Divide the branches of a large source into chapters.
async fn request_chapters(
    ctx: &RunContext,
    system: &str,
    discovery: &Discovery,
    view: &BranchView,
    feedback: &serde_json::Value,
) -> Result<Chapters> {
    let groups = group_view(view);
    let feedback = feedback_view(feedback, view, None, OUTLINE_VIEW_BYTES / 2);
    let mut previous_error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        ctx.event("outline_chapters", json!({"stage":"planning","title":"큰 소스를 장 단위로 나누기","attempt":attempt+1,"branches":view.branches.len(),"groups":view.groups.len()})).await?;
        let mut input = json!({"phase":"outline_chapters","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "source_brief":uncited(&discovery.brief),"source_groups":groups,"feedback":feedback,
            "previous_error":previous_error,"attempt":attempt+1,"instruction":CHAPTERS.as_str()});
        repair.apply(&mut input);
        let result = llm::call(ctx, system, input.clone())
            .await
            .and_then(|s| decode_chapters(&s, &mut repair, view));
        match result {
            Ok(chapters) => return Ok(chapters),
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                previous_error = editorial::excerpt(&format!("{e:#}"), 1500);
                ctx.event(
                    "outline_validation",
                    json!({"stage":"planning","phase":"chapters","attempt":attempt+1,"error":previous_error}),
                )
                .await?;
            }
        }
    }
    bail!("AWAITING_OUTLINE: Could not divide the document into chapters: {previous_error}")
}

/// Chapters as the model wrote them, before labels are resolved.
#[derive(Deserialize)]
struct ChapterDraft {
    title: String,
    summary: String,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    branches: Vec<String>,
}

#[derive(Deserialize)]
struct ChaptersDraft {
    chapters: Vec<ChapterDraft>,
    #[serde(default)]
    excluded_branches: Vec<BranchExclusion>,
    reader_goal: String,
    storyline: String,
}

/// Resolve a chapter division, so that every branch lands in exactly one
/// chapter or one exclusion. A branch named on its own wins over the group that
/// holds it; a group taken twice, a branch named twice, and any branch left
/// out are errors that name what to fix.
fn decode_chapters(
    response: &str,
    repair: &mut llm::JsonRepair,
    view: &BranchView,
) -> Result<Chapters> {
    let draft: ChaptersDraft = repair.decode(response)?;
    ensure!(
        (1..=MAX_CHAPTERS).contains(&draft.chapters.len()),
        "Supply 1-{MAX_CHAPTERS} chapters"
    );
    ensure!(
        bounded_text(&draft.reader_goal, 2000) && bounded_text(&draft.storyline, 4000),
        "Supply a bounded reader_goal and storyline"
    );
    let branch_index: HashMap<&str, usize> = view
        .branches
        .iter()
        .enumerate()
        .map(|(i, b)| (b.label.as_str(), i))
        .collect();
    let group_index: HashMap<&str, usize> = view
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.label.as_str(), i))
        .collect();
    let unknown = |label: &str, subject: &str| {
        anyhow::anyhow!(
            "{subject} names {label:?}, which is neither a group nor a branch label in source_groups"
        )
    };
    let mut titles = HashSet::new();
    let mut explicit: HashMap<usize, usize> = HashMap::new();
    let mut by_group: HashMap<usize, usize> = HashMap::new();
    for (index, chapter) in draft.chapters.iter().enumerate() {
        ensure!(
            bounded_text(&chapter.title, 299)
                && bounded_text(&chapter.summary, 1500)
                && titles.insert(
                    chapter
                        .title
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_lowercase()
                ),
            "chapters[{index}] needs a distinct title under 300 bytes and a summary under 1500 bytes"
        );
        for label in &chapter.branches {
            let branch = *branch_index
                .get(label.trim())
                .ok_or_else(|| unknown(label, &format!("chapters[{index}].branches")))?;
            if let Some(first) = explicit.insert(branch, index)
                && first != index
            {
                bail!(
                    "Branch {label} is named in both {:?} and {:?}; name each branch in one chapter",
                    draft.chapters[first].title,
                    chapter.title
                );
            }
        }
        for label in &chapter.groups {
            let group = *group_index
                .get(label.trim())
                .ok_or_else(|| unknown(label, &format!("chapters[{index}].groups")))?;
            if let Some(first) = by_group.insert(group, index)
                && first != index
            {
                bail!(
                    "Group {label} is taken by both {:?} and {:?}; give each group to one chapter and name single branches to move them",
                    draft.chapters[first].title,
                    chapter.title
                );
            }
        }
    }
    let mut excluded: HashMap<usize, String> = HashMap::new();
    for (index, exclusion) in draft.excluded_branches.iter().enumerate() {
        ensure!(
            bounded_text(&exclusion.reason, 1500),
            "excluded_branches[{index}] needs a short reason tied to the purpose"
        );
        let label = exclusion.branch.trim();
        let members: Vec<usize> = match (branch_index.get(label), group_index.get(label)) {
            (Some(branch), _) => vec![*branch],
            (None, Some(group)) => {
                if let Some(chapter) = by_group.get(group) {
                    bail!(
                        "Group {label} is taken by {:?} and excluded; choose one, or exclude single branches of it",
                        draft.chapters[*chapter].title
                    );
                }
                view.groups[*group].members.clone()
            }
            (None, None) => return Err(unknown(label, &format!("excluded_branches[{index}]"))),
        };
        for branch in members {
            if let Some(chapter) = explicit.get(&branch)
                && branch_index.contains_key(label)
            {
                bail!(
                    "Branch {label} is named in {:?} and excluded; choose one",
                    draft.chapters[*chapter].title
                );
            }
            excluded.insert(branch, exclusion.reason.clone());
        }
    }
    let mut owner: Vec<Option<usize>> = vec![None; view.branches.len()];
    for (group, chapter) in &by_group {
        for branch in &view.groups[*group].members {
            owner[*branch] = Some(*chapter);
        }
    }
    for branch in excluded.keys() {
        // An exclusion takes a branch out of the group that brought it in; a
        // branch named on its own was checked above.
        if !explicit.contains_key(branch) {
            owner[*branch] = None;
        }
    }
    for (branch, chapter) in &explicit {
        owner[*branch] = Some(*chapter);
    }
    let missing: Vec<&str> = view
        .branches
        .iter()
        .enumerate()
        .filter(|(i, _)| owner[*i].is_none() && !excluded.contains_key(i))
        .map(|(_, b)| b.label.as_str())
        .collect();
    ensure!(
        missing.is_empty(),
        "{} branches are in no chapter and not excluded: {}. Give each to a chapter, by group or by label, or exclude it with a reason",
        missing.len(),
        missing
            .iter()
            .take(24)
            .copied()
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut chapters: Vec<Chapter> = draft
        .chapters
        .into_iter()
        .map(|c| Chapter {
            title: c.title,
            summary: c.summary,
            branches: vec![],
        })
        .collect();
    for (branch, chapter) in owner.iter().enumerate() {
        if let Some(chapter) = chapter {
            chapters[*chapter]
                .branches
                .push(view.branches[branch].key.clone());
        }
    }
    for (index, chapter) in chapters.iter().enumerate() {
        ensure!(
            !chapter.branches.is_empty(),
            "chapters[{index}] ({:?}) ends up with no branch; give it groups or branches, or remove it",
            chapter.title
        );
    }
    let mut excluded: Vec<(usize, String)> = excluded
        .into_iter()
        .filter(|(branch, _)| owner[*branch].is_none())
        .collect();
    excluded.sort();
    Ok(Chapters {
        chapters,
        excluded_branches: excluded
            .into_iter()
            .map(|(branch, reason)| BranchExclusion {
                branch: view.branches[branch].key.clone(),
                reason,
            })
            .collect(),
        reader_goal: draft.reader_goal,
        storyline: draft.storyline,
    })
}

/// Divide `total` across parts by their branch counts: at least one each,
/// never more than `total` together.
fn chapter_shares(total: usize, weights: &[usize]) -> Vec<usize> {
    let sum = weights.iter().sum::<usize>().max(1);
    let mut shares: Vec<usize> = weights.iter().map(|w| (total * w / sum).max(1)).collect();
    while shares.iter().sum::<usize>() > total {
        let Some((largest, _)) = shares
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > 1)
            .max_by_key(|(_, s)| **s)
        else {
            break;
        };
        shares[largest] -= 1;
    }
    shares
}

/// Divide a document's diagram limit across parts by their branch counts,
/// exactly: a part may get none.
fn diagram_shares(total: u32, weights: &[usize]) -> Vec<u32> {
    let sum = weights.iter().sum::<usize>().max(1) as u64;
    let mut shares: Vec<u32> = weights
        .iter()
        .map(|w| (u64::from(total) * *w as u64 / sum) as u32)
        .collect();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|a, b| weights[*b].cmp(&weights[*a]).then(a.cmp(b)));
    let mut remaining = total.saturating_sub(shares.iter().sum());
    for index in order.iter().cycle().take(weights.len() * total as usize) {
        if remaining == 0 {
            break;
        }
        shares[*index] += 1;
        remaining -= 1;
    }
    shares
}

/// One planning request of a chapter: its branches, at most sixteen.
struct Batch {
    chapter: usize,
    part: usize,
    parts: usize,
    branches: Vec<String>,
}

/// Cut each chapter into requests of at most `CHAPTER_BRANCHES` branches, in
/// reading order, so a chapter holding sixty branches is planned from sixty
/// described branches rather than sixty one-line ones.
fn chapter_batches(chapters: &Chapters, view: &BranchView) -> Vec<Batch> {
    let mut batches = vec![];
    for (chapter, entry) in chapters.chapters.iter().enumerate() {
        let mut branches = entry.branches.clone();
        branches.sort_by_key(|key| view.index(key));
        let runs: Vec<&[String]> = branches.chunks(CHAPTER_BRANCHES).collect();
        for (part, run) in runs.iter().enumerate() {
            batches.push(Batch {
                chapter,
                part,
                parts: runs.len(),
                branches: run.to_vec(),
            });
        }
    }
    batches
}

/// Plan a large source in chapters: one request divides the branches, and each
/// chapter is planned in requests of at most sixteen branches. Every request is
/// bounded, so the document can follow the source's size without any request
/// describing a part of it in a single sentence.
async fn plan_in_chapters(
    ctx: &RunContext,
    system: &str,
    discovery: &Discovery,
    view: &BranchView,
    feedback: &serde_json::Value,
    revision: u32,
) -> Result<Outline> {
    let chapters_key = format!("outline_chapters:{revision}");
    let chapters: Chapters = match db::load_checkpoint(&ctx.pool, &ctx.id, &chapters_key).await? {
        Some(saved) => serde_json::from_value(saved)?,
        None => {
            let chapters = request_chapters(ctx, system, discovery, view, feedback).await?;
            db::checkpoint(
                &ctx.pool,
                &ctx.id,
                &chapters_key,
                &serde_json::to_value(&chapters)?,
            )
            .await?;
            chapters
        }
    };
    let batches = chapter_batches(&chapters, view);
    let weights: Vec<usize> = batches.iter().map(|b| b.branches.len()).collect();
    let sections_per_batch = chapter_shares(section_ceiling(view.count()), &weights);
    let diagrams_per_batch = ctx
        .snapshot
        .task
        .max_diagrams
        .map(|total| diagram_shares(total, &weights));
    let outline_of_chapters: Vec<serde_json::Value> = chapters
        .chapters
        .iter()
        .enumerate()
        .map(|(index, c)| json!({"index":index,"title":c.title,"summary":c.summary}))
        .collect();
    let mut sections: Vec<SectionPlan> = vec![];
    let mut excluded = chapters.excluded_branches.clone();
    for (index, batch) in batches.iter().enumerate() {
        let key = format!(
            "outline_chapter:{revision}:{}:{}",
            batch.chapter, batch.part
        );
        let part: Outline = match db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? {
            Some(saved) => serde_json::from_value(saved)?,
            None => {
                let earlier: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
                let part = plan_chapter(
                    ctx,
                    system,
                    discovery,
                    view,
                    ChapterRequest {
                        chapters: &outline_of_chapters,
                        batch,
                        sections: sections_per_batch[index],
                        diagrams: diagrams_per_batch.as_ref().map(|d| d[index]),
                        earlier: &earlier,
                        feedback,
                        revision,
                    },
                )
                .await?;
                db::checkpoint(&ctx.pool, &ctx.id, &key, &serde_json::to_value(&part)?).await?;
                part
            }
        };
        let offset = sections.len();
        for mut section in part.sections {
            section.depends_on = section.depends_on.iter().map(|d| d + offset).collect();
            sections.push(section);
        }
        excluded.extend(part.excluded_branches);
    }
    let mut plan = Outline {
        sections,
        reader_goal: chapters.reader_goal,
        storyline: chapters.storyline,
        terminology: vec![],
        requirements: vec![],
        revision,
        excluded_branches: excluded,
    };
    validate_outline(
        &mut plan,
        &discovery.evidence,
        ctx.snapshot.task.max_diagrams,
        view.count(),
    )
    .map_err(|e| anyhow::anyhow!("AWAITING_OUTLINE: chapter plans could not be joined: {e:#}"))?;
    Ok(plan)
}

struct ChapterRequest<'a> {
    chapters: &'a [serde_json::Value],
    batch: &'a Batch,
    sections: usize,
    diagrams: Option<u32>,
    earlier: &'a [&'a str],
    feedback: &'a serde_json::Value,
    revision: u32,
}

async fn plan_chapter(
    ctx: &RunContext,
    system: &str,
    discovery: &Discovery,
    view: &BranchView,
    request: ChapterRequest<'_>,
) -> Result<Outline> {
    let batch = request.batch;
    let members: Vec<&Branch> = batch
        .branches
        .iter()
        .filter_map(|key| view.get(key))
        .collect();
    let labels: HashMap<String, String> = members
        .iter()
        .flat_map(|b| {
            std::iter::once((b.label.clone(), b.key.clone()))
                .chain(b.parts.iter().map(|p| (p.label.clone(), p.key.clone())))
        })
        .collect();
    let scope: HashSet<String> = batch.branches.iter().cloned().collect();
    let files: HashSet<String> = members
        .iter()
        .flat_map(|b| b.files.iter().cloned())
        .collect();
    let scoped = scope_discovery(discovery, &files, &ctx.snapshot.task.sources);
    let branches = branch_view(&members);
    let feedback = feedback_view(request.feedback, view, Some(&scope), OUTLINE_VIEW_BYTES / 2);
    let normalize = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let earlier: HashSet<String> = request.earlier.iter().map(|t| normalize(t)).collect();
    let shown_earlier: Vec<String> = request
        .earlier
        .iter()
        .map(|t| editorial::excerpt(t, 120))
        .collect();
    let mut reductions = 0u32;
    let mut previous_error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        let mut input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "document_chapters":request.chapters,"chapter":request.chapters[batch.chapter],
            "part":{"part":batch.part+1,"parts":batch.parts},
            "earlier_section_titles":shown_earlier,
            "source_brief":scoped.brief,"supporting_findings":crate::purpose::context(&scoped, true),
            "source_branches":branches,
            "feedback":feedback,"revision":request.revision,"max_diagrams":request.diagrams,
            "previous_error":previous_error,"attempt":attempt+1,
            "instruction":format!("{} {CHAPTER_NOTE} At most {} sections for this part.", *PLAN, request.sections)});
        repair.apply(&mut input);
        if let Some(response) = &repair.response {
            input["previous_section_dependencies"] = dependency_repair_context(response);
        }
        let packed = pack_planning_evidence(ctx, &scoped, &mut input, reductions, false)?;
        ctx.event("outline_planning", json!({"stage":"planning","title":"장별 설명 순서 구성","chapter":batch.chapter+1,"chapters":request.chapters.len(),"part":batch.part+1,"parts":batch.parts,"attempt":attempt+1,"evidence_chunks":packed,"branches":branches.len()})).await?;
        let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let mut part = decode_generated_outline(&s, &mut repair, &labels)?;
            part.revision = request.revision;
            validate_outline(&mut part, &scoped.evidence, request.diagrams, None)?;
            ensure!(
                part.sections.len() <= request.sections,
                "Supply at most {} sections for this part",
                request.sections
            );
            let reused: Vec<&str> = part
                .sections
                .iter()
                .filter(|s| earlier.contains(&normalize(&s.title)))
                .map(|s| s.title.as_str())
                .collect();
            ensure!(
                reused.is_empty(),
                "Section titles {reused:?} are already used by earlier chapters; choose distinct titles"
            );
            Ok(part)
        });
        match result {
            Ok(part) => return Ok(part),
            Err(e) if fatal(&e) || is_budget(&e) => return Err(e),
            Err(e) => {
                llm::forget(ctx, system, input).await?;
                previous_error = editorial::excerpt(&format!("{e:#}"), 1500);
                if previous_error.contains("CONTEXT_BUDGET") {
                    reductions += 1;
                }
                ctx.event(
                    "outline_validation",
                    json!({"stage":"planning","chapter":batch.chapter+1,"part":batch.part+1,"attempt":attempt+1,"error":previous_error}),
                )
                .await?;
            }
        }
    }
    bail!(
        "AWAITING_OUTLINE: Invalid outline for chapter {} part {} after three attempts: {previous_error}",
        batch.chapter + 1,
        batch.part + 1
    )
}

async fn review_outline(
    ctx: &RunContext,
    system: &str,
    plan: &Outline,
    discovery: &Discovery,
    view: &BranchView,
    coverage: &serde_json::Value,
) -> Result<OutlineReview> {
    let key = format!("outline_review:{}", plan.revision);
    if let Some(saved) = db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? {
        return Ok(serde_json::from_value(saved)?);
    }
    let shown_plan = outline_view(
        plan,
        &view.labels_by_key(),
        &|_, _| true,
        OUTLINE_VIEW_BYTES,
    );
    let mut error = String::new();
    let mut repair = llm::JsonRepair::default();
    for attempt in 0..3 {
        let mut input = json!({"phase":"outline_review","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,"outline":shown_plan,"source_brief":uncited(&discovery.brief),"supporting_findings":crate::purpose::context(discovery, false),"attempt":attempt,"previous_error":error,"instruction":"Review the section grouping before writing. Return JSON {issues:[{severity:'major'|'minor',code:string,message:string,section_ids:[string],query:string}]}. Check only clear omission of the user's explicit scope, substantial duplicate coverage, incoherent reading order and explicit section/diagram constraints. Do not require generated questions, ownership assignments, handoffs or formal prerequisite metadata. Shared evidence is valid when sections explain different behavior. Detailed implementation verification belongs to section writing and source review. An unresolved source link alone is not a defective section grouping; preserve it for deeper reading. Report a major issue only when a concrete scope or structure defect would prevent a useful document. Name affected section IDs exactly as outline shows them and a specific correction; query may name observed sources when necessary. outline shows each section's key points shortened as the plan grows; do not report a shortened key point as missing detail. Do not infer missing code from omitted excerpts or require every module to get a section. Return an empty issues array when there is no concrete structural defect. Use the requested language. At most 8 issues."});
        if !coverage.is_null() {
            input["branch_coverage"] = coverage_view(coverage);
            input["coverage_rules"] = json!(
                "branch_coverage is computed from the plan, not asserted by it: every part of the source the reading found is covered by a section's branches, excluded with a reason, or unassigned. A large branch the plan split into parts (labels like B3.2) is judged part by part. An unassigned branch or part the purpose needs is a major missing_topic issue: name it and the section that should take it, or say that a new section is needed. One the purpose does not need is at most minor. An exclusion whose reason contradicts the purpose is major. crowded_sections lists sections that took four or more branches or parts: when those serve different workflows the purpose needs explained, report a major granularity issue naming the section id and how to split it; when they are one workflow, or the purpose needs only an overview of them, at most minor. Covered and excluded branches need no comment."
            );
        }
        repair.apply(&mut input);
        let parsed = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let mut r: OutlineReview = repair.decode(&s)?;
            let resolved = r
                .issues
                .iter_mut()
                .all(|i| resolve_section_ids(&mut i.section_ids, plan));
            ensure!(
                resolved
                    && r.issues.len() <= 8
                    && r.issues
                        .iter()
                        .all(|i| ["major", "minor"].contains(&i.severity.as_str())
                            && bounded_text(&i.message, 4000)
                            && bounded_text(&i.code, 100)
                            && i.query.len() <= 2000),
                "Invalid outline review issue or reference; section_ids must be ids shown in outline"
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
        .iter()
        .filter(|e| plan.evidence_ids.contains(&e.id))
        .cloned()
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

    #[test]
    fn a_branch_says_where_it_lives_and_the_views_are_related() -> Result<()> {
        // One directory for a branch that sits in one; the nearest shared one
        // when it spans several; nothing when it spans the whole tree.
        let at =
            |paths: &[&str]| shared_root(&paths.iter().map(|p| p.to_string()).collect::<Vec<_>>());
        assert_eq!(at(&["a/b/one.rs", "a/b/two.rs"]), "a/b");
        assert_eq!(at(&["a/b/one.rs", "a/c/two.rs"]), "a");
        assert_eq!(at(&["a/one.rs", "b/two.rs"]), "");
        assert_eq!(at(&[]), "");
        // A prefix that is not a directory boundary must not match.
        assert_eq!(at(&["a/bc/one.rs", "a/bd/two.rs"]), "a");
        // Paths are shown against the source the user named, so a branch does
        // not repeat the same long prefix in `where` and in every file it lists.
        let roots = vec!["/w/proj".to_string(), "/w/proj/vendor".to_string()];
        assert_eq!(relative("/w/proj/backend/a.rs", &roots), "backend/a.rs");
        // The nearest root wins when they nest.
        assert_eq!(relative("/w/proj/vendor/b.rs", &roots), "b.rs");
        // A path under no named source is left as it is rather than mangled.
        assert_eq!(relative("/elsewhere/c.rs", &roots), "/elsewhere/c.rs");
        // A trailing slash on the source must not leave a leading one behind.
        assert_eq!(relative("/w/p/a.rs", &["/w/p/".to_string()]), "a.rs");
        // The planner is told the three views are one thing at three depths,
        // and which order the branches are in, or it merges them as three
        // unrelated lists and the narrative wanders.
        assert!(PLAN.contains("one thing at three depths"), "{}", *PLAN);
        // The spine is the through-line, not the order the files happen to sit
        // in: a file layout is not a reader's journey.
        assert!(
            PLAN.contains("spine is source_brief's through-line"),
            "{}",
            *PLAN
        );
        assert!(
            PLAN.contains("must not become the document's structure"),
            "{}",
            *PLAN
        );
        Ok(())
    }

    #[test]
    fn branches_raise_the_ceiling_without_obliging_a_section() -> Result<()> {
        // The branch view is read from the understanding tree, which knows
        // nothing of the purpose. Telling the planner to cover every branch
        // therefore spent sections on parts the reader never asked about: a
        // large source with a narrow purpose is a short document.
        assert!(
            PLAN.contains("Choose the smallest number of sections"),
            "{}",
            *PLAN
        );
        assert!(
            PLAN.contains("not what this document owes a section"),
            "{}",
            *PLAN
        );
        assert!(PLAN.contains("gets no section"), "{}", *PLAN);
        // And the ceiling is only a ceiling: nothing requires a plan to reach it.
        let source = evidence("/project/a.py", "def process(): return 1");
        let mut plan: Outline = serde_json::from_value(json!({
            "reader_goal":"Understand processing","storyline":"One step",
            "terminology":[],"sections":[{"title":"Only","query":"process",
                "key_points":["Describe it"],"evidence_ids":[source.id],"diagrams":[]}]
        }))?;
        validate_outline(&mut plan, std::slice::from_ref(&source), Some(0), Some(40))?;
        Ok(())
    }

    #[test]
    fn the_document_widens_with_the_source_rather_than_a_fixed_ceiling() {
        // The tree fans in by four, so a larger source only deepens it. Picking
        // the branch level by an absolute threshold therefore described 520
        // leaves with fewer branches than 52; the target follows the leaves.
        assert!(branch_target(520) > branch_target(52));
        assert!(branch_target(5200) >= branch_target(520));
        // A small project is not squeezed by the same rule.
        assert_eq!(branch_target(4), 8);
        // Sections follow the branches, with a floor for a small project and an
        // absolute limit because one file has to end somewhere.
        assert!(
            section_ceiling(Some(branch_target(520))) > section_ceiling(Some(branch_target(52)))
        );
        // Never tighter than the flat ceiling it replaced: the change was to
        // let a large source have more, not to take from a small one.
        assert_eq!(section_ceiling(Some(0)), SECTIONS_WITHOUT_BRANCHES);
        assert!(
            (0..=SECTIONS_MAX).all(|n| section_ceiling(Some(n)) >= SECTIONS_WITHOUT_BRANCHES),
            "a branch count must never narrow the document"
        );
        assert_eq!(section_ceiling(Some(usize::MAX)), SECTIONS_MAX);
        // Levels step by the fan-in, so the first past the target can overshoot
        // it several times; the closer of the two is chosen.
        assert!(closer_level(21, 82, 64));
        assert!(!closer_level(60, 300, 64));
        // The old flat ceiling is no longer the answer for a large source.
        assert!(section_ceiling(Some(branch_target(5200))) > 32);
        // An outline re-read without its branches keeps the widest bound, or one
        // that was valid when planned would be rejected on the next read.
        assert_eq!(section_ceiling(None), SECTIONS_MAX);
    }

    fn test_branch(index: usize, files: usize, topics: usize) -> Branch {
        Branch {
            label: format!("B{}", index + 1),
            key: format!("understanding:node:{index}"),
            findings: (0..topics)
                .map(|i| Finding {
                    topic: format!("주제 {i}"),
                    observation: "관".repeat(4000),
                    kind: FindingKind::Context,
                    evidence_ids: vec![],
                })
                .collect(),
            files: (0..files).map(|i| format!("src/m{i}.rs")).collect(),
            whole: true,
            parts: vec![],
        }
    }

    #[test]
    fn a_branch_view_describes_every_branch_inside_one_budget() -> Result<()> {
        // A wide tree must not spend the budget on its first branches and leave
        // the rest out of the document: every branch is described.
        let wide: Vec<Branch> = (0..16).map(|i| test_branch(i, 3, MAX_FINDINGS)).collect();
        let view = branch_view(&wide.iter().collect::<Vec<_>>());
        assert_eq!(view.len(), 16);
        assert_eq!(view[0]["file_count"], 3);
        // Every branch carries the label a plan names it by.
        assert_eq!(view[15]["branch"], "B16");
        assert!(
            view.iter()
                .all(|b| b["topics"].as_array().is_some_and(|t| !t.is_empty()))
        );
        // The budget is measured, not divided and hoped for: dividing it and
        // flooring each share spent five times the allowance once the tree was
        // wide, and the planning request reserves exactly this much.
        for count in [2usize, 16, 64] {
            let level: Vec<Branch> = (0..count)
                .map(|i| test_branch(i, 3, MAX_FINDINGS))
                .collect();
            let wide = branch_view(&level.iter().collect::<Vec<_>>());
            assert_eq!(wide.len(), count, "every branch is described");
            let size = serde_json::to_vec(&wide)?.len();
            assert!(size <= BRANCH_VIEW_BYTES, "{count} branches took {size}");
            assert!(
                wide.iter()
                    .all(|b| b["topics"].as_array().is_some_and(|t| !t.is_empty())),
                "{count} branches left one silent"
            );
            // A branch whose lists were trimmed still says how much it holds,
            // and where it lives - a file list and two counts left the planner
            // guessing what the group was even for.
            assert_eq!(wide[0]["topic_count"], MAX_FINDINGS);
            assert_eq!(wide[0]["file_count"], 3);
            assert_eq!(wide[0]["where"], "src");
        }
        // Wider than the budget can describe at all: carry what fits and say how
        // many were left out, rather than presenting a prefix as the whole source.
        let huge: Vec<Branch> = (0..200).map(|i| test_branch(i, 3, MAX_FINDINGS)).collect();
        let view = branch_view(&huge.iter().collect::<Vec<_>>());
        assert!(serde_json::to_vec(&view)?.len() <= BRANCH_VIEW_BYTES);
        let omitted = view
            .last()
            .and_then(|v| v["branches_not_shown"].as_u64())
            .context("an omission must be named")?;
        assert_eq!(omitted as usize + view.len() - 1, 200);
        // The file count travels even when the list is capped, so a large branch
        // is recognisable as large.
        let big = [test_branch(0, 40, 1)];
        let big = branch_view(&big.iter().collect::<Vec<_>>());
        assert_eq!(big[0]["file_count"], 40);
        assert_eq!(big[0]["files"].as_array().map(Vec::len), Some(12));
        // A walk that could not reach every leaf says so, or an undercount
        // reads as a small branch and the branch earns fewer sections - the
        // very failure taking the count from the leaves was meant to end.
        let mut partial = test_branch(0, 1, 1);
        partial.whole = false;
        let partial = branch_view(&[&partial]);
        assert_eq!(partial[0]["file_count_is_a_floor"], true);
        assert!(view[0]["file_count_is_a_floor"].is_null());
        // Fewer branches each get more room than many do.
        let narrow = [
            test_branch(0, 1, MAX_FINDINGS),
            test_branch(1, 1, MAX_FINDINGS),
        ];
        let narrow = branch_view(&narrow.iter().collect::<Vec<_>>());
        let narrow_len = narrow[0]["topics"][0]["observation"]
            .as_str()
            .unwrap_or("")
            .len();
        let wide_len = view[0]["topics"][0]["observation"]
            .as_str()
            .unwrap_or("")
            .len();
        assert!(narrow_len > wide_len, "{narrow_len} vs {wide_len}");
        Ok(())
    }

    #[test]
    fn a_branch_spread_across_directories_names_where_its_files_are() {
        let files: Vec<String> = [
            "api/a.rs", "api/b.rs", "db/c.rs", "ui/d.rs", "ui/e.rs", "ui/f.rs",
        ]
        .iter()
        .map(|p| p.to_string())
        .collect();
        assert_eq!(shared_root(&files), "");
        let dirs = main_dirs(&files, "");
        assert_eq!(dirs[0], json!(["ui", 3]));
        assert_eq!(dirs.len(), 3);
        // A branch in one directory is already named by `where`.
        let local: Vec<String> = vec!["api/a.rs".into(), "api/b.rs".into()];
        assert!(main_dirs(&local, "api").is_empty());
    }

    #[test]
    fn every_branch_is_covered_excluded_or_reported_unassigned() -> Result<()> {
        let view = BranchView {
            branches: (0..3).map(|i| test_branch(i, 2, 2)).collect(),
            groups: vec![],
        };
        let labels = view.keys_by_label();
        // Labels resolve to node keys; a label the request never showed is named.
        let response = json!({"reader_goal":"goal","storyline":"story","sections":[
            {"title":"A","query":"a","key_points":["a"],"evidence_ids":[],"diagrams":[],"branches":["B1","B1"]}],
            "excluded_branches":[{"branch":"B2","reason":"설치 절차는 목적 밖이다"}]});
        let mut plan = decode_generated_outline(
            &response.to_string(),
            &mut llm::JsonRepair::default(),
            &labels,
        )?;
        assert_eq!(
            plan.sections[0].branches,
            vec!["understanding:node:0".to_string()]
        );
        assert_eq!(plan.excluded_branches[0].branch, "understanding:node:1");
        // A section planned from branches needs no anchor from elsewhere.
        validate_outline(&mut plan, &[], Some(0), view.count())?;
        let coverage = branch_coverage(&plan, &view);
        assert_eq!(coverage["covered"], 1);
        assert_eq!(coverage["excluded"][0]["branch"], "B2");
        assert_eq!(coverage["unassigned"][0]["branch"], "B3");
        assert_eq!(coverage["unassigned"].as_array().map(Vec::len), Some(1));
        // What no section took and no exclusion named becomes an issue whether
        // or not the review named it, so the next plan has to decide.
        let judged = crate::model::OutlineIssue {
            severity: "minor".into(),
            code: "missing_topic".into(),
            message: "B3 is a test harness the purpose does not ask about".into(),
            section_ids: vec![],
            requirement_ids: vec![],
            query: String::new(),
        };
        assert!(unassigned_issues(&coverage, std::slice::from_ref(&judged)).is_empty());
        // A review that named a longer label has not answered for this one.
        let other = crate::model::OutlineIssue {
            message: "B31 and B3.2 are covered by section two".into(),
            ..judged.clone()
        };
        assert_eq!(
            unassigned_issues(&coverage, std::slice::from_ref(&other)).len(),
            1
        );
        assert!(names_label("B3 is unassigned", "B3"));
        assert!(names_label("sections cover B3, and B4", "B3"));
        assert!(!names_label("B31 is crowded", "B3"));
        assert!(!names_label("B3.2 is crowded", "B3"));
        let raised = unassigned_issues(&coverage, &[]);
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].severity, "major");
        assert_eq!(raised[0].code, "unassigned_branch");
        assert!(raised[0].message.contains("B3"), "{}", raised[0].message);
        // A branch cannot be explained and excluded at once.
        let both = json!({"reader_goal":"goal","storyline":"story","sections":[
            {"title":"A","query":"a","key_points":["a"],"evidence_ids":[],"diagrams":[],"branches":["B1","B2"]}],
            "excluded_branches":[{"branch":"B2","reason":"설치 절차는 목적 밖이다"}]});
        let contradiction = decode_generated_outline(
            &both.to_string(),
            &mut llm::JsonRepair::default(),
            &labels,
        )
        .err()
        .context("a branch both covered and excluded must not decode")?
        .to_string();
        assert!(contradiction.contains("B2"), "{contradiction}");
        assert!(contradiction.contains("excluded_branches"), "{contradiction}");

        let unknown = json!({"reader_goal":"goal","storyline":"story","sections":[
            {"title":"A","query":"a","key_points":["a"],"evidence_ids":[],"diagrams":[],"branches":["B9"]}]});
        let error = decode_generated_outline(
            &unknown.to_string(),
            &mut llm::JsonRepair::default(),
            &labels,
        )
        .err()
        .context("an unknown branch label must be rejected")?
        .to_string();
        assert!(
            error.contains("B9") && error.contains("sections[0]"),
            "{error}"
        );
        let unreasoned = json!({"reader_goal":"goal","storyline":"story","sections":[],
            "excluded_branches":[{"branch":"B1","reason":""}]});
        assert!(
            decode_generated_outline(
                &unreasoned.to_string(),
                &mut llm::JsonRepair::default(),
                &labels
            )
            .is_err()
        );
        // Without branches a section still needs an anchor.
        plan.sections[0].branches.clear();
        assert!(validate_outline(&mut plan, &[], Some(0), view.count()).is_err());
        // Stored plans are shown to a request by label, never by node key.
        let shown = outline_view(
            &plan,
            &view.labels_by_key(),
            &|_, _| true,
            OUTLINE_VIEW_BYTES,
        );
        assert!(
            !shown.to_string().contains("understanding:node:"),
            "{shown}"
        );
        // A plan made without a branch view keeps no branches it was not shown.
        let plain = decode_generated_outline(
            &response.to_string(),
            &mut llm::JsonRepair::default(),
            &HashMap::new(),
        )?;
        assert!(plain.sections[0].branches.is_empty() && plain.excluded_branches.is_empty());
        assert!(branch_coverage(&plain, &BranchView::default()).is_null());
        Ok(())
    }

    #[test]
    fn a_large_source_is_divided_into_chapters_that_account_for_every_branch() -> Result<()> {
        let view = BranchView {
            branches: (0..40).map(|i| test_branch(i, 2, 3)).collect(),
            groups: (0..5)
                .map(|g| Group {
                    label: format!("G{}", g + 1),
                    topics: vec!["그룹 주제".into()],
                    members: (g * 8..g * 8 + 8).collect(),
                })
                .collect(),
        };
        assert!(view.chapters_needed());
        // The chapter view names every branch within one budget.
        let groups = group_view(&view);
        assert!(serde_json::to_vec(&groups)?.len() <= BRANCH_VIEW_BYTES);
        assert_eq!(
            groups
                .iter()
                .map(|g| g["branches"].as_array().map_or(0, Vec::len))
                .sum::<usize>(),
            40
        );
        let chapter = |range: std::ops::Range<usize>| {
            json!({"title":format!("장 {}", range.start),"summary":"요약",
                "branches":range.map(|i| format!("B{}", i + 1)).collect::<Vec<_>>()})
        };
        let complete = json!({"reader_goal":"goal","storyline":"story",
            "chapters":[chapter(0..20), chapter(20..39)],
            "excluded_branches":[{"branch":"B40","reason":"테스트 도구는 목적 밖이다"}]});
        let chapters = decode_chapters(
            &complete.to_string(),
            &mut llm::JsonRepair::default(),
            &view,
        )?;
        assert_eq!(chapters.chapters[1].branches.len(), 19);
        // A branch in no chapter and not excluded is named, not silently lost.
        let missing = json!({"reader_goal":"goal","storyline":"story",
            "chapters":[chapter(0..20), chapter(20..38)],"excluded_branches":[]});
        let error = decode_chapters(&missing.to_string(), &mut llm::JsonRepair::default(), &view)
            .err()
            .context("a missing branch must be rejected")?
            .to_string();
        assert!(error.contains("B39") && error.contains("B40"), "{error}");
        // A branch in two chapters is rejected.
        let twice = json!({"reader_goal":"goal","storyline":"story",
            "chapters":[chapter(0..21), chapter(20..40)],"excluded_branches":[]});
        assert!(
            decode_chapters(&twice.to_string(), &mut llm::JsonRepair::default(), &view,).is_err()
        );
        // Sections and diagrams are shared by branch count without exceeding the
        // document's limits.
        let sections = chapter_shares(64, &[20, 19, 1]);
        assert!(sections.iter().sum::<usize>() <= 64 && sections.iter().all(|s| *s >= 1));
        assert!(sections[0] > sections[2]);
        assert_eq!(chapter_shares(3, &[1, 1, 1, 1]).iter().sum::<usize>(), 4);
        let diagrams = diagram_shares(5, &[20, 19, 1]);
        assert_eq!(diagrams.iter().sum::<u32>(), 5);
        assert_eq!(diagram_shares(0, &[3, 3]), vec![0, 0]);
        assert!(CHAPTERS.contains("exactly one chapter") && !CHAPTERS.contains("{MAX_"));
        // A chapter can take a whole group by label; a branch named on its own
        // leaves the group for the chapter that names it; a group can be
        // excluded whole.
        let by_group = json!({"reader_goal":"goal","storyline":"story",
            "chapters":[{"title":"앞","summary":"요약","groups":["G1","G2"],"branches":["B20"]},
                        {"title":"뒤","summary":"요약","groups":["G3","G4"]}],
            "excluded_branches":[{"branch":"G5","reason":"테스트 도구는 목적 밖이다"},{"branch":"B1","reason":"예제는 목적 밖이다"}]});
        let chapters = decode_chapters(
            &by_group.to_string(),
            &mut llm::JsonRepair::default(),
            &view,
        )?;
        let first: HashSet<&str> = chapters.chapters[0]
            .branches
            .iter()
            .map(String::as_str)
            .collect();
        assert!(
            first.contains("understanding:node:19"),
            "B20 moved to the chapter naming it"
        );
        assert!(
            !first.contains("understanding:node:0"),
            "B1 excluded from its group"
        );
        assert_eq!(chapters.chapters[1].branches.len(), 15);
        assert_eq!(chapters.excluded_branches.len(), 9);
        // Branches of a chapter stay in reading order.
        assert!(
            chapters.chapters[0]
                .branches
                .windows(2)
                .all(|w| view.index(&w[0]) < view.index(&w[1]))
        );
        for (bad, expected) in [
            (
                json!({"reader_goal":"g","storyline":"s","chapters":[
                {"title":"a","summary":"s","groups":["G1","G2","G3"]},{"title":"b","summary":"s","groups":["G3","G4","G5"]}]}),
                "Group G3",
            ),
            (
                json!({"reader_goal":"g","storyline":"s","chapters":[
                {"title":"a","summary":"s","groups":["G1","G2","G3","G4","G5"]}],
                "excluded_branches":[{"branch":"G2","reason":"r"}]}),
                "taken by",
            ),
            (
                json!({"reader_goal":"g","storyline":"s","chapters":[
                {"title":"a","summary":"s","groups":["G1","G2","G3","G4","G9"]}]}),
                "G9",
            ),
        ] {
            let error = decode_chapters(&bad.to_string(), &mut llm::JsonRepair::default(), &view)
                .err()
                .context("an invalid division must be rejected")?
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
        // Each chapter is planned sixteen branches to a request.
        let big = Chapters {
            chapters: vec![Chapter {
                title: "전체".into(),
                summary: "요약".into(),
                branches: view.branches.iter().rev().map(|b| b.key.clone()).collect(),
            }],
            excluded_branches: vec![],
            reader_goal: "g".into(),
            storyline: "s".into(),
        };
        let batches = chapter_batches(&big, &view);
        assert_eq!(
            batches.iter().map(|b| b.branches.len()).collect::<Vec<_>>(),
            [16, 16, 8]
        );
        assert_eq!((batches[2].part, batches[2].parts), (2, 3));
        assert_eq!(batches[0].branches[0], "understanding:node:0");
        Ok(())
    }

    #[test]
    fn a_large_branch_is_split_into_parts_and_judged_part_by_part() -> Result<()> {
        let mut large = test_branch(0, 8, 2);
        large.parts = (0..3)
            .map(|i| Part {
                label: format!("B1.{}", i + 1),
                key: format!("understanding:node:0.{i}"),
                topics: vec![format!("부분 {i}")],
                files: 2,
            })
            .collect();
        let view = BranchView {
            branches: vec![large, test_branch(1, 2, 2)],
            groups: vec![],
        };
        let rendered = branch_view(&view.branches.iter().collect::<Vec<_>>());
        assert_eq!(rendered[0]["parts"][2]["branch"], "B1.3");
        // A full single request - sixteen branches of four parts, every topic
        // a real sentence - still carries every part inside the budget.
        let dense: Vec<Branch> = (0..CHAPTER_BRANCHES)
            .map(|i| {
                let mut b = test_branch(i, 30, MAX_FINDINGS);
                for f in &mut b.findings {
                    f.topic = "요청 처리와 취소 전파를 담당하는 실행 흐름".into();
                }
                b.parts = (0..4)
                    .map(|k| Part {
                        label: format!("B{}.{}", i + 1, k + 1),
                        key: format!("understanding:node:{i}.{k}"),
                        topics: vec!["하위 흐름의 입력 검증과 결과 반환".into(); 12],
                        files: 8,
                    })
                    .collect();
                b
            })
            .collect();
        let dense_view = branch_view(&dense.iter().collect::<Vec<_>>());
        assert!(serde_json::to_vec(&dense_view)?.len() <= BRANCH_VIEW_BYTES);
        assert!(
            dense_view
                .iter()
                .all(|b| b["parts"].as_array().is_some_and(|p| p.len() == 4)),
            "{dense_view:?}"
        );
        let labels = view.keys_by_label();
        assert_eq!(labels["B1.2"], "understanding:node:0.1");
        assert_eq!(
            view.owner("understanding:node:0.1")
                .map(|b| b.label.as_str()),
            Some("B1")
        );
        let response = json!({"reader_goal":"goal","storyline":"story","sections":[
            {"title":"A","query":"a","key_points":["a"],"evidence_ids":[],"diagrams":[],"branches":["B1.1"]},
            {"title":"B","query":"b","key_points":["b"],"evidence_ids":[],"diagrams":[],"branches":["B1.3","B2"]}],
            "excluded_branches":[]});
        let plan = decode_generated_outline(
            &response.to_string(),
            &mut llm::JsonRepair::default(),
            &labels,
        )?;
        let coverage = branch_coverage(&plan, &view);
        // The part nobody took is named, not hidden inside a covered branch.
        assert_eq!(coverage["covered"], 2);
        assert_eq!(coverage["unassigned"].as_array().map(Vec::len), Some(1));
        assert_eq!(coverage["unassigned"][0]["branch"], "B1.2");
        assert!(
            coverage["crowded_sections"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        // A section that takes many branches at once is named for review.
        let wide = BranchView {
            branches: (0..6).map(|i| test_branch(i, 3, 1)).collect(),
            groups: vec![],
        };
        let crowded = json!({"reader_goal":"goal","storyline":"story","sections":[
            {"title":"전부","query":"q","key_points":["k"],"evidence_ids":[],"diagrams":[],"branches":["B1","B2","B3","B4"]},
            {"title":"나머지","query":"q","key_points":["k"],"evidence_ids":[],"diagrams":[],"branches":["B5","B6"]}]});
        let mut plan = decode_generated_outline(
            &crowded.to_string(),
            &mut llm::JsonRepair::default(),
            &wide.keys_by_label(),
        )?;
        validate_outline(&mut plan, &[], None, wide.count())?;
        let coverage = branch_coverage(&plan, &wide);
        let listed = coverage["crowded_sections"].as_array().context("crowded")?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["title"], "전부");
        assert_eq!(listed[0]["file_count"], 12);
        assert_eq!(listed[0]["branches"], json!(["B1", "B2", "B3", "B4"]));
        assert!(PLAN.contains("Choose the smallest number of sections"));
        assert!(PLAN.contains("different workflows their own sections"));
        Ok(())
    }

    #[test]
    fn reviews_and_corrections_see_a_bounded_plan() -> Result<()> {
        let view = BranchView {
            branches: (0..64).map(|i| test_branch(i, 2, 2)).collect(),
            groups: vec![],
        };
        let section = |i: usize| {
            json!({"title":format!("섹션 {i} {}", "제목".repeat(12)),
                "query":"backend/src/runner.rs write_section section_output::write",
                "key_points":(0..5).map(|_| "가".repeat(80)).collect::<Vec<_>>(),
                "evidence_ids":[],
                "diagrams":["흐름"],"branches":[format!("understanding:node:{}", i % 64)]})
        };
        for count in [64usize, 128, 256] {
            let mut plan: Outline = serde_json::from_value(json!({"reader_goal":"가".repeat(200),
                "storyline":"가".repeat(600),"sections":(0..count).map(section).collect::<Vec<_>>()}))?;
            validate_outline(&mut plan, &[], None, None)?;
            let whole = serde_json::to_vec(&plan)?.len();
            let shown = outline_view(
                &plan,
                &view.labels_by_key(),
                &|_, _| true,
                OUTLINE_VIEW_BYTES,
            );
            let size = serde_json::to_vec(&shown)?.len();
            assert!(size <= OUTLINE_VIEW_BYTES, "{count} sections: {size} bytes");
            assert!(size * 4 < whole, "{count} sections: {size} of {whole}");
            assert_eq!(shown["sections"].as_array().map(Vec::len), Some(count));
            let text = shown.to_string();
            assert!(!text.contains("understanding:node:") && !text.contains("\"query\""));
            // A review names sections by the short id it was shown.
            let mut ids = vec![
                shown["sections"][count - 1]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ];
            assert!(resolve_section_ids(&mut ids, &plan));
            assert_eq!(ids[0], plan.sections[count - 1].id);
            assert!(!resolve_section_ids(&mut ["".to_string()], &plan));
            assert!(!resolve_section_ids(&mut ["zzzz".to_string()], &plan));
        }
        // A chapter's correction round sees only its sections and their issues.
        let mut plan: Outline = serde_json::from_value(json!({"reader_goal":"g","storyline":"s",
            "sections":(0..64).map(section).collect::<Vec<_>>()}))?;
        validate_outline(&mut plan, &[], None, None)?;
        let feedback = json!({"previous_plan":plan,"issues":[
            {"severity":"major","code":"overlap","message":"앞 장 문제","section_ids":[plan.sections[1].id],"query":""},
            {"severity":"major","code":"scope","message":"뒤 장 문제","section_ids":[plan.sections[40].id],"query":""},
            {"severity":"minor","code":"order","message":"전체 문제","section_ids":[],"query":""}]});
        let scope: HashSet<String> = (0..16).map(|i| format!("understanding:node:{i}")).collect();
        let scoped = feedback_view(&feedback, &view, Some(&scope), OUTLINE_VIEW_BYTES / 2);
        assert_eq!(
            scoped["previous_plan"]["sections"].as_array().map(Vec::len),
            Some(16)
        );
        let messages: Vec<&str> = scoped["issues"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["message"].as_str())
            .collect();
        assert_eq!(messages, ["앞 장 문제", "전체 문제"]);
        let whole = feedback_view(&feedback, &view, None, OUTLINE_VIEW_BYTES);
        assert_eq!(whole["issues"].as_array().map(Vec::len), Some(3));
        assert!(
            feedback_view(&json!({}), &view, None, OUTLINE_VIEW_BYTES)
                .as_object()
                .is_some_and(|o| o.is_empty())
        );
        Ok(())
    }

    #[test]
    fn a_chapter_request_carries_only_its_own_files() {
        let inside = evidence("/project/api/handler.rs", "fn handle() {}");
        let outside = evidence("/project/ui/view.ts", "function view() {}");
        let finding = |id: &str, topic: &str| Finding {
            topic: topic.into(),
            observation: "관찰".into(),
            kind: FindingKind::Runtime,
            evidence_ids: vec![id.into()],
        };
        let discovery = Discovery {
            brief: SourceBrief {
                findings: vec![finding(&inside.id, "api"), finding(&outside.id, "ui")],
                uncertainties: vec!["미확인".into()],
                followup_queries: vec![],
            },
            details: vec![finding(&outside.id, "ui detail")],
            evidence: vec![inside.clone(), outside],
            validation_unresolved: false,
        };
        let files: HashSet<String> = ["api/handler.rs".to_string()].into();
        let scoped = scope_discovery(&discovery, &files, &["/project".to_string()]);
        assert_eq!(scoped.evidence.len(), 1);
        assert_eq!(scoped.evidence[0].id, inside.id);
        assert_eq!(scoped.brief.findings.len(), 1);
        assert_eq!(scoped.brief.findings[0].topic, "api");
        assert!(scoped.details.is_empty());
    }

    #[test]
    fn branches_come_from_one_level_and_group_under_a_narrower_one() {
        use crate::understanding::{TreeIndex, TreeNode};
        let node = |children: Vec<String>| TreeNode {
            files: vec![],
            children,
            findings: vec![],
            details: vec![],
            spans: vec![],
        };
        // root -> 4 groups -> 5 branches each -> leaves
        let mut nodes = HashMap::new();
        let mut groups = vec![];
        for g in 0..4 {
            let mut members = vec![];
            for b in 0..5 {
                let leaves: Vec<String> = (0..2).map(|l| format!("l{g}{b}{l}")).collect();
                for leaf in &leaves {
                    nodes.insert(leaf.clone(), node(vec![]));
                }
                let key = format!("b{g}{b}");
                nodes.insert(key.clone(), node(leaves));
                members.push(key);
            }
            let key = format!("g{g}");
            nodes.insert(key.clone(), node(members));
            groups.push(key);
        }
        nodes.insert("root".into(), node(groups));
        let tree = TreeIndex {
            root: Some("root".into()),
            leaves: vec![],
            nodes,
        };
        let levels = tree.levels();
        assert_eq!(
            levels.iter().map(Vec::len).collect::<Vec<_>>(),
            [1, 4, 20, 40]
        );
        let chosen = branch_level(&levels, 20).unwrap_or(0);
        assert_eq!(levels[chosen].len(), 20);
        let branches: Vec<Branch> = levels[chosen]
            .iter()
            .enumerate()
            .map(|(i, key)| Branch {
                key: key.clone(),
                ..test_branch(i, 1, 0)
            })
            .collect();
        let groups = branch_groups(&tree, &levels, chosen, &branches);
        assert_eq!(groups.len(), 4);
        assert!(groups.iter().all(|g| g.members.len() == 5));
        assert_eq!(groups[1].members, vec![5, 6, 7, 8, 9]);
        assert_eq!(branch_level(&[vec!["root".into()]], 8), None);
    }

    #[test]
    fn an_uncitable_item_is_named_so_a_repair_knows_which_one_to_fix() -> Result<()> {
        let source = evidence("/project/main.py", "def process(): return 1");
        let mut brief: SourceBrief = serde_json::from_value(json!({"findings":[
            {"topic":"첫 관찰","observation":"근거가 있다","kind":"context","evidence_ids":[source.id]},
            {"topic":"근거 없는 관찰","observation":"근거가 없다","kind":"context","evidence_ids":[]}
        ],"uncertainties":[],"followup_queries":[]}))?;
        let error = validate_brief(&mut brief, std::slice::from_ref(&source), true)
            .err()
            .context("an empty citation list must be rejected")?
            .to_string();
        // A bare count leaves the next attempt guessing which of a dozen items
        // was empty, so it repeats the mistake until the retries run out.
        assert!(error.contains("근거 없는 관찰"), "{error}");
        assert!(error.contains("supplied 0"), "{error}");
        assert!(!error.contains("첫 관찰"), "{error}");
        Ok(())
    }

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
    fn a_section_that_cannot_be_shown_its_own_scope_is_reported_however_coherent() -> Result<()> {
        let section: SectionPlan = serde_json::from_value(json!({
            "id":"37d5c3b904b0aaaabbbbccccdddd","title":"벡터 검색·임베딩 동기화·지식 데이터 모델",
            "key_points":["a"],"query":"q","evidence_ids":[],"diagrams":[],
            "branches":["B1","B2","B3","B4","B5","B6"]}))?;
        // What fits is the checklist its writer is given; what does not is what
        // the reading found and the document will not be told.
        let issue = scope_issue(&section, 14, 61).context("a mostly deferred scope is an issue")?;
        assert_eq!(issue.severity, "major");
        assert_eq!(issue.code, "section_scope_not_shown");
        assert_eq!(issue.section_ids, vec![section.id.clone()]);
        assert!(issue.message.contains("61"), "{}", issue.message);
        assert!(issue.message.contains('6'), "{}", issue.message);
        // A section whose scope reaches its writer is left alone, crowded or not.
        assert!(scope_issue(&section, 40, 3).is_none());
        assert!(scope_issue(&section, 12, 12).is_none());
        Ok(())
    }

    #[test]
    fn a_reading_past_the_requested_count_is_kept_when_it_is_valid_otherwise() -> Result<()> {
        let implementation = evidence("/project/a.py", "def receive(x): return finish(x)");
        let sources = vec![implementation.clone()];
        let brief_of = |count: usize| -> Result<SourceBrief> {
            Ok(serde_json::from_value(json!({
                "findings":(0..count).map(|n| json!({"topic":format!("topic {n}"),
                    "observation":"receive passes input to finish","kind":"runtime",
                    "evidence_ids":[&implementation.id[..8]]})).collect::<Vec<_>>(),
                "uncertainties":[],"followup_queries":[]
            }))?)
        };
        // What the request asks for, and two past it: the second is a reading
        // that said more true things, not a defect, so it is not sent back.
        validate_brief(&mut brief_of(MAX_FINDINGS)?, &sources, true)?;
        validate_brief(&mut brief_of(MAX_FINDINGS + 2)?, &sources, true)?;
        validate_brief(&mut brief_of(ACCEPTED_FINDINGS)?, &sources, true)?;
        // Past the ceiling the reading has stopped grouping at all, and the
        // error names both numbers so the retry knows which one it missed.
        let error = validate_brief(&mut brief_of(ACCEPTED_FINDINGS + 1)?, &sources, true)
            .err()
            .context("a reading past the ceiling must not validate")?
            .to_string();
        assert!(error.contains(&ACCEPTED_FINDINGS.to_string()), "{error}");
        assert!(error.contains(&MAX_FINDINGS.to_string()), "{error}");
        // A reading with nothing in it is still nothing.
        assert!(validate_brief(&mut brief_of(0)?, &sources, true).is_err());

        // Unresolved links follow the same split, and the refusal says which
        // rule broke rather than "Invalid uncertainties".
        let gaps = |count: usize, size: usize| -> Result<SourceBrief> {
            let mut brief = brief_of(1)?;
            brief.uncertainties = (0..count).map(|n| format!("{n}{}", "가".repeat(size))).collect();
            Ok(brief)
        };
        validate_brief(&mut gaps(MAX_UNCERTAINTIES + 4, 1)?, &sources, true)?;
        validate_brief(&mut gaps(ACCEPTED_UNCERTAINTIES, 1)?, &sources, true)?;
        let error = validate_brief(&mut gaps(ACCEPTED_UNCERTAINTIES + 1, 1)?, &sources, true)
            .err()
            .context("more unresolved links than a request carries must not validate")?
            .to_string();
        assert!(error.contains(&(ACCEPTED_UNCERTAINTIES + 1).to_string()), "{error}");
        assert!(error.contains(&MAX_UNCERTAINTIES.to_string()), "{error}");
        // One too long is still refused, and the lengths say which.
        let long = validate_brief(&mut gaps(1, 600)?, &sources, true)
            .err()
            .context("an oversized uncertainty must not validate")?
            .to_string();
        assert!(long.contains("1500"), "{long}");
        assert!(long.contains("lengths"), "{long}");

        // Told its summary was over budget, a reading groups passages under one
        // observation and cites them together - what the instruction asks for.
        // The citation target must not reject it for obeying.
        let passages: Vec<Evidence> = (0..ACCEPTED_EVIDENCE_IDS + 1)
            .map(|n| evidence(&format!("/project/p{n}.py"), &format!("def f{n}(): return {n}")))
            .collect();
        let grouped = |count: usize| -> Result<SourceBrief> {
            Ok(serde_json::from_value(json!({
                "findings":[{"topic":"픽스처 묶음","observation":"같은 계약을 여러 원문이 함께 뒷받침한다",
                    "kind":"runtime",
                    "evidence_ids":passages[..count].iter().map(|e| e.id.clone()).collect::<Vec<_>>()}],
                "uncertainties":[],"followup_queries":[]
            }))?)
        };
        validate_brief(&mut grouped(MAX_EVIDENCE_IDS)?, &passages, true)?;
        validate_brief(&mut grouped(MAX_EVIDENCE_IDS + 4)?, &passages, true)?;
        validate_brief(&mut grouped(ACCEPTED_EVIDENCE_IDS)?, &passages, true)?;
        let error = validate_brief(&mut grouped(ACCEPTED_EVIDENCE_IDS + 1)?, &passages, true)
            .err()
            .context("a finding past the citation ceiling must not validate")?
            .to_string();
        assert!(error.contains(&ACCEPTED_EVIDENCE_IDS.to_string()), "{error}");
        assert!(error.contains(&MAX_EVIDENCE_IDS.to_string()), "{error}");
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
        for section in value["sections"].as_array_mut().context("sections")? {
            section["evidence_ids"] = json!([source.id]);
        }
        value["sections"][31]["prerequisite_titles"] = json!(["단계 1", "단계 31", "단계 1"]);
        let mut repair = llm::JsonRepair::default();
        let mut plan = decode_generated_outline(&value.to_string(), &mut repair, &HashMap::new())?;
        assert!(plan.sections[0].depends_on.is_empty());
        assert_eq!(plan.sections[1].depends_on, vec![0]);
        assert_eq!(plan.sections[30].depends_on, vec![29]);
        assert_eq!(plan.sections[31].depends_on, vec![0, 30]);
        validate_outline(&mut plan, &[source], Some(0), None)?;
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
            let result = decode_generated_outline(
                &value.to_string(),
                &mut llm::JsonRepair::default(),
                &HashMap::new(),
            );
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
        let error = decode_generated_outline(
            &duplicate.to_string(),
            &mut llm::JsonRepair::default(),
            &HashMap::new(),
        )
        .err()
        .context("Ambiguous titles unexpectedly accepted")?;
        assert!(error.to_string().contains("matches 2 sections"));
        Ok(())
    }

    #[test]
    fn legacy_generated_dependencies_deduplicate_but_do_not_shift_numbering() -> Result<()> {
        let source = evidence("/project/main.py", "def process(): return 1");
        let mut value = generated_plan(3);
        for section in value["sections"].as_array_mut().context("sections")? {
            section
                .as_object_mut()
                .context("section object")?
                .remove("prerequisite_titles");
            section["evidence_ids"] = json!([source.id]);
        }
        value["sections"][1]["depends_on"] = json!([0, 0]);
        let mut plan = decode_generated_outline(
            &value.to_string(),
            &mut llm::JsonRepair::default(),
            &HashMap::new(),
        )?;
        assert_eq!(plan.sections[1].depends_on, vec![0]);
        validate_outline(&mut plan, std::slice::from_ref(&source), Some(0), None)?;
        for (reference, reason) in [
            (1, "a self-reference"),
            (2, "a forward reference"),
            (3, "outside the section array"),
        ] {
            plan.sections[1].depends_on = vec![reference];
            let error = validate_outline(&mut plan, std::slice::from_ref(&source), Some(0), None)
                .err()
                .context("an invalid dependency must be rejected")?
                .to_string();
            assert!(error.contains("sections[1]"), "{error}");
            assert!(error.contains(reason), "{error}");
        }
        Ok(())
    }

    #[test]
    fn dependency_repair_includes_middle_sections_of_long_responses() -> Result<()> {
        let mut value = generated_plan(32);
        for section in value["sections"].as_array_mut().context("sections")? {
            section["key_points"] = json!(["detail".repeat(1000)]);
        }
        let context = dependency_repair_context(&value.to_string());
        assert_eq!(context.as_array().map(Vec::len), Some(32));
        assert_eq!(context[16]["title"], "단계 17");
        assert_eq!(context[16]["prerequisite_titles"], json!(["단계 16"]));
        assert!(context[16].get("key_points").is_none());
        Ok(())
    }

    #[test]
    fn plans_reject_forward_prerequisites_duplicate_titles_and_missing_evidence() -> Result<()> {
        let source = evidence("/project/a.py", "def receive(x): return finish(x)");
        let mut plan: Outline = serde_json::from_value(json!({
            "reader_goal":"Send input and understand the result", "storyline":"Prepare input, then interpret its result", "terminology":[],
            "sections":[
                {"title":"Prepare input","query":"receive","reader_question":"Which input is valid?","handoff":"The validated input can be submitted","diagrams":[],"depends_on":[],"evidence_ids":[&source.id[..8]]},
                {"title":"Interpret the result","query":"finish","reader_question":"What does the result mean?","handoff":"","diagrams":[],"depends_on":[0],"evidence_ids":[&source.id[..8]]}
            ]
        }))?;
        validate_outline(&mut plan, std::slice::from_ref(&source), Some(0), None)?;
        assert_eq!(plan.sections[0].evidence_ids, vec![source.id.clone()]);
        plan.sections[0].depends_on = vec![1];
        assert!(validate_outline(&mut plan, std::slice::from_ref(&source), None, None).is_err());
        plan.sections[0].depends_on.clear();
        plan.sections[0].handoff.clear();
        validate_outline(&mut plan, std::slice::from_ref(&source), None, None)?;
        plan.sections[0].handoff = "Valid input".into();
        plan.sections[1].title = "  Prepare   INPUT  ".into();
        assert!(validate_outline(&mut plan, std::slice::from_ref(&source), None, None).is_err());
        plan.sections[1].title = "Interpret result".into();
        plan.sections[1].reader_question = "What does the result mean?".into();
        plan.sections[1].evidence_ids.clear();
        assert!(validate_outline(&mut plan, &[source], None, None).is_err());
        Ok(())
    }

    #[test]
    fn an_outline_is_bounded_by_the_branches_it_was_planned_from() -> Result<()> {
        let source = evidence("/project/a.py", "def process(): return 1");
        // Read back without its branches, an outline keeps the widest bound.
        for count in [0, 1, 8, 9, 16, 32, SECTIONS_MAX, SECTIONS_MAX + 1] {
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
            let result = validate_outline(&mut plan, std::slice::from_ref(&source), Some(0), None);
            assert_eq!(
                result.is_ok(),
                (1..=SECTIONS_MAX).contains(&count),
                "section count {count}: {result:?}"
            );
            // Planned from four branches, the same outline is held to twelve:
            // sections follow what the source was read in, not a flat number.
            let narrow =
                validate_outline(&mut plan, std::slice::from_ref(&source), Some(0), Some(4));
            assert_eq!(
                narrow.is_ok(),
                (1..=SECTIONS_WITHOUT_BRANCHES).contains(&count),
                "narrow section count {count}: {narrow:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn simple_outlines_require_no_generated_questions_or_ownership() -> Result<()> {
        let e = evidence("/project/main.rs", "fn main() {}");
        for count in [1, 2, 32] {
            let response = json!({"reader_goal":"Code summary","storyline":"Group related behavior",
                "requirements":"obsolete metadata", "requirement_owners":42,
                "sections":(0..count).map(|i|json!({"title":format!("Topic {i}"),"query":"main.rs",
                    "key_points":["Describe relevant behavior"],"evidence_ids":[e.id],"diagrams":[],
                    "owns_requirement_ids":"obsolete metadata"})).collect::<Vec<_>>()});
            let mut plan = decode_generated_outline(
                &response.to_string(),
                &mut llm::JsonRepair::default(),
                &HashMap::new(),
            )?;
            validate_outline(&mut plan, std::slice::from_ref(&e), Some(0), None)?;
            assert!(plan.requirements.is_empty());
            assert!(plan.sections.iter().all(|s| s.reader_question.is_empty()
                && s.handoff.is_empty()
                && s.owns_requirement_ids.is_empty()
                && s.depends_on.is_empty()));
        }
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
