//! Bounded reading context and publication layout; evidence stays in analysis checkpoints.
use crate::model::Section;
use serde_json::{Value, json};

/// A bullet or ordered list marker at any indentation.
fn list_marker(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    match bytes.first() {
        Some(b'-' | b'*' | b'+') => matches!(bytes.get(1), Some(b' ' | b'\t')),
        Some(b'0'..=b'9') => {
            let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
            digits <= 9
                && matches!(bytes.get(digits), Some(b'.' | b')'))
                && matches!(bytes.get(digits + 1), Some(b' ' | b'\t'))
        }
        _ => false,
    }
}

fn block_code_ranges(markdown: &str) -> (Vec<std::ops::Range<usize>>, bool) {
    let mut ranges = Vec::new();
    let mut fence: Option<(u8, usize, usize)> = None;
    let mut offset = 0;
    // An indented code block cannot interrupt a paragraph, and inside a list the
    // same indentation is item continuation. Without both rules a third-level
    // bullet reads as a code literal, and a citation on that line is never
    // validated, never resolved and reaches the published document raw.
    let (mut paragraph, mut in_list) = (false, false);
    for line in markdown.split_inclusive('\n') {
        let inside_fence = fence.is_some();
        let blank = line.trim().is_empty();
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let marker = trimmed.as_bytes().first().copied().unwrap_or_default();
        let width = trimmed.bytes().take_while(|b| *b == marker).count();
        let mut indented_code = false;
        if let Some((kind, size, start)) = fence {
            if indent <= 3 && marker == kind && width >= size && trimmed[width..].trim().is_empty()
            {
                ranges.push(start..offset + line.len());
                fence = None;
            }
        } else if indent <= 3
            && matches!(marker, b'`' | b'~')
            && width >= 3
            && (marker != b'`' || !trimmed[width..].contains('`'))
        {
            fence = Some((marker, width, offset));
        } else if (indent >= 4 || line.starts_with('\t')) && !paragraph && !in_list {
            ranges.push(offset..offset + line.len());
            indented_code = true;
        }
        // Blank lines, fences and indented code all leave no paragraph open for
        // the next line to continue; a list survives the blank lines between
        // its items and ends at the next unindented line that starts no item.
        paragraph = !blank && !indented_code && !inside_fence && fence.is_none();
        if !inside_fence && fence.is_none() && !indented_code {
            if list_marker(trimmed) {
                in_list = true;
            } else if !blank && indent == 0 {
                in_list = false;
            }
        }
        offset += line.len();
    }
    let unclosed = fence.is_some();
    if let Some((_, _, start)) = fence {
        ranges.push(start..markdown.len());
    }
    (ranges, unclosed)
}

/// Byte ranges of code literals, so citation examples are not treated as references.
pub fn code_ranges(markdown: &str) -> Vec<std::ops::Range<usize>> {
    let (mut ranges, _) = block_code_ranges(markdown);
    let blocks = ranges.clone();
    let bytes = markdown.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(range) = blocks.iter().find(|r| r.contains(&i)) {
            i = range.end;
            continue;
        }
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] == b'`' {
            i += 1;
        }
        let width = i - start;
        let mut end = i;
        while end < bytes.len() && !blocks.iter().any(|r| r.contains(&end)) {
            // A code span does not cross a blank line. A stray backtick with no
            // closer in its own paragraph is literal text, and pairing it with
            // one further down the document marks every citation in between as
            // a code example, which then reaches the published page raw.
            if bytes[end] == b'\n' {
                let mut peek = end + 1;
                while peek < bytes.len() && matches!(bytes[peek], b' ' | b'\t' | b'\r') {
                    peek += 1;
                }
                if peek >= bytes.len() || bytes[peek] == b'\n' {
                    break;
                }
            }
            if bytes[end] != b'`' {
                end += 1;
                continue;
            }
            let close = end;
            while end < bytes.len() && bytes[end] == b'`' {
                end += 1;
            }
            if end - close == width {
                ranges.push(start..end);
                i = end;
                break;
            }
        }
    }
    ranges
}

/// CommonMark-style fence balance check. Counting literal ``` markers is not
/// sufficient because fences may use tildes, more than three markers, or contain
/// another apparent opening fence as literal text.
pub fn has_unclosed_fence(markdown: &str) -> bool {
    block_code_ranges(markdown).1
}

pub fn replace_citation(markdown: &str, id: &str, replacement: &str) -> String {
    let ranges = code_ranges(markdown);
    let needle = format!("[E:{id}]");
    let mut out = String::new();
    let mut cursor = 0;
    for (start, matched) in markdown.match_indices(&needle) {
        if ranges.iter().any(|r| r.contains(&start)) {
            continue;
        }
        out.push_str(&markdown[cursor..start]);
        out.push_str(replacement);
        cursor = start + matched.len();
    }
    out.push_str(&markdown[cursor..]);
    out
}

pub fn excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    const GAP: &str = "\n[… excerpt …]\n";
    let usable = limit.saturating_sub(GAP.len());
    let mut head = usable / 2;
    let mut tail = text.len() - (usable - head);
    while !text.is_char_boundary(head) {
        head = head.saturating_sub(1);
    }
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    if limit < GAP.len() {
        return String::new();
    }
    format!("{}{}{}", &text[..head], GAP, &text[tail..])
}

fn markdown_headings(markdown: &str) -> Vec<&str> {
    let (literals, _) = block_code_ranges(markdown);
    let mut offset = 0;
    let mut headings = Vec::new();
    for raw_line in markdown.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        if !literals.iter().any(|range| range.contains(&offset)) && indent <= 3 {
            let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
            if (1..=6).contains(&level)
                && trimmed
                    .get(level..)
                    .is_some_and(|rest| rest.starts_with(' '))
            {
                headings.push(trimmed);
            }
        }
        offset += raw_line.len();
    }
    headings
}

pub fn digest(sections: &[Section], byte_budget: usize) -> Vec<Value> {
    let per = byte_budget / sections.len().max(1);
    sections.iter().enumerate().map(|(index, section)| {
        let mut text = section.markdown.clone();
        for e in &section.evidence { text = replace_citation(&text, &e.id, "[source]"); }
        let headings = markdown_headings(&text);
        let heading_count = headings.len();
        let headings = headings.into_iter().take(16).collect::<Vec<_>>().join("\n");
        json!({"section":index,"title":section.title,"headings":excerpt(&headings,per/3),"text":excerpt(&text,per*2/3),"excerpted":text.len()>per*2/3,"mermaid_count":section.markdown.lines().filter(|line| line.trim_start().starts_with("```mermaid")).count(),"heading_count":heading_count})
    }).collect()
}

/// The passages the body actually cites, in the order footnote numbers are
/// handed out. Extracted so that anything else pointing at a passage - a review
/// warning, say - lands on the same footnote as the body rather than printing a
/// raw evidence id, and cannot drift out of step with it.
pub fn cited(sections: &[Section]) -> Vec<&crate::model::Evidence> {
    let mut seen = std::collections::HashSet::new();
    let mut result = vec![];
    for section in sections {
        for e in &section.evidence {
            if replace_citation(&section.markdown, &e.id, "") != section.markdown
                && seen.insert(e.id.as_str())
            {
                result.push(e);
            }
        }
    }
    result
}

/// Point a review warning at the same footnote the body uses.
///
/// A reviewer writes its warnings in prose and names the passage it is talking
/// about, usually by a short prefix of the evidence id. That prose is appended
/// to the document verbatim, so it never passed through citation normalisation
/// and the raw `[E:...]` marker reached the page - a bare hash the reader can do
/// nothing with. A passage the body cites becomes its footnote; one it does not
/// becomes the file name, which is what the sentence is about anyway; a prefix
/// that names nothing is dropped, since it identified no passage to begin with.
pub fn warning_citations(warning: &str, sections: &[Section]) -> String {
    let Ok(cite) = regex::Regex::new(r"\[E:([^\]]{1,400})\]") else {
        return warning.to_string();
    };
    let numbered = cited(sections);
    let literals = code_ranges(warning);
    let unique = |pool: &mut dyn Iterator<Item = (usize, &crate::model::Evidence)>| {
        let first = pool.next();
        match (first, pool.next()) {
            (Some(hit), None) => Some((hit.0, hit.1.path.clone())),
            _ => None,
        }
    };
    cite.replace_all(warning, |captures: &regex::Captures<'_>| {
        if captures
            .get(0)
            .is_some_and(|m| literals.iter().any(|r| r.contains(&m.start())))
        {
            return captures[0].to_string();
        }
        // A reviewer names the passage by a short prefix and sometimes appends a
        // label, the same shapes the body's own citations arrive in.
        let prefix: String = captures[1]
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        if prefix.len() < 8 {
            return String::new();
        }
        if let Some((index, _)) = unique(
            &mut numbered
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, e)| e.id.starts_with(&prefix)),
        ) {
            return format!("[^s{}]", index + 1);
        }
        // Not cited by the body, so it has no footnote. The file is what the
        // sentence is about and is what a reader can act on.
        match unique(
            &mut sections
                .iter()
                .flat_map(|s| &s.evidence)
                .enumerate()
                .filter(|(_, e)| e.id.starts_with(&prefix)),
        ) {
            Some((_, path)) => format!("`{}`", path.rsplit('/').next().unwrap_or(&path)),
            None => String::new(),
        }
    })
    .into_owned()
}

pub fn render_sections(sections: &[Section]) -> (String, String) {
    let mut numbers = std::collections::HashMap::new();
    let mut references = String::from("## Source references\n\n");
    for (index, e) in cited(sections).into_iter().enumerate() {
        let number = index + 1;
        numbers.insert(e.id.clone(), number);
        references.push_str(&format!(
            "[^s{number}]: `{}` L{}–L{} · Evidence `{}`\n\n",
            e.path.replace('`', ""),
            e.start,
            e.end,
            e.id
        ));
    }
    let mut body = String::new();
    for section in sections {
        let mut markdown = section.markdown.clone();
        for (id, number) in &numbers {
            markdown = replace_citation(&markdown, id, &format!("[^s{number}]"));
        }
        body.push_str(&format!("## {}\n\n{}\n\n", section.title, markdown));
    }
    (body, references)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Evidence, Outline};
    #[test]
    fn excerpts_bound_unicode_and_keep_both_ends() {
        let text = format!("시작{}끝", "한글본문".repeat(10000));
        let summary = excerpt(&text, 1000);
        assert!(summary.len() <= 1000);
        assert!(summary.starts_with("시작") && summary.ends_with("끝"));
        assert!(excerpt(&text, 3).len() <= 3);
    }
    #[test]
    fn legacy_outlines_still_load() -> anyhow::Result<()> {
        let o: Outline =
            serde_json::from_value(json!({"sections":[{"title":"Old","query":"old.rs"}]}))?;
        assert!(o.storyline.is_empty());
        Ok(())
    }
    #[test]
    fn a_review_warning_lands_on_the_same_footnote_as_the_body() {
        let cited_ev = Evidence {
            id: "a".repeat(64),
            path: "/repo/backend/src/server.js".into(),
            start: 2,
            end: 9,
            content: "source".into(),
        };
        let only_supplied = Evidence {
            id: "b".repeat(64),
            path: "/repo/backend/src/oracle.js".into(),
            ..cited_ev.clone()
        };
        let section = Section {
            title: "요청 처리".into(),
            markdown: format!("응답을 기록한다 [E:{}].", cited_ev.id),
            evidence: vec![cited_ev.clone(), only_supplied.clone()],
        };
        let sections = [section];
        let (body, _) = render_sections(&sections);
        assert!(body.contains("[^s1]"));

        // Cited by the body: the warning points at the footnote the reader
        // already has, not at a hash they can do nothing with.
        let fixed = warning_citations(
            &format!(
                "[minor] 근거 [E:{}]에서 훅 존재까지만 확인된다.",
                &cited_ev.id[..8]
            ),
            &sections,
        );
        assert_eq!(fixed, "[minor] 근거 [^s1]에서 훅 존재까지만 확인된다.");

        // Supplied but never cited, so it has no footnote; the file is what the
        // sentence is about.
        let fixed = warning_citations(
            &format!(
                "[minor] 제공된 [E:{}] 발췌에는 정리 코드가 없다.",
                &only_supplied.id[..10]
            ),
            &sections,
        );
        assert_eq!(
            fixed,
            "[minor] 제공된 `oracle.js` 발췌에는 정리 코드가 없다."
        );

        // Names no supplied passage at all, so it identified nothing to keep.
        let fixed = warning_citations("[minor] 근거 [E:deadbeef99]는 확인되지 않는다.", &sections);
        assert_eq!(fixed, "[minor] 근거 는 확인되지 않는다.");
        assert!(!fixed.contains("[E:"));
    }

    #[test]
    fn a_warning_that_quotes_the_citation_syntax_keeps_it() {
        let e = Evidence {
            id: "a".repeat(64),
            path: "/repo/src/server.js".into(),
            start: 2,
            end: 9,
            content: "source".into(),
        };
        let sections = [Section {
            title: "인용".into(),
            markdown: format!("본문 [E:{}].", e.id),
            evidence: vec![e.clone()],
        }];
        let warning = format!(
            "[minor] 예시 `[E:{}]` 표기는 그대로 두어야 한다.",
            &e.id[..8]
        );
        assert_eq!(warning_citations(&warning, &sections), warning);
    }

    #[test]
    fn citations_are_short_but_traceable_and_unused_evidence_is_omitted() {
        let e = Evidence {
            id: "a".repeat(64),
            path: "source.rs".into(),
            start: 2,
            end: 9,
            content: "source".into(),
        };
        let unused = Evidence {
            id: "unused".into(),
            ..e.clone()
        };
        let s = Section {
            title: "Operate".into(),
            markdown: format!("First [E:{}]. Then [E:{}].", e.id, e.id),
            evidence: vec![e.clone(), unused],
        };
        let (body, refs) = render_sections(&[s]);
        assert_eq!(body.matches("[^s1]").count(), 2);
        assert!(!body.contains(&e.id));
        assert!(refs.contains(&e.id) && refs.contains("L2–L9"));
        assert!(!refs.contains("unused"));
    }
    #[test]
    fn fence_balance_follows_commonmark_closing_rules() {
        assert!(!has_unclosed_fence(
            "~~~~rust\nlet value = 1;\n~~~~\n\n```text\nplain\n```"
        ));
        assert!(has_unclosed_fence(
            "```markdown\n```mermaid\nflowchart LR\nA-->B\n```\n```"
        ));
        assert!(has_unclosed_fence("~~~rust\nlet value = 1;"));
    }
    #[test]
    fn a_code_span_does_not_cross_a_blank_line() {
        let e = Evidence {
            id: "d".repeat(64),
            path: "parse.js".into(),
            start: 1,
            end: 9,
            content: "function parseCandidate() {}".into(),
        };
        // A stray backtick with no closer in its own paragraph used to pair with
        // one several paragraphs later, marking every citation between them as a
        // code example. Nothing rewrote them, nothing validated them, and the
        // raw marker reached the published page.
        let section = Section {
            title: "파싱".into(),
            markdown: format!(
                "여는 ` 문자는 문자열 안에만 온다.\n\n파서는 중괄호 깊이를 센다 [E:{id}].\n\n`parseCandidate`가 끝을 정한다.\n",
                id = e.id
            ),
            evidence: vec![e.clone()],
        };
        let (body, refs) = render_sections(std::slice::from_ref(&section));
        assert!(body.contains("[^s1]"), "{body}");
        assert!(
            !body.contains(&e.id),
            "a raw citation reached the document: {body}"
        );
        assert!(refs.contains(&e.id));
        // A span that opens and closes normally is still a literal.
        let ranges = code_ranges("Call `parseCandidate` now.");
        assert!(ranges.iter().any(|r| r.contains(&6)), "{ranges:?}");
    }

    #[test]
    fn list_indentation_is_not_code_but_real_indented_blocks_still_are() {
        let e = Evidence {
            id: "b".repeat(64),
            path: "source.rs".into(),
            start: 1,
            end: 4,
            content: "source".into(),
        };
        // Four spaces is an ordinary third-level bullet, and a continuation line
        // under an ordered item is item text, not a code literal.
        let nested = format!(
            "- 상위\n  - 중간\n    - 하위는 값을 검증한다 [E:{id}]\n\n1. 첫째\n    이어지는 설명 [E:{id}]\n",
            id = e.id
        );
        let section = Section {
            title: "절차".into(),
            markdown: nested,
            evidence: vec![e.clone()],
        };
        let (body, refs) = render_sections(std::slice::from_ref(&section));
        assert_eq!(body.matches("[^s1]").count(), 2, "{body}");
        assert!(
            !body.contains(&e.id),
            "a raw citation reached the document: {body}"
        );
        assert!(refs.contains(&e.id));
        // A genuinely indented block after a blank line, and one right after a
        // closing fence, are still literals.
        let literal = format!(
            "설명\n\n    [E:{id}]\n\n~~~~\ncode\n~~~~\n    [E:{id}]\n",
            id = e.id
        );
        let ranges = code_ranges(&literal);
        assert_eq!(
            literal
                .match_indices(&format!("[E:{}]", e.id))
                .filter(|(at, _)| ranges.iter().any(|r| r.contains(at)))
                .count(),
            2,
            "{ranges:?}"
        );
    }

    #[test]
    fn digest_counts_only_rendered_headings() {
        let section = Section {
            title: "Examples".into(),
            markdown:
                "### 실제 제목\n\n```text\n# 명령 예시\n## 출력 예시\n```\n\n    # 들여쓴 코드\n"
                    .into(),
            evidence: vec![],
        };
        let digest = digest(&[section], 4000);
        assert_eq!(digest[0]["heading_count"], 1);
        assert_eq!(digest[0]["headings"], "### 실제 제목");
    }
}

/// Expose short, unique citation IDs to the model; persisted evidence keeps its hash.
pub fn compact_evidence_ids(input: &mut Value) {
    fn collect(value: &Value, ids: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if map.contains_key("path")
                    && (map.contains_key("content")
                        || map.get("previously_read").and_then(Value::as_bool) == Some(true))
                    && let Some(id) = map.get("id").and_then(Value::as_str)
                    && id.len() == 64
                    && id.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    ids.insert(id.to_string());
                }
                for value in map.values() {
                    collect(value, ids);
                }
            }
            Value::Array(values) => {
                for value in values {
                    collect(value, ids);
                }
            }
            _ => {}
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    collect(input, &mut ids);
    compact_against(input, &ids);
}

/// Rewrite ids to the aliases they would be sent as, for a value whose own
/// shape does not carry the passages that name them.
///
/// A brief holds bare id strings and no evidence objects, so `collect` finds
/// nothing in it. Measuring one against a byte budget still has to charge what
/// the request will actually spend, which is the alias and not the hash.
pub fn compact_against(value: &mut Value, ids: &std::collections::BTreeSet<String>) {
    fn rewrite(value: &mut Value, aliases: &[(String, String)]) {
        match value {
            Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    if key != "content" {
                        rewrite(value, aliases);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    rewrite(value, aliases);
                }
            }
            Value::String(text) => {
                for (id, alias) in aliases {
                    if text == id {
                        *text = alias.clone();
                    } else {
                        *text = text.replace(&format!("[E:{id}]"), &format!("[E:{alias}]"));
                    }
                }
            }
            _ => {}
        }
    }
    let aliases = ids
        .iter()
        .map(|id| {
            let mut length = 8;
            while length < id.len()
                && ids
                    .iter()
                    .any(|other| other != id && other.starts_with(&id[..length]))
            {
                length += 1;
            }
            (id.clone(), id[..length].to_string())
        })
        .collect::<Vec<_>>();
    rewrite(value, &aliases);
}

#[cfg(test)]
mod citation_view_tests {
    use super::*;
    #[test]
    fn aliases_are_unique_and_source_content_is_unchanged() {
        let a = format!("12345678a{}", "0".repeat(55));
        let b = format!("12345678b{}", "0".repeat(55));
        let source = format!("// Source contains [E:{a}]");
        let mut input = json!({"evidence":[{"id":a,"path":"a.rs","content":source},{"id":b,"path":"b.rs","content":"code"}],"source_anchors":[{"id":a,"path":"a.rs","previously_read":true}],"correction":{"previous":format!("Fact [E:{a}] [E:{b}] [E:unknown]")}});
        compact_evidence_ids(&mut input);
        assert_eq!(input["evidence"][0]["id"], "12345678a");
        assert_eq!(input["source_anchors"][0]["id"], "12345678a");
        assert_eq!(input["evidence"][1]["id"], "12345678b");
        assert_eq!(input["evidence"][0]["content"], source);
        assert_eq!(
            input["correction"]["previous"],
            "Fact [E:12345678a] [E:12345678b] [E:unknown]"
        );
    }
}
