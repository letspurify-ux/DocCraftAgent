//! The vocabulary of source reading: what a reading of the code produces, how
//! much of it one request may carry, and how it is checked.
//!
//! `understanding` reads the whole source into these types, `purpose` narrows
//! that reading to one document's needs, and `planning` turns the result into an
//! outline. All three share this module so the stages can depend on it instead
//! of on each other.
//!
//! Limits come in pairs. `MAX_*` is what a prompt asks for; `ACCEPTED_*` is what
//! a response may return before it is rejected. They differ because the real
//! constraint is bytes, checked separately: a reading that says more true things
//! than the target, inside the budget, was rejected on the count alone and the
//! retry wrote the same thing again.

use crate::{model::Evidence, source};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;

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

pub(crate) fn bounded_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}

/// Resolve only unambiguous prefixes from THIS request, then persist full hashes.
///
/// `subject` names the finding or section being checked. Every other rule here
/// says which item broke it; a bare "supply 1-N evidence_ids" leaves a repair
/// attempt guessing which of a dozen items was empty, so it repeats the mistake.
pub(crate) fn resolve_ids(
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
        ensure!(
            matches.len() == 1,
            "{subject:?} cites {id:?}, which {} of the evidence supplied to this request. Use an id exactly as this request spells it, such as {:?}, or drop the citation when nothing supplied supports the item",
            if matches.is_empty() {
                "names none"
            } else {
                "is the start of more than one"
            },
            evidence
                .iter()
                .take(6)
                .map(|e| e.id.get(..12).unwrap_or(&e.id))
                .collect::<Vec<_>>()
        );
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
