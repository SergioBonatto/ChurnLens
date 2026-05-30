use super::{CognitiveComplexity, LanguageSupport};
use tree_sitter::{Language, Node};

pub struct RustSupport;

impl LanguageSupport for RustSupport {
    fn extensions(&self) -> &[&str] {
        &["rs"]
    }

    fn language(&self) -> Language {
        tree_sitter_rust::language()
    }

    fn is_function(&self, node: Node) -> bool {
        matches!(node.kind(), "function_item" | "method_declaration")
    }

    fn is_complexity_increment(&self, node: Node) -> bool {
        matches!(
            node.kind(),
            "if_expression"
                | "if_let_expression"
                | "match_expression"
                | "for_expression"
                | "while_expression"
                | "loop_expression"
                | "match_arm"
                | "match_pattern"
                | "?"
        )
    }

    fn cognitive_complexity(&self, node: Node) -> CognitiveComplexity {
        match node.kind() {
            "if_expression" | "if_let_expression" | "for_expression" | "while_expression"
            | "loop_expression" => CognitiveComplexity::Nesting,
            "match_expression" => CognitiveComplexity::Structural,
            "binary_expression" => CognitiveComplexity::Logical,
            _ => CognitiveComplexity::None,
        }
    }

    fn extract_name(&self, node: Node, source: &str) -> String {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" {
                return match child.utf8_text(source.as_bytes()) {
                    Ok(text) => text.to_string(),
                    Err(_) => "<unknown>".to_string(),
                };
            }
        }
        "<anonymous>".to_string()
    }
}
