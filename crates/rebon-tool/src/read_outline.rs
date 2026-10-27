//! Syntax-aware outlines. A malformed file is rejected as a whole rather than
//! returning potentially misleading boundaries from tree-sitter's recovery tree.
use std::path::Path;

use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Symbol {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Return source-ordered symbols with one-based, inclusive line boundaries.
/// Unsupported extensions and syntactically invalid input return `None`.
pub(crate) fn outline(path: &Path, content: &str) -> Option<Vec<Symbol>> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let language: Language = match ext.as_str() {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" | "pyi" => tree_sitter_python::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => return None,
    };
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut symbols = Vec::new();
    visit(root, content, &mut symbols);
    Some(symbols)
}

fn text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}

fn visit(node: Node<'_>, source: &str, symbols: &mut Vec<Symbol>) {
    let node_kind = node.kind();
    let kind = match node_kind {
        "function_item"
        | "function_signature_item"
        | "function_definition"
        | "function_declaration"
        | "generator_function_declaration" => Some("function"),
        "method_definition" | "method_signature" | "abstract_method_signature" => Some("method"),
        "impl_item" => Some("impl"),
        "class_definition" | "class_declaration" | "abstract_class_declaration" => Some("class"),
        _ => None,
    };
    if let Some(kind) = kind {
        let name = if node_kind == "impl_item" {
            // Preserve generics and trait qualification; the body is not part of the name.
            node.child_by_field_name("body")
                .and_then(|body| source.get(node.start_byte()..body.start_byte()))
                .map(str::trim)
        } else {
            node.child_by_field_name("name")
                .and_then(|n| text(n, source))
        };
        if let Some(name) = name.filter(|s| !s.is_empty()) {
            // Python decorators are siblings of the definition under this wrapper.
            let boundary = node
                .parent()
                .filter(|p| p.kind() == "decorated_definition")
                .unwrap_or(node);
            push(symbols, name, kind, boundary);
        }
    }
    // JavaScript/TypeScript arrow and function expressions assigned to a name.
    if node_kind == "variable_declarator" {
        if let (Some(name), Some(value)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("value"),
        ) {
            if name.kind() == "identifier"
                && matches!(
                    value.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                )
            {
                if let Some(name) = text(name, source) {
                    push(symbols, name, "function", node);
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, source, symbols);
    }
}

fn push(symbols: &mut Vec<Symbol>, name: &str, kind: &str, node: Node<'_>) {
    let end = node.end_position();
    // Tree-sitter's end is exclusive; an end at column zero belongs to the
    // previous line. Nonempty declarations always start before their end.
    let end_line = if end.column == 0 {
        end.row
    } else {
        end.row + 1
    };
    symbols.push(Symbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        start: node.start_position().row + 1,
        end: end_line,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(path: &str, source: &str) -> Vec<Symbol> {
        outline(Path::new(path), source).unwrap()
    }
    fn sym(name: &str, kind: &str, start: usize, end: usize) -> Symbol {
        Symbol {
            name: name.into(),
            kind: kind.into(),
            start,
            end,
        }
    }

    #[test]
    fn rust_impl_nested_and_decoys() {
        let src = "// fn fake() {}\nimpl<T> Trait for Thing<T> {\n    fn run(&self) {\n        let s = \"} fn fake() {}\";\n        fn nested() {}\n    }\n}\n";
        assert_eq!(
            parse("x.rs", src),
            vec![
                sym("impl<T> Trait for Thing<T>", "impl", 2, 7),
                sym("run", "function", 3, 6),
                sym("nested", "function", 5, 5)
            ]
        );
    }

    #[test]
    fn python_decorators_and_nested_methods() {
        let src = "class C:\n    @staticmethod\n    def f():\n        s = 'def fake(): pass'\n        def inner():\n            pass\n        return inner\n";
        assert_eq!(
            parse("x.py", src),
            vec![
                sym("C", "class", 1, 7),
                sym("f", "function", 2, 7),
                sym("inner", "function", 5, 6)
            ]
        );
    }

    #[test]
    fn javascript_and_typescript() {
        let src = "// function fake() {}\nclass C {\n  method() { return `}`; }\n}\nconst arrow = (x) => { return x; };\nfunction real() {}\n";
        assert_eq!(
            parse("x.js", src),
            vec![
                sym("C", "class", 2, 4),
                sym("method", "method", 3, 3),
                sym("arrow", "function", 5, 5),
                sym("real", "function", 6, 6)
            ]
        );
        let ts = "class C {\n  method(x: number): number { return x; }\n}\nconst view = () => <div />;\n";
        assert_eq!(
            parse("x.tsx", ts),
            vec![
                sym("C", "class", 1, 3),
                sym("method", "method", 2, 2),
                sym("view", "function", 4, 4)
            ]
        );
        assert!(outline(
            Path::new("x.ts"),
            "function f(x: number): number { return x; }"
        )
        .is_some());
    }

    #[test]
    fn invalid_or_unsupported_are_not_outlines() {
        assert_eq!(outline(Path::new("x.rs"), "fn broken( {"), None);
        assert_eq!(outline(Path::new("x.py"), "def broken(:\n pass"), None);
        assert_eq!(outline(Path::new("x.txt"), "fn f() {}"), None);
        assert_eq!(parse("x.RS", "// nothing\n"), Vec::<Symbol>::new());
    }

    #[test]
    fn unicode_and_single_line_boundaries() {
        assert_eq!(
            parse("x.rs", "fn café() {}\nfn last() {}"),
            vec![sym("café", "function", 1, 1), sym("last", "function", 2, 2)]
        );
    }
}
