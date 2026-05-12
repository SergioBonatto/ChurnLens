use tree_sitter::{Language, Node};
use super::LanguageSupport;

pub struct CSupport;

impl LanguageSupport for CSupport {
    fn extensions(&self) -> &[&str] {
        &["c", "h"]
    }

    fn language(&self) -> Language {
        tree_sitter_c::language()
    }

    fn is_function(&self, node: Node) -> bool {
        matches!(node.kind(), "function_definition")
    }

    fn is_complexity_increment(&self, node: Node) -> bool {
        matches!(
            node.kind(),
            "if_statement"
                | "for_statement"
                | "while_statement"
                | "do_statement"
                | "case_statement"
                | "&&"
                | "||"
                | "conditional_expression"
        )
    }

    fn extract_name(&self, node: Node, source: &str) -> String {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "function_declarator" {
                let mut d_cursor = child.walk();
                for d_child in child.children(&mut d_cursor) {
                    if d_child.kind() == "identifier" {
                        return match d_child.utf8_text(source.as_bytes()) {
                            Ok(text) => text.to_string(),
                            Err(_) => "<unknown>".to_string(),
                        };
                    }
                }
            }
        }
        "<anonymous>".to_string()
    }
}
