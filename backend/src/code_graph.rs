//! Immutable syntax graph. Edges describe syntax, never proven dynamic dispatch.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tree_sitter::Node;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start: u32,
    pub end: u32,
}
impl Span {
    fn of(node: Node<'_>) -> Self {
        Self {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start: node.start_position().row as u32 + 1,
            end: node.end_position().row as u32 + u32::from(node.end_position().column > 0),
        }
    }
    pub fn overlaps(&self, start: u32, end: u32) -> bool {
        self.start <= end && self.end >= start
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub parent: Option<String>,
    pub span: Span,
    pub signature: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub source: String,
    pub kind: String,
    /// Exact callee/dependency expression. Resolution is a navigation hint only.
    pub target: String,
    pub span: Span,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodeGraph {
    pub symbols: Vec<Symbol>,
    pub edges: Vec<Edge>,
    pub parse_errors: bool,
    pub supported: bool,
}

fn symbol_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "function_item"
        | "function_definition"
        | "function_declaration"
        | "generator_function_declaration"
        | "function_signature" => "function",
        "method_definition"
        | "method_declaration"
        | "constructor_declaration"
        | "method_signature" => "method",
        "arrow_function" | "function_expression" | "lambda" | "closure_expression" => "closure",
        "class_definition" | "class_declaration" | "class_specifier" | "class" => "class",
        "struct_item"
        | "struct_specifier"
        | "enum_item"
        | "enum_declaration"
        | "enum_specifier"
        | "type_spec"
        | "type_alias_declaration"
        | "type_item" => "type",
        "trait_item" | "interface_declaration" => "interface",
        "impl_item" => "implementation",
        "mod_item" | "namespace_definition" => "module",
        _ => return None,
    })
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or_default()
}

fn declaration_name(node: Node<'_>, source: &str) -> String {
    for field in ["name"] {
        if let Some(value) = node.child_by_field_name(field) {
            return text(value, source).into();
        }
    }
    // C/C++ declarators may wrap pointer/reference declarators.
    let mut value = node.child_by_field_name("declarator");
    while let Some(n) = value {
        if let Some(inner) = n.child_by_field_name("declarator") {
            value = Some(inner);
        } else {
            return text(n, source).into();
        }
    }
    if let Some(value) = node.child_by_field_name("type") {
        return text(value, source).into();
    }
    if let Some(parent) = node.parent()
        && matches!(parent.kind(), "variable_declarator" | "assignment" | "pair")
        && let Some(name) = parent
            .child_by_field_name("name")
            .or_else(|| parent.child_by_field_name("left"))
            .or_else(|| parent.child_by_field_name("key"))
    {
        return text(name, source).into();
    }
    format!("<{}@{}>", node.kind(), node.start_byte())
}

fn edge_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "call_expression"
        | "call"
        | "method_invocation"
        | "new_expression"
        | "object_creation_expression"
        | "macro_invocation" => "calls",
        "import_statement"
        | "import_from_statement"
        | "import_declaration"
        | "use_declaration"
        | "import_spec"
        | "preproc_include" => "imports",
        "superclass"
        | "super_interfaces"
        | "base_class_clause"
        | "extends_clause"
        | "extends_type_clause"
        | "implements_clause"
        | "trait_bounds" => "inherits",
        "if_expression"
        | "if_statement"
        | "match_expression"
        | "match_statement"
        | "switch_statement"
        | "switch_expression"
        | "conditional_expression"
        | "match_arm"
        | "case_clause"
        | "except_clause"
        | "catch_clause"
        | "for_statement"
        | "for_expression"
        | "while_statement"
        | "while_expression" => "branch",
        "return_statement" | "return_expression" | "yield" | "yield_expression" => "returns",
        "throw_statement" | "throw_expression" | "raise_statement" | "try_expression" => {
            "error_path"
        }
        "await_expression" | "await" => "awaits",
        "assignment_expression"
        | "assignment"
        | "augmented_assignment"
        | "compound_assignment_expr"
        | "assignment_statement" => "writes",
        _ => return None,
    })
}

pub fn extract(root: Node<'_>, source: &str) -> CodeGraph {
    let span = Span::of(root);
    let mut graph = CodeGraph {
        symbols: vec![Symbol {
            id: "file".into(),
            name: "<file>".into(),
            qualified_name: "<file>".into(),
            kind: "file".into(),
            parent: None,
            span: span.clone(),
            signature: span,
        }],
        edges: vec![],
        parse_errors: root.has_error(),
        supported: true,
    };
    // Scope is tied to AST identity, not a bare name shared by multiple classes.
    let mut owners = HashMap::new();
    owners.insert(root.id(), 0usize);
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        let owner = node
            .parent()
            .and_then(|p| owners.get(&p.id()).copied())
            .unwrap_or(0);
        let mut current = owner;
        if let Some(kind) = symbol_kind(node.kind()) {
            let name = declaration_name(node, source);
            let id = format!("{}:{}:{}", node.kind(), node.start_byte(), node.end_byte());
            let span = Span::of(node);
            let mut signature = span.clone();
            if let Some(body) = node.child_by_field_name("body") {
                signature.end_byte = body.start_byte();
                signature.end = body.start_position().row as u32 + 1;
            }
            let qualified_name = if owner == 0 {
                if let Some(receiver) = node.child_by_field_name("receiver") {
                    format!("{}::{name}", text(receiver, source))
                } else {
                    name.clone()
                }
            } else {
                format!("{}::{name}", graph.symbols[owner].qualified_name)
            };
            graph.symbols.push(Symbol {
                id,
                name,
                qualified_name,
                kind: kind.into(),
                parent: Some(graph.symbols[owner].id.clone()),
                span,
                signature,
            });
            current = graph.symbols.len() - 1;
        }
        owners.insert(node.id(), current);
        if let Some(base) = node.child_by_field_name("superclasses") {
            graph.edges.push(Edge {
                source: graph.symbols[current].id.clone(),
                kind: "inherits".into(),
                target: text(base, source).into(),
                span: Span::of(base),
            });
        }
        let short_circuit = matches!(
            node.kind(),
            "binary_expression" | "boolean_operator" | "binary_operator"
        ) && node
            .child_by_field_name("operator")
            .is_some_and(|n| matches!(text(n, source), "&&" | "||" | "??" | "and" | "or"));
        if let Some(kind) = edge_kind(node.kind()).or_else(|| short_circuit.then_some("branch")) {
            let target = if kind == "calls" {
                let callee = node
                    .child_by_field_name("function")
                    .or_else(|| node.child_by_field_name("name"))
                    .or_else(|| node.child_by_field_name("macro"))
                    .or_else(|| node.child_by_field_name("type"))
                    .map(|n| text(n, source).to_string())
                    .unwrap_or_default();
                if let Some(object) = node.child_by_field_name("object") {
                    format!("{}.{callee}", text(object, source))
                } else {
                    callee
                }
            } else if matches!(kind, "imports" | "inherits") {
                text(node, source).into()
            } else {
                String::new()
            };
            graph.edges.push(Edge {
                source: graph.symbols[current].id.clone(),
                kind: kind.into(),
                target,
                span: Span::of(node),
            });
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            owners.remove(&cursor.node().id());
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return graph;
            }
        }
    }
}

impl CodeGraph {
    /// Parse cache IDs are file-local. Copies of identical code remain distinct.
    pub fn at_path(mut self, path: &str) -> Self {
        let ids: HashMap<_, _> = self
            .symbols
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    crate::source::hash(format!("{path}:{}", s.id).as_bytes()),
                )
            })
            .collect();
        for symbol in &mut self.symbols {
            symbol.id = ids[&symbol.id].clone();
            symbol.parent = symbol.parent.as_ref().and_then(|p| ids.get(p).cloned());
        }
        for edge in &mut self.edges {
            edge.source = ids[&edge.source].clone();
        }
        self
    }
}

#[cfg(test)]
pub fn parse_test(source: &str, language: &str) -> anyhow::Result<CodeGraph> {
    let file = tempfile::NamedTempFile::new()?;
    std::fs::write(file.path(), source)?;
    Ok(crate::parser::parse_file(file.path(), language)?.graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};

    #[test]
    fn classes_methods_nested_scopes_and_inheritance_keep_distinct_identities() -> Result<()> {
        let graph = parse_test("class Base:\n    pass\nclass A(Base):\n    def save(self):\n        def validate():\n            return True\n        return validate()\nclass B:\n    def save(self):\n        raise ValueError('cancelled')\n", "python")?.at_path("service.py");
        let a = graph
            .symbols
            .iter()
            .find(|s| s.qualified_name == "A::save")
            .context("A.save")?;
        let b = graph
            .symbols
            .iter()
            .find(|s| s.qualified_name == "B::save")
            .context("B.save")?;
        let nested = graph
            .symbols
            .iter()
            .find(|s| s.qualified_name == "A::save::validate")
            .context("nested function")?;
        assert_ne!(a.id, b.id);
        assert_eq!(nested.parent.as_deref(), Some(a.id.as_str()));
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.kind == "calls" && e.source == a.id && e.target == "validate")
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.kind == "error_path" && e.source == b.id)
        );
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.kind == "inherits" && e.target.contains("Base"))
        );
        Ok(())
    }

    #[test]
    fn eight_languages_preserve_declarations_and_exact_source_spans() -> Result<()> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures");
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
            let text = std::fs::read_to_string(&path)?;
            let parsed = crate::parser::parse_file(&path, crate::parser::language(&path))?;
            assert!(
                parsed.graph.supported && !parsed.graph.parse_errors,
                "{name}"
            );
            let functions: Vec<_> = parsed
                .graph
                .symbols
                .iter()
                .filter(|s| matches!(s.kind.as_str(), "method" | "function"))
                .collect();
            assert!(!functions.is_empty(), "{name}");
            for s in functions {
                assert!(
                    text[s.span.start_byte..s.span.end_byte].contains(&s.name),
                    "{name}: {}",
                    s.name
                );
                assert!(
                    !["int", "void", "String"].contains(&s.name.as_str()),
                    "return type misidentified as name in {name}"
                );
            }
            assert!(
                parsed.graph.edges.iter().any(|e| e.kind == "branch"),
                "{name}"
            );
        }
        Ok(())
    }

    #[test]
    fn graph_does_not_truncate_relations_or_symbols_at_old_limits() -> Result<()> {
        let text = format!(
            "fn entry() {{ {} }}\n{}",
            "work();".repeat(10_050),
            (0..130)
                .map(|n| format!("fn helper_{n}() {{}}\n"))
                .collect::<String>()
        );
        let graph = parse_test(&text, "rust")?;
        assert_eq!(
            graph.edges.iter().filter(|e| e.kind == "calls").count(),
            10_050
        );
        assert_eq!(graph.symbols.len(), 132);
        let a = graph.clone().at_path("a.rs");
        let b = graph.clone().at_path("b.rs");
        assert_ne!(a.symbols[1].id, b.symbols[1].id);
        assert_eq!(
            serde_json::to_value(&a)?,
            serde_json::to_value(graph.at_path("a.rs"))?
        );
        Ok(())
    }

    #[test]
    fn go_receiver_methods_and_javascript_arrow_functions_are_named() -> Result<()> {
        let go = parse_test(
            "package p\ntype A struct{}\ntype B struct{}\nfunc (a A) Save() {}\nfunc (b B) Save() {}",
            "go",
        )?;
        let methods: Vec<_> = go.symbols.iter().filter(|s| s.name == "Save").collect();
        assert_eq!(methods.len(), 2);
        assert_ne!(methods[0].qualified_name, methods[1].qualified_name);
        let js = parse_test(
            "export const save = () => { if (cancelled) throw new Error('cancel'); return store(); };",
            "javascript",
        )?;
        assert!(
            js.symbols
                .iter()
                .any(|s| s.name == "save" && s.kind == "closure")
        );
        assert!(js.edges.iter().any(|e| e.kind == "error_path"));
        Ok(())
    }
}
