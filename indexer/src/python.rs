use tree_sitter::{Node, Parser};

pub fn is_marimo_notebook(source: impl AsRef<[u8]>) -> bool {
    let source = source.as_ref();
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let tree = match parser.parse(source, None) {
        Some(tree) => tree,
        None => return false,
    };
    let mut flags = ScanFlags::default();
    scan_node(tree.root_node(), source, false, &mut flags);
    flags.has_app && flags.has_import
}

#[derive(Default)]
struct ScanFlags {
    has_app: bool,
    has_import: bool,
}

fn scan_node(node: Node, source: &[u8], in_scope: bool, flags: &mut ScanFlags) {
    let kind = node.kind();
    let now_in_scope = in_scope || is_scope_boundary(kind);
    if !now_in_scope {
        match kind {
            "assignment" | "augmented_assignment" => {
                if assignment_targets_app(node, source) {
                    flags.has_app = true;
                }
            }
            "import_statement" => {
                if import_statement_has_marimo(node, source) {
                    flags.has_import = true;
                }
            }
            "import_from_statement" => {
                if import_from_statement_has_marimo(node, source) {
                    flags.has_import = true;
                }
            }
            _ => {}
        }
    }
    if flags.has_app && flags.has_import {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_node(child, source, now_in_scope, flags);
        if flags.has_app && flags.has_import {
            return;
        }
    }
}

fn is_scope_boundary(kind: &str) -> bool {
    matches!(kind, "function_definition" | "class_definition" | "lambda")
}

fn assignment_targets_app(node: Node, source: &[u8]) -> bool {
    let Some(left) = node.child_by_field_name("left") else {
        return false;
    };
    pattern_has_identifier(left, source, "app")
}

fn pattern_has_identifier(node: Node, source: &[u8], name: &str) -> bool {
    match node.kind() {
        "identifier" => node_text(node, source).is_some_and(|text| text == name),
        "attribute" | "subscript" => false,
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if pattern_has_identifier(child, source, name) {
                    return true;
                }
            }
            false
        }
    }
}

fn import_statement_has_marimo(node: Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                if dotted_name_is_marimo(child, source) {
                    return true;
                }
            }
            "aliased_import" => {
                if let Some(name) = child.child_by_field_name("name")
                    && dotted_name_is_marimo(name, source)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn import_from_statement_has_marimo(node: Node, source: &[u8]) -> bool {
    let Some(module_name) = node.child_by_field_name("module_name") else {
        return false;
    };
    match module_name.kind() {
        "dotted_name" => dotted_name_is_marimo(module_name, source),
        "relative_import" => {
            let mut cursor = module_name.walk();
            for child in module_name.children(&mut cursor) {
                if child.kind() == "dotted_name" && dotted_name_is_marimo(child, source) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

fn dotted_name_is_marimo(node: Node, source: &[u8]) -> bool {
    let Some(text) = node_text(node, source) else {
        return false;
    };
    text == "marimo" || text.starts_with("marimo.")
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

#[cfg(test)]
mod tests {
    use super::is_marimo_notebook;

    fn check(source: &str) -> bool {
        is_marimo_notebook(source.as_bytes())
    }

    #[test]
    fn detects_import_marimo_and_app_assignment() {
        let source = r#"
import marimo

app = marimo.App()
"#;
        assert!(check(source));
    }

    #[test]
    fn detects_from_marimo_import_and_app_assignment() {
        let source = r#"
from marimo import App as M

app = M()
"#;
        assert!(check(source));
    }

    #[test]
    fn rejects_missing_marimo_import() {
        let source = r#"
app = object()
"#;
        assert!(!check(source));
    }

    #[test]
    fn rejects_missing_app_assignment() {
        let source = r#"
import marimo

def make():
    return marimo.App()
"#;
        assert!(!check(source));
    }

    #[test]
    fn rejects_app_assignment_in_function_scope() {
        let source = r#"
import marimo

def build():
    app = marimo.App()
    return app
"#;
        assert!(!check(source));
    }

    #[test]
    fn rejects_attribute_assignment_only() {
        let source = r#"
import marimo as mo

config.app = mo.App()
"#;
        assert!(!check(source));
    }

    #[test]
    fn detects_tuple_assignment_with_app() {
        let source = r#"
import marimo

app, other = marimo.App(), 1
"#;
        assert!(check(source));
    }

    #[test]
    fn detects_annotated_assignment() {
        let source = r#"
import marimo

app: marimo.App = marimo.App()
"#;
        assert!(check(source));
    }

    #[test]
    fn detects_import_marimo_as_alias() {
        let source = r#"
import marimo as mo

app = mo.App()
"#;
        assert!(check(source));
    }

    #[test]
    fn rejects_similar_module_name() {
        let source = r#"
import marimo_tools

app = marimo_tools.App()
"#;
        assert!(!check(source));
    }
}
