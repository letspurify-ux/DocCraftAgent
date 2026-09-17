//! Read implementation before choosing the reader's journey. Discovery is bounded
//! and checkpointed independently of the final outline and section drafts.
use crate::{
    db, editorial, llm,
    model::{Evidence, Outline, OutlineReview, SectionPlan},
    runner::{RunContext, fatal, is_budget, outline_diagram_error},
    source,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;

const PLAN_TEMPLATE: &str = "Return JSON {sections:[{title:string,key_points:[string],query:string,evidence_ids:[string],diagrams:[string]}],reader_goal:string,storyline:string}. Organize the code into clear sections and summarize its important behavior according to purpose. Choose the smallest number of sections that serves the purpose. The three views of the source are one thing at three depths, not three lists to merge: source_brief is the through-line across the whole project, source_branches are the parts that through-line runs through - each with `where` it lives, the topics it covers and how many files it holds - and supporting_findings are earlier observations that may belong to any of them. A topic appearing in more than one view is the same topic seen from further away, so say it once, in the section whose branch owns it. The document's spine is source_brief's through-line, ordered the way a reader should meet it; the branches supply the parts that sit under it, and a section belongs to the through-line finding it serves. source_branches arrive in the order the source was read, which follows how the code is laid out: useful for ordering sections within one theme, but a file layout is not a reader's journey and must not become the document's structure. source_branches is what the source contains, not what this document owes a section. Judge each branch against the purpose: a branch the purpose does not ask about gets no section, however many files it holds, and naming it in a section's out_of_scope is better than covering it. What the branches do change is the ceiling: where the purpose does reach many branches, give a branch with more relevant topics more sections than one with few, and do not compress a large relevant branch into one section because a smaller number looks tidier. A large source with a narrow purpose is a short document, and that is the correct answer rather than a failure to fill the room. Group related responsibilities and workflows, merging thin or overlapping topics. Do not force one section per file or impose a fixed template. Use an order that makes the actual code easy to follow: establish needed context before explaining processing, outputs and important alternative/error paths. Respect the user's explicit audience, scope, section count and diagram instructions. reader_goal briefly states what the document explains; storyline briefly explains the grouping and order. Each section needs a unique title, 1-12 concrete key_points, a query naming observed files/symbols for deeper reading, 1-{MAX_SECTION_ANCHORS} supplied evidence_ids, and diagrams as an array of diagram objectives (use [] if none). Share source evidence across sections when useful, but avoid repeating the same explanation. source_brief and supporting_findings contain previously checked observations, with uncertainties; use original evidence to resolve contradictions or add connections. Previously_read source anchors may support those existing observations when the original is omitted from this request. Missing excerpts and old uncertainties do not prove absent implementation. Do not invent runtime order, join independent workflows, or turn conditional paths into an unconditional sequence. Outline descriptions guide later writing and are not proof of execution. Keep titles under 300 UTF-8 bytes, each key point under 1500 bytes, query under 2000 bytes, reader_goal under 2000 bytes and storyline under 4000 bytes. Allocate at most 4 diagrams per section and respect max_diagrams across the whole document; do not repeat the overall diagram in each section. Use the requested language. Do not generate reader questions, requirement IDs, ownership tables or mandatory handoffs. Detailed transitions belong in the section prose.";
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
}

fn bounded_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}

/// Resolve only unambiguous prefixes from THIS request, then persist full hashes.
///
/// `subject` names the finding or section being checked. Every other rule here
/// says which item broke it; a bare "supply 1-N evidence_ids" leaves a repair
/// attempt guessing which of a dozen items was empty, so it repeats the mistake.
fn resolve_ids(ids: &mut [String], evidence: &[Evidence], max: usize, subject: &str) -> Result<()> {
    ensure!(
        !ids.is_empty() && ids.len() <= max,
        "{subject:?} supplied {} evidence_ids; cite 1-{max} of the evidence IDs in this request, or drop the item when nothing supplied supports it. Names and line numbers from source_graph are not evidence IDs",
        ids.len()
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
        !brief.findings.is_empty() && brief.findings.len() <= MAX_FINDINGS,
        "Supply 1-{MAX_FINDINGS} source findings"
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
        let subject = finding.topic.clone();
        resolve_ids(
            &mut finding.evidence_ids,
            evidence,
            MAX_EVIDENCE_IDS,
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
        let subject = section.title.clone();
        resolve_ids(
            &mut section.evidence_ids,
            evidence,
            MAX_SECTION_ANCHORS,
            &subject,
        )?;
    }
    if let Some(error) = outline_diagram_error(outline, maximum) {
        bail!("{error}");
    }
    Ok(())
}

/// Resolve generated title references locally. Numeric dependencies remain supported
/// for old providers/checkpoints; never guess whether their numbering is one-based.
fn decode_generated_outline(response: &str, repair: &mut llm::JsonRepair) -> Result<Outline> {
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

/// The most sections one document may hold, however large the source.
///
/// A file a reader opens has to end somewhere, and past this the answer is more
/// documents rather than a longer one.
const SECTIONS_MAX: usize = 64;
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
    (leaves / 4).clamp(8, SECTIONS_MAX)
}
/// Bytes the branch view may spend. Split evenly, so more branches each say
/// less rather than the last ones saying nothing.
const BRANCH_VIEW_BYTES: usize = 24_000;

/// An empty branch view is a tree that could not be read, not a project with no
/// parts. Reported as unknown, or the ceiling would tighten below where it sat
/// before the branches were consulted at all.
fn branch_count(branches: &[serde_json::Value]) -> Option<usize> {
    // The trailing marker that names what could not be shown is not itself a
    // branch, and a view that is only that marker describes none.
    let counted = branches
        .iter()
        .filter(|b| b.get("branches_not_shown").is_none())
        .count();
    (counted > 0).then_some(counted)
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
pub(crate) async fn branch_topics(ctx: &RunContext) -> Result<Vec<serde_json::Value>> {
    let Some(root) = db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:root").await? else {
        return Ok(vec![]);
    };
    let Ok(root) = serde_json::from_value::<crate::understanding::Node>(root) else {
        return Ok(vec![]);
    };
    let leaves: Vec<String> = db::load_checkpoint(&ctx.pool, &ctx.id, "understanding:leaves")
        .await?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    let target = branch_target(leaves.len());
    let mut level = vec![root];
    loop {
        if level.len() >= target {
            break;
        }
        let mut next = vec![];
        for node in &level {
            for key in &node.children {
                // A node this run cannot read is a narrower view, not a reason
                // to abandon the outline: `analyze` reads only the root when it
                // resumes, so a child of an older shape reaches here unchecked
                // and used to fail planning that had no need of it.
                if let Some(value) = db::load_checkpoint(&ctx.pool, &ctx.id, key).await?
                    && let Ok(child) = serde_json::from_value::<crate::understanding::Node>(value)
                {
                    next.push(child);
                }
            }
        }
        // A level that adds nothing is the leaves, or a tree that was never
        // built; either way the level above is the widest honest view.
        if next.is_empty() || next.len() <= level.len() {
            break;
        }
        // Levels step by the fan-in, so the first one past the target can
        // overshoot it several times over - 21 then 82 against a target of 64.
        // Whichever sits closer to the target is the better description.
        if !closer_level(level.len(), next.len(), target) {
            break;
        }
        level = next;
    }
    if level.len() < 2 {
        return Ok(vec![]);
    }
    // Paired, so a branch and its files cannot come apart: zipping two slices
    // would have dropped branches silently if they ever differed in length.
    let mut branches = Vec::with_capacity(level.len());
    for node in level {
        let (files, whole) = covered_files(ctx, &node).await?;
        branches.push(Branch { node, files, whole });
    }
    Ok(branch_view(&branches))
}

/// Every file a branch was read from, and whether the walk saw all of them.
///
/// A node's own `files` are the paths of the passages that reduction carried,
/// and a reduction packs its children's originals into whatever budget the
/// summaries leave over - so a branch covering sixteen files reports the six
/// that happened to fit. The count decides how many sections a branch earns and
/// the shared directory is its only identity, so both come from the leaves,
/// where `files` is what was actually read.
///
/// Breadth-first and only two fields deep: a depth-first walk that ran out of
/// budget could return interior nodes and almost no leaves, which is the very
/// undercount this replaces, and a leaf checkpoint carries its passages' text,
/// which need not be parsed to read a path.
async fn covered_files(
    ctx: &RunContext,
    branch: &crate::understanding::Node,
) -> Result<(Vec<String>, bool)> {
    fn strings(value: &serde_json::Value, key: &str) -> Vec<String> {
        value[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
    let mut seen = HashSet::new();
    let mut frontier: std::collections::VecDeque<Vec<String>> =
        std::collections::VecDeque::from([branch.children.clone()]);
    if branch.children.is_empty() {
        seen.extend(branch.files.iter().cloned());
    }
    let mut budget = 4_096usize;
    let mut whole = true;
    while let Some(keys) = frontier.pop_front() {
        for key in keys {
            if budget == 0 {
                whole = false;
                break;
            }
            budget -= 1;
            let Some(value) = db::load_checkpoint(&ctx.pool, &ctx.id, &key).await? else {
                whole = false;
                continue;
            };
            let children = strings(&value, "children");
            if children.is_empty() {
                seen.extend(strings(&value, "files"));
            } else {
                frontier.push_back(children);
            }
        }
    }
    // Shown against the source the user named, not from the filesystem root:
    // every branch otherwise repeats the same long prefix in `where` and in
    // every path it lists, and the reader already knows where their source is.
    let roots = &ctx.snapshot.task.sources;
    let mut files: Vec<String> = seen.iter().map(|path| relative(path, roots)).collect();
    files.sort();
    Ok((files, whole))
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
struct Branch {
    node: crate::understanding::Node,
    files: Vec<String>,
    /// False when the walk could not reach every leaf, so `file_count` is a
    /// floor rather than the number. Silence there would read as a small branch.
    whole: bool,
}

fn branch_view(level: &[Branch]) -> Vec<serde_json::Value> {
    // Measured, not divided and trusted. Dividing the budget by the branches and
    // flooring each share spent five times the allowance on a wide tree, and
    // trading topics for room keeps their product the same, so shrinking the
    // allowance is what actually converges.
    let mut allowance = BRANCH_VIEW_BYTES;
    for _ in 0..24 {
        let per = allowance / level.len().max(1);
        let topics = (per / OBSERVATION_FLOOR_BYTES).clamp(1, MAX_FINDINGS);
        let view = render_branches(
            level,
            topics,
            (per / topics).max(OBSERVATION_FLOOR_BYTES),
            (per / 600).min(12),
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
        let mut view = render_branches(&level[..shown], 1, OBSERVATION_FLOOR_BYTES, 0);
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

/// The shortest observation still worth reading; below this a topic is a title.
const OBSERVATION_FLOOR_BYTES: usize = 120;

fn render_branches(
    level: &[Branch],
    topics: usize,
    room: usize,
    files: usize,
) -> Vec<serde_json::Value> {
    level
        .iter()
        .map(|branch| {
            let (node, paths) = (&branch.node, &branch.files);
            let shown: Vec<serde_json::Value> = node
                .discovery
                .brief
                .findings
                .iter()
                .take(topics)
                .map(|f| {
                    json!({"topic":editorial::excerpt(&f.topic,200),
                        "observation":editorial::excerpt(&f.observation, room)})
                })
                .collect();
            // The counts travel even when the lists are trimmed, so a branch
            // that holds a lot is still recognisable as one that does.
            let mut record = json!({"where":shared_root(paths),
                "files":paths.iter().take(files).collect::<Vec<_>>(),
                "file_count":paths.len(),"topic_count":node.discovery.brief.findings.len(),
                "topics":shown});
            if !branch.whole {
                record["file_count_is_a_floor"] = json!(true);
            }
            record
        })
        .collect()
}

// The brief, supporting findings, anchors, project overview, feedback, the
// branch view and the planning instruction that ride along with the packed
// evidence. Derived so that widening the branch view cannot quietly overrun the
// request it shares with the evidence.
const PLAN_REQUEST_OVERHEAD_BYTES: usize = 32_000 + BRANCH_VIEW_BYTES;

pub(crate) fn evidence_budget(ctx: &RunContext) -> usize {
    crate::budget::packing_limit(
        &ctx.snapshot.settings.llm,
        ctx.extra_margin.load(std::sync::atomic::Ordering::Relaxed),
        PLAN_REQUEST_OVERHEAD_BYTES.saturating_add(ctx.snapshot.task.direction.len()),
    )
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
    let discovery = crate::purpose::analyze(ctx, system, &whole).await?;
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
    // Read once: the tree does not change while an outline is being planned.
    let branches = branch_topics(ctx).await?;
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
            ctx.event("outline_planning", json!({"stage":"planning","title":"구현 근거에 맞춰 설명 순서 구성","attempt":attempt+1,"evidence_chunks":evidence.len(),"branches":branches.len(),"branch_topics":branches.iter().map(|b| b["topics"].as_array().map_or(0,Vec::len)).sum::<usize>()})).await?;
            let mut input = json!({"purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,
            "source_brief":brief,"supporting_findings":crate::purpose::context(&discovery),"source_branches":branches,"source_anchors":discovery.evidence.iter().map(|e| json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),"project_overview":inventory,"feedback":feedback,"revision":revision,
            "evidence":evidence,"max_diagrams":ctx.snapshot.task.max_diagrams,
            "previous_error":previous_error,"attempt":attempt+1,"instruction":format!("{} At most {} sections for this document. Respect user feedback and preserve valid existing section IDs when supplied.", *PLAN, section_ceiling(branch_count(&branches)))});
            repair.apply(&mut input);
            if let Some(response) = &repair.response {
                input["previous_section_dependencies"] = dependency_repair_context(response);
            }
            let result = llm::call(ctx, system, input.clone()).await.and_then(|s| {
                let mut plan = decode_generated_outline(&s, &mut repair)?;
                plan.revision = revision;
                validate_outline(
                    &mut plan,
                    &discovery.evidence,
                    ctx.snapshot.task.max_diagrams,
                    branch_count(&branches),
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
        let mut input = json!({"phase":"outline_review","purpose":ctx.snapshot.task.direction,"language":ctx.snapshot.task.language,"outline":plan,"source_brief":discovery.brief,"supporting_findings":crate::purpose::context(discovery),"evidence":crate::purpose::pack(discovery,evidence_budget(ctx)),"source_anchors":discovery.evidence.iter().map(|e| json!({"id":e.id,"path":e.path,"previously_read":true})).collect::<Vec<_>>(),"attempt":attempt,"previous_error":error,"instruction":"Review the section grouping before writing. Return JSON {issues:[{severity:'major'|'minor',code:string,message:string,section_ids:[string],query:string}]}. Check only clear omission of the user's explicit scope, substantial duplicate coverage, incoherent reading order and explicit section/diagram constraints. Do not require generated questions, ownership assignments, handoffs or formal prerequisite metadata. Shared evidence is valid when sections explain different behavior. Detailed implementation verification belongs to section writing and source review. An unresolved source link alone is not a defective section grouping; preserve it for deeper reading. Report a major issue only when a concrete scope or structure defect would prevent a useful document. Name affected section IDs and a specific correction; query may name observed sources when necessary. Do not infer missing code from omitted excerpts or require every module to get a section. Return an empty issues array when there is no concrete structural defect. Use the requested language. At most 8 issues."});
        repair.apply(&mut input);
        let parsed = llm::call(ctx, system, input.clone()).await.and_then(|s| {
            let r: OutlineReview = repair.decode(&s)?;
            ensure!(
                r.issues.len() <= 8
                    && r.issues
                        .iter()
                        .all(|i| ["major", "minor"].contains(&i.severity.as_str())
                            && bounded_text(&i.message, 4000)
                            && bounded_text(&i.code, 100)
                            && i.query.len() <= 2000
                            && i.section_ids
                                .iter()
                                .all(|id| plan.sections.iter().any(|s| &s.id == id))),
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

    #[test]
    fn a_branch_view_describes_every_branch_inside_one_budget() -> Result<()> {
        let branch = |files: usize, topics: usize| crate::understanding::Node {
            key: format!("k{files}"),
            files: (0..files).map(|i| format!("src/m{i}.rs")).collect(),
            children: vec![],
            discovery: Discovery {
                brief: SourceBrief {
                    findings: (0..topics)
                        .map(|i| Finding {
                            topic: format!("주제 {i}"),
                            observation: "관".repeat(4000),
                            kind: FindingKind::Context,
                            evidence_ids: vec![],
                        })
                        .collect(),
                    uncertainties: vec![],
                    followup_queries: vec![],
                },
                evidence: vec![],
                details: vec![],
                validation_unresolved: false,
            },
            unresolved_nodes: 0,
            validation_issues: vec![],
            unverified_brief: None,
            unverified_output: None,
        };
        // A wide tree must not spend the budget on its first branches and leave
        // the rest out of the document: every branch is described.
        let pair = |nodes: Vec<crate::understanding::Node>, files: usize| -> Vec<Branch> {
            nodes
                .into_iter()
                .map(|node| Branch {
                    node,
                    files: (0..files).map(|i| format!("src/m{i}.rs")).collect(),
                    whole: true,
                })
                .collect()
        };
        let wide: Vec<_> = (1..=16).map(|i| branch(i, MAX_FINDINGS)).collect();
        let view = branch_view(&pair(wide, 3));
        assert_eq!(view.len(), 16);
        assert_eq!(view[0]["file_count"], 3);
        assert!(
            view.iter()
                .all(|b| b["topics"].as_array().is_some_and(|t| !t.is_empty()))
        );
        // The budget is measured, not divided and hoped for: dividing it and
        // flooring each share spent five times the allowance once the tree was
        // wide, and the planning request reserves exactly this much.
        // The level chosen sits near the target, which is capped, so these are
        // the widths that actually occur.
        for count in [2usize, 16, 64] {
            let level: Vec<_> = (0..count).map(|_| branch(3, MAX_FINDINGS)).collect();
            let wide = branch_view(&pair(level, 3));
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
        let huge: Vec<_> = (0..200).map(|_| branch(3, MAX_FINDINGS)).collect();
        let view = branch_view(&pair(huge, 3));
        assert!(serde_json::to_vec(&view)?.len() <= BRANCH_VIEW_BYTES);
        // The marker is not a branch, so the ceiling is not raised by it.
        assert_eq!(branch_count(&view), Some(view.len() - 1));
        assert_eq!(branch_count(&[json!({"branches_not_shown": 9})]), None);
        let omitted = view
            .last()
            .and_then(|v| v["branches_not_shown"].as_u64())
            .context("an omission must be named")?;
        assert_eq!(omitted as usize + view.len() - 1, 200);
        // The file count travels even when the list is capped, so a large branch
        // is recognisable as large.
        let big = branch_view(&pair(vec![branch(40, 1)], 40));
        assert_eq!(big[0]["file_count"], 40);
        assert_eq!(big[0]["files"].as_array().map(Vec::len), Some(12));
        // A walk that could not reach every leaf says so, or an undercount
        // reads as a small branch and the branch earns fewer sections - the
        // very failure taking the count from the leaves was meant to end.
        let partial = branch_view(&[Branch {
            node: branch(2, 1),
            files: vec!["src/a.rs".into()],
            whole: false,
        }]);
        assert_eq!(partial[0]["file_count_is_a_floor"], true);
        assert!(view[0]["file_count_is_a_floor"].is_null());
        // Fewer branches each get more room than many do.
        let narrow = branch_view(&pair(
            vec![branch(1, MAX_FINDINGS), branch(1, MAX_FINDINGS)],
            1,
        ));
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
        let mut plan = decode_generated_outline(&value.to_string(), &mut repair)?;
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
            let result =
                decode_generated_outline(&value.to_string(), &mut llm::JsonRepair::default());
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
            decode_generated_outline(&duplicate.to_string(), &mut llm::JsonRepair::default())
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
        let mut plan =
            decode_generated_outline(&value.to_string(), &mut llm::JsonRepair::default())?;
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
            let mut plan =
                decode_generated_outline(&response.to_string(), &mut llm::JsonRepair::default())?;
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
