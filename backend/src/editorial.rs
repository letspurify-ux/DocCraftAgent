//! Bounded reading context and publication layout; evidence stays in analysis checkpoints.
use crate::model::Section;
use serde_json::{Value, json};

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

pub fn digest(sections: &[Section], byte_budget: usize) -> Vec<Value> {
    let per = byte_budget / sections.len().max(1);
    sections.iter().enumerate().map(|(index, section)| {
        let mut text = section.markdown.clone();
        for e in &section.evidence { text = text.replace(&format!("[E:{}]", e.id), "[source]"); }
        let headings = text.lines().filter(|line| line.starts_with('#')).take(16).collect::<Vec<_>>().join("\n");
        json!({"section":index,"title":section.title,"headings":excerpt(&headings,per/3),"text":excerpt(&text,per*2/3),"excerpted":text.len()>per*2/3,"mermaid_count":section.markdown.lines().filter(|line| line.trim_start().starts_with("```mermaid")).count(),"heading_count":section.markdown.lines().filter(|line| line.trim_start().starts_with('#')).count()})
    }).collect()
}

pub fn render_sections(sections: &[Section]) -> (String, String) {
    let mut numbers = std::collections::HashMap::new();
    let mut references = String::from("## Source references\n\n");
    for section in sections {
        for e in &section.evidence {
            if section.markdown.contains(&format!("[E:{}]", e.id)) && !numbers.contains_key(&e.id) {
                let number = numbers.len() + 1;
                numbers.insert(e.id.clone(), number);
                references.push_str(&format!(
                    "[^s{number}]: `{}` L{}–L{} · Evidence `{}`\n\n",
                    e.path.replace('`', ""),
                    e.start,
                    e.end,
                    e.id
                ));
            }
        }
    }
    let mut body = String::new();
    for section in sections {
        let mut markdown = section.markdown.clone();
        for (id, number) in &numbers {
            markdown = markdown.replace(&format!("[E:{id}]"), &format!("[^s{number}]"));
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
}

/// Expose short, unique citation IDs to the model; persisted evidence keeps its hash.
pub fn compact_evidence_ids(input: &mut Value) {
    fn collect(value: &Value, ids: &mut std::collections::BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if map.contains_key("path")
                    && map.contains_key("content")
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
    let mut ids = std::collections::BTreeSet::new();
    collect(input, &mut ids);
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
    rewrite(input, &aliases);
}

#[cfg(test)]
mod citation_view_tests {
    use super::*;
    #[test]
    fn aliases_are_unique_and_source_content_is_unchanged() {
        let a = format!("12345678a{}", "0".repeat(55));
        let b = format!("12345678b{}", "0".repeat(55));
        let source = format!("// Source contains [E:{a}]");
        let mut input = json!({"evidence":[{"id":a,"path":"a.rs","content":source},{"id":b,"path":"b.rs","content":"code"}],"correction":{"previous":format!("Fact [E:{a}] [E:{b}] [E:unknown]")}});
        compact_evidence_ids(&mut input);
        assert_eq!(input["evidence"][0]["id"], "12345678a");
        assert_eq!(input["evidence"][1]["id"], "12345678b");
        assert_eq!(input["evidence"][0]["content"], source);
        assert_eq!(
            input["correction"]["previous"],
            "Fact [E:12345678a] [E:12345678b] [E:unknown]"
        );
    }
}
