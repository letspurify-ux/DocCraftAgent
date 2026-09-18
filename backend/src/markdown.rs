//! Markdown the pipeline produces and has to put back together.
//!
//! Model output arrives as prose, and these are the pure transforms applied to
//! it before it is checkpointed: unwrapping a stray outer fence, normalizing
//! heading depth, resolving `[E:id]` citations against the evidence actually
//! supplied, and splicing a repair's returned subsections back into a draft.
//!
//! Everything here treats fenced and indented code as literal text. A heading
//! inside a code block is an example being shown, not structure to rewrite, and
//! a citation inside backticks is documentation of the syntax rather than a
//! claim needing evidence.

use crate::model::Section;
use anyhow::{Result, bail, ensure};
use std::collections::HashSet;

/// What counts as a citation, for resolving one and for checking one.
///
/// A citation holds no closing bracket, so everything up to one is its id list.
/// The two passes used to disagree here - resolution allowed spaces, validation
/// did not - and a citation the model annotated, as
/// `[E:410c7872 — preserved observation]`, was therefore resolved by neither and
/// seen by neither: no issue was raised, no repair was asked for, and the raw
/// marker reached the page. One pattern, so they cannot drift apart again.
pub(crate) const CITATION_PATTERN: &str = r"\[E:([^\]]{1,400})\]";

pub(crate) const SUBSECTION_MARKER: &str = "<!-- DOCCRAFT_SUBSECTION ";

pub(crate) fn fence_spec(line: &str) -> Option<(u8, usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len().saturating_sub(trimmed.len()) > 3 {
        return None;
    }
    let marker = trimmed.as_bytes().first().copied()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let width = trimmed.bytes().take_while(|byte| *byte == marker).count();
    (width >= 3).then(|| (marker, width, &trimmed[width..]))
}

/// Models occasionally wrap the requested Markdown in a Markdown code block.
/// Removing that transport wrapper before citation and heading processing turns
/// its inner Mermaid/code fences back into real document structure.
pub(crate) fn unwrap_outer_markdown_fence(markdown: &str) -> String {
    let trimmed = markdown.trim();
    let lines: Vec<&str> = trimmed.lines().collect();
    let Some(first) = lines.first() else {
        return String::new();
    };
    let Some(last) = lines.last() else {
        return String::new();
    };
    let Some((marker, width, info)) = fence_spec(first) else {
        return trimmed.to_string();
    };
    let info = info.trim();
    if !info.eq_ignore_ascii_case("markdown") && !info.eq_ignore_ascii_case("md") {
        return trimmed.to_string();
    }
    let Some((last_marker, last_width, suffix)) = fence_spec(last) else {
        return trimmed.to_string();
    };
    if marker != last_marker || last_width < width || !suffix.trim().is_empty() || lines.len() < 2 {
        return trimmed.to_string();
    }
    lines[1..lines.len() - 1].join("\n").trim().to_string()
}

pub(crate) fn strip_heading_number(text: &str) -> &str {
    let original = text.trim();
    let (mut rest, had_prefix) = original
        .strip_prefix('제')
        .map(|value| (value.trim_start(), true))
        .unwrap_or((original, false));
    let mut end = 0;
    let mut saw_digit = false;
    for (index, ch) in rest.char_indices() {
        if ch.is_ascii_digit() || (saw_digit && ch == '.') {
            saw_digit |= ch.is_ascii_digit();
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if !saw_digit {
        return original;
    }
    let after_number = rest[end..].trim_start();
    let had_space = rest[end..].len() != after_number.len();
    rest = after_number;
    if let Some(value) = rest.strip_prefix('장') {
        rest = value.trim_start();
    } else if let Some(first) = rest.chars().next()
        && matches!(first, ':' | '：' | ')' | '）' | '-' | '–' | '—')
    {
        rest = rest[first.len_utf8()..].trim_start();
    } else if !had_prefix && !had_space {
        return original;
    }
    rest.trim_start_matches([':', '：', '.', ')', '）', '-', '–', '—', ' '])
}

pub(crate) fn canonical_heading(text: &str) -> String {
    strip_heading_number(text)
        .chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    ':' | '：' | '.' | ',' | '，' | '(' | ')' | '（' | '）' | '-' | '–' | '—'
                )
        })
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) fn prepare_section_markdown(
    markdown: &str,
    evidence: &[crate::model::Evidence],
    title: &str,
) -> Result<String> {
    let markdown = unwrap_outer_markdown_fence(markdown);
    let markdown = normalize_citations(&markdown, evidence)?;
    Ok(normalize_section_headings(&markdown, title))
}

pub(crate) fn mermaid_blocks(markdown: &str) -> Vec<std::ops::Range<usize>> {
    let mut result = Vec::new();
    let mut fence: Option<(u8, usize, usize, bool)> = None;
    let mut offset = 0;
    for line in markdown.split_inclusive('\n') {
        if let Some((marker, width, start, mermaid)) = fence {
            if fence_spec(line).is_some_and(|(close, close_width, suffix)| {
                close == marker && close_width >= width && suffix.trim().is_empty()
            }) {
                if mermaid {
                    result.push(start..offset + line.len());
                }
                fence = None;
            }
        } else if let Some((marker, width, info)) = fence_spec(line) {
            fence = Some((
                marker,
                width,
                offset,
                info.trim().eq_ignore_ascii_case("mermaid"),
            ));
        }
        offset += line.len();
    }
    result
}

#[derive(Clone)]
pub(crate) struct ProseBlock {
    pub section: usize,
    pub block: usize,
    pub text: String,
    pub normalized: String,
    pub shingles: HashSet<String>,
}

pub(crate) fn prose_blocks(sections: &[Section]) -> Result<Vec<ProseBlock>> {
    let citation = regex::Regex::new(r"\[E:[^\]\s]+\]")?;
    let mut result = Vec::new();
    for (section_index, section) in sections.iter().enumerate() {
        let mut ranges = crate::editorial::code_ranges(&section.markdown);
        ranges.sort_by_key(|range| range.start);
        let mut prose = String::new();
        let mut cursor = 0;
        for range in ranges {
            if range.start >= cursor {
                prose.push_str(&section.markdown[cursor..range.start]);
                prose.push_str("\n\n");
                cursor = range.end;
            }
        }
        prose.push_str(&section.markdown[cursor..]);
        for (block_index, paragraph) in prose.split("\n\n").enumerate() {
            let text = paragraph
                .lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join(" ");
            let text = citation.replace_all(&text, " ");
            let mut normalized = String::new();
            let mut previous_space = true;
            for ch in text.chars().flat_map(char::to_lowercase) {
                if ch.is_alphanumeric() {
                    normalized.push(ch);
                    previous_space = false;
                } else if !previous_space {
                    normalized.push(' ');
                    previous_space = true;
                }
            }
            let normalized = normalized.trim().to_string();
            if normalized.chars().count() < 120 {
                continue;
            }
            let tokens: Vec<&str> = normalized.split_whitespace().collect();
            let shingles = tokens
                .windows(2)
                .map(|pair| format!("{}\u{0}{}", pair[0], pair[1]))
                .collect();
            result.push(ProseBlock {
                section: section_index,
                block: block_index,
                text: text.trim().to_string(),
                normalized,
                shingles,
            });
        }
    }
    Ok(result)
}

/// A draft split at its top-level subsection headings, outside code. The text
/// before the first heading is a part of its own.
pub(crate) fn subsections(markdown: &str) -> Vec<String> {
    let literals = crate::editorial::code_ranges(markdown);
    let heading = |line: &str| {
        let trimmed = line.trim_start_matches(' ');
        let level = trimmed.bytes().take_while(|b| *b == b'#').count();
        ((1..=6).contains(&level) && trimmed[level..].starts_with(' ')).then_some(level)
    };
    let mut starts = vec![];
    let mut offset = 0;
    for line in markdown.split_inclusive('\n') {
        if !literals.iter().any(|r| r.contains(&offset))
            && let Some(level) = heading(line)
        {
            starts.push((offset, level));
        }
        offset += line.len();
    }
    let Some(top) = starts.iter().map(|(_, level)| *level).min() else {
        return vec![markdown.to_string()];
    };
    let mut cuts: Vec<usize> = starts
        .into_iter()
        .filter(|(_, level)| *level == top)
        .map(|(at, _)| at)
        .collect();
    if cuts.first() != Some(&0) {
        cuts.insert(0, 0);
    }
    cuts.push(markdown.len());
    cuts.windows(2)
        .map(|w| markdown[w[0]..w[1]].trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Apply a repair that returned only the subsections it changed.
///
/// `targets`, when given, are the subsections the repair was shown, with
/// `parts.len()` standing for a new subsection; only those may come back, and
/// a markerless answer to a single target is that target. Without `targets`,
/// `Ok(None)` means a response with no markers about as long as the draft,
/// which is a whole rewrite and is accepted as one. A markerless answer much
/// shorter than the draft is a partial repair that lost its markers: taking it
/// as the whole section threw every untouched subsection and its citations
/// away. That, a marker naming a subsection the repair may not change and a
/// marker naming one twice are errors the next attempt is told about rather
/// than guesses.
pub(crate) fn splice_subsections(
    parts: &[String],
    output: &str,
    targets: Option<&[usize]>,
) -> Result<Option<String>> {
    // A model sometimes wraps its whole answer in a Markdown fence, which would
    // hide every marker inside a code block.
    let unwrapped = unwrap_outer_markdown_fence(output);
    let output = unwrapped.as_str();
    let literals = crate::editorial::code_ranges(output);
    let marker = regex::Regex::new(r"^<!--\s*DOCCRAFT_SUBSECTION[\s:#]*(\d+)\s*-->$")?;
    let mut markers = vec![];
    let mut offset = 0;
    for line in output.split_inclusive('\n') {
        if !literals.iter().any(|r| r.contains(&offset))
            && let Some(captures) = marker.captures(line.trim())
        {
            let n: usize = captures[1].parse()?;
            markers.push((offset, offset + line.len(), n));
        }
        offset += line.len();
    }
    let mut replaced: Vec<Option<String>> = vec![None; parts.len() + 1];
    if markers.is_empty() {
        match targets {
            Some([only]) => replaced[*only] = Some(output.trim().to_string()),
            Some(_) => bail!(
                "The repair returned subsections without {SUBSECTION_MARKER}n --> markers; return each shown subsection after its marker"
            ),
            None => {
                let draft: usize = parts.iter().map(String::len).sum();
                ensure!(
                    output.trim().len() * 2 >= draft,
                    "The repair returned {} bytes without {SUBSECTION_MARKER}n --> markers for a {draft}-byte section. Either return the changed subsections each after its marker, or return the whole corrected section",
                    output.trim().len()
                );
                return Ok(None);
            }
        }
    } else {
        ensure!(
            output[..markers[0].0].trim().is_empty(),
            "Text appears before the first {SUBSECTION_MARKER}n --> marker; put every change inside the subsection it replaces"
        );
        for (index, (_, body_start, n)) in markers.iter().enumerate() {
            match targets {
                Some(targets) => ensure!(
                    targets.contains(n),
                    "Subsection marker {n} names a subsection this repair was not shown; return only {targets:?}"
                ),
                None => ensure!(
                    *n < parts.len(),
                    "Subsection marker {n} names no subsection; the draft has subsections 0-{}",
                    parts.len() - 1
                ),
            }
            ensure!(replaced[*n].is_none(), "Subsection {n} is replaced twice");
            let body_end = markers.get(index + 1).map_or(output.len(), |next| next.0);
            replaced[*n] = Some(output[*body_start..body_end].trim().to_string());
        }
    }
    let appended = replaced.pop().flatten();
    Ok(Some(
        parts
            .iter()
            .zip(replaced)
            .map(|(original, replacement)| replacement.unwrap_or_else(|| original.clone()))
            .chain(appended)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    ))
}

pub(crate) fn normalize_section_headings(markdown: &str, title: &str) -> String {
    let literals = crate::editorial::code_ranges(markdown);
    let title = canonical_heading(title);
    let mut offset = 0;
    let mut minimum = None;
    for raw_line in markdown.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let literal = literals.iter().any(|range| range.contains(&offset));
        offset += raw_line.len();
        let heading = line.trim_start_matches(' ');
        let indent = line.len() - heading.len();
        let level = heading.bytes().take_while(|byte| *byte == b'#').count();
        if !literal
            && indent <= 3
            && (1..=6).contains(&level)
            && heading
                .get(level..)
                .is_some_and(|rest| rest.starts_with(' '))
            && canonical_heading(heading[level..].trim()) != title
        {
            minimum = Some(minimum.map_or(level, |current: usize| current.min(level)));
        }
    }
    let shift = minimum
        .filter(|level| *level > 3)
        .map_or(0, |level| level - 3);
    let mut offset = 0;
    markdown
        .split_inclusive('\n')
        .filter_map(|raw_line| {
            let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let literal = literals.iter().any(|range| range.contains(&offset));
            offset += raw_line.len();
            let heading = line.trim_start_matches(' ');
            if !literal && line.len() - heading.len() <= 3 && heading.starts_with('#') {
                let n = heading.bytes().take_while(|b| *b == b'#').count();
                if (1..=6).contains(&n) && heading.get(n..).is_some_and(|s| s.starts_with(' ')) {
                    let text = heading[n..].trim();
                    if canonical_heading(text) == title {
                        return None;
                    }
                    let normalized = if n < 3 { 3 } else { n - shift };
                    return Some(format!("{} {text}", "#".repeat(normalized)));
                }
            }
            Some(line.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

pub(crate) fn normalize_citations(
    markdown: &str,
    evidence: &[crate::model::Evidence],
) -> Result<String> {
    let literals = crate::editorial::code_ranges(markdown);
    let cite = regex::Regex::new(CITATION_PATTERN)?;
    Ok(cite
        .replace_all(markdown, |captures: &regex::Captures<'_>| {
            if captures
                .get(0)
                .is_some_and(|m| literals.iter().any(|r| r.contains(&m.start())))
            {
                return captures[0].to_string();
            }
            let resolve = |candidate: &str| -> Option<String> {
                if candidate.len() < 8 {
                    return None;
                }
                let matches: std::collections::HashSet<&str> = evidence
                    .iter()
                    .filter(|e| e.id.starts_with(candidate))
                    .map(|e| e.id.as_str())
                    .collect();
                (matches.len() == 1)
                    .then(|| matches.into_iter().next().map(str::to_string))
                    .flatten()
            };
            let id = &captures[1];
            // A model sometimes labels a citation with the symbol it points at,
            // as [E:b85584b4(`resolvePassword`)]. The id in front of the label
            // still names one supplied passage; keeping it and dropping the
            // label is more faithful than rejecting the claim it anchors.
            let labelled = |candidate: &str| {
                let hex: String = candidate
                    .chars()
                    .take_while(char::is_ascii_hexdigit)
                    .collect();
                (hex.len() < candidate.len())
                    .then(|| resolve(&hex))
                    .flatten()
            };
            // One citation may name several passages. Each becomes its own
            // reference; if any of them names nothing, the whole citation is
            // left for review to reject rather than half-resolved.
            let mut ids = vec![];
            for part in id.split(',').map(str::trim) {
                // A model that names several passages often repeats the marker
                // on every part, as [E:a, E:b]. That prefix is the syntax the
                // part is already inside, not a character of the id it names,
                // and leaving it attached failed the whole citation: one part
                // that resolves to nothing keeps the raw marker on the page.
                let part = part.strip_prefix("E:").unwrap_or(part).trim_start();
                let Some(full) = resolve(part).or_else(|| labelled(part)) else {
                    return captures[0].to_string();
                };
                if !ids.contains(&full) {
                    ids.push(full);
                }
            }
            ids.iter()
                .map(|full| format!("[E:{full}]"))
                .collect::<String>()
        })
        .into_owned())
}
