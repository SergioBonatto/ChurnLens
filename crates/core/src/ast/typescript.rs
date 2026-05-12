use tree_sitter::{Language, Node};
use tree_sitter_typescript::{language_tsx, language_typescript};
use super::LanguageSupport;

pub struct TypeScriptSupport {
    is_tsx: bool,
}

impl TypeScriptSupport {
    pub fn new(is_tsx: bool) -> Self {
        Self { is_tsx }
    }
}

impl LanguageSupport for TypeScriptSupport {
    fn extensions(&self) -> &[&str] {
        if self.is_tsx {
            &["tsx", "jsx"]
        } else {
            &["ts", "js"]
        }
    }

    fn language(&self) -> Language {
        if self.is_tsx {
            language_tsx()
        } else {
            language_typescript()
        }
    }

    fn is_function(&self, node: Node) -> bool {
        matches!(
            node.kind(),
            "function_declaration" | "function_expression" | "arrow_function" | "method_definition"
        )
    }

    fn is_complexity_increment(&self, node: Node) -> bool {
        matches!(
            node.kind(),
            "if" | "for" | "while" | "do" | "case" | "catch" | "&&" | "||" | "?"
        )
    }

    fn extract_name(&self, node: Node, source: &str) -> String {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" || child.kind() == "property_identifier" {
                return match child.utf8_text(source.as_bytes()) {
                    Ok(text) => text.to_string(),
                    Err(_) => "<unknown>".to_string(),
                };
            }
        }

        if let Some(parent) = node.parent() {
            if parent.kind() == "variable_declarator" || parent.kind() == "public_field_definition"
            {
                let mut p_cursor = parent.walk();
                for child in parent.children(&mut p_cursor) {
                    if child.kind() == "identifier" || child.kind() == "property_identifier" {
                        return match child.utf8_text(source.as_bytes()) {
                            Ok(text) => text.to_string(),
                            Err(_) => "<unknown>".to_string(),
                        };
                    }
                }
            }
            if parent.kind() == "assignment_expression" {
                if let Some(left) = parent.child_by_field_name("left") {
                    return match left.utf8_text(source.as_bytes()) {
                        Ok(text) => text.to_string(),
                        Err(_) => "<unknown>".to_string(),
                    };
                }
            }
        }

        "<anonymous>".to_string()
    }
}
