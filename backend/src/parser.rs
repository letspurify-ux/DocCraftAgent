use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const VERSION: &str = "parser-v3-graph";
#[derive(Clone, Serialize, Deserialize)]
pub struct Parsed {
    pub language: String,
    pub has_errors: bool,
    pub chunks: Vec<Chunk>,
    pub relations: Vec<String>,
    #[serde(default)]
    pub graph: crate::code_graph::CodeGraph,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub start: u32,
    pub end: u32,
    pub symbols: Vec<String>,
    pub content: String,
}
pub fn language(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "py" => "python",
        "java" => "java",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        _ => "text",
    }
}
pub fn parse_file(path: &Path, lang: &str) -> Result<Parsed> {
    if std::fs::metadata(path)?.len() > 100 * 1024 * 1024 {
        bail!("File exceeds hard worker limit");
    }
    let content = std::fs::read_to_string(path)?;
    let grammar: Option<tree_sitter::Language> = match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "c" => Some(tree_sitter_c::LANGUAGE.into()),
        "cpp" => Some(tree_sitter_cpp::LANGUAGE.into()),
        _ => None,
    };
    let mut boundaries = vec![0usize];
    let mut names = Vec::<(usize, String)>::new();
    let mut relations = Vec::new();
    let mut has_errors = false;
    if let Some(grammar) = grammar {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&grammar)?;
        let tree = parser
            .parse(&content, None)
            .context("Parser returned no tree")?;
        has_errors = tree.root_node().has_error();
        let graph = crate::code_graph::extract(tree.root_node(), &content);
        let mut cursor = tree.walk();
        loop {
            let node = cursor.node();
            let kind = node.kind();
            if kind.contains("function")
                || kind.contains("class")
                || kind.contains("method")
                || kind == "struct_item"
                || kind == "impl_item"
            {
                boundaries.push(node.start_byte());
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(content.as_bytes()).ok())
                {
                    names.push((node.start_byte(), name.chars().take(200).collect()));
                }
            }
            if relations.len() < 10_000
                && (kind.contains("import")
                    || kind == "use_declaration"
                    || kind == "call_expression"
                    || kind == "method_invocation")
                && let Ok(text) = node.utf8_text(content.as_bytes())
            {
                relations.push(format!(
                    "{}:{}:{}",
                    node.start_position().row + 1,
                    kind,
                    text.chars().take(180).collect::<String>()
                ));
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return Ok(Parsed {
                        language: lang.into(),
                        has_errors,
                        chunks: chunk_text(&content, &boundaries, &names),
                        relations,
                        graph,
                    });
                }
            }
        }
    }
    Ok(Parsed {
        language: lang.into(),
        has_errors,
        chunks: chunk_text(&content, &boundaries, &names),
        relations,
        graph: crate::code_graph::CodeGraph::default(),
    })
}
fn chunk_text(content: &str, boundaries: &[usize], names: &[(usize, String)]) -> Vec<Chunk> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut line = 1u32;
    while start < content.len() {
        let mut max_end = start.saturating_add(12_000).min(content.len());
        // Normalize the byte limit before searching the slice for a line boundary.
        while !content.is_char_boundary(max_end) {
            max_end -= 1;
        }
        let mut end = max_end;
        if end < content.len() {
            if let Some(boundary) = boundaries
                .iter()
                .copied()
                .filter(|b| *b > start + 4000 && *b <= max_end)
                .max()
            {
                end = boundary;
            } else if let Some(offset) = content.get(start..max_end).and_then(|s| s.rfind('\n')) {
                end = start + offset + 1;
            }
            while !content.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
        }
        if end <= start {
            break;
        }
        if let Some(slice) = content.get(start..end) {
            let newlines = slice.bytes().filter(|b| *b == b'\n').count() as u32;
            let end_line = line + newlines - u32::from(slice.ends_with('\n'));
            result.push(Chunk {
                start: line,
                end: end_line.max(line),
                symbols: names
                    .iter()
                    .filter(|(b, _)| *b >= start && *b < end)
                    .map(|(_, n)| n.clone())
                    .collect(),
                content: slice.into(),
            });
            line += newlines;
        }
        start = end;
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_byte_limit_preserves_available_line_boundary() {
        let first_line = format!("{}\n", "a".repeat(11989));
        let text = format!("{first_line}{}", "가".repeat(10));
        let chunks = chunk_text(&text, &[0], &[]);
        assert_eq!(chunks[0].content, first_line);
        assert_eq!((chunks[0].start, chunks[0].end), (1, 1));
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.content.as_str())
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn long_unicode_line_is_lossless() {
        let text = "한글🦀".repeat(9000);
        let chunks = chunk_text(&text, &[0], &[]);
        assert!(chunks.len() > 1);
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.content.as_str())
                .collect::<String>(),
            text
        );
        assert!(
            chunks
                .iter()
                .all(|c| c.content.len() <= 12_000 && c.start == 1)
        );
    }
    #[test]
    fn line_ranges_are_exact() {
        let chunks = chunk_text("a\nb\n", &[0], &[]);
        assert_eq!(chunks.first().map(|c| (c.start, c.end)), Some((1, 2)));
    }
}

#[cfg(test)]
mod language_tests {
    use super::*;
    #[test]
    fn eight_language_fixtures_parse_without_errors() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures");
        for name in [
            "service.rs",
            "service.py",
            "service.js",
            "service.ts",
            "Service.java",
            "service.go",
            "service.c",
            "service.cpp",
        ] {
            let path = root.join(name);
            let result = parse_file(&path, language(&path))?;
            assert!(!result.has_errors, "{name}");
            assert!(!result.chunks.is_empty(), "{name}");
            assert_eq!(
                result
                    .chunks
                    .iter()
                    .map(|c| c.content.as_str())
                    .collect::<String>(),
                std::fs::read_to_string(&path)?
            );
        }
        Ok(())
    }
}
