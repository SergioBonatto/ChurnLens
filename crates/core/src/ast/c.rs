use super::{CognitiveComplexity, LanguageSupport};
use tree_sitter::{Language, Node};

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

    fn cognitive_complexity(&self, node: Node) -> CognitiveComplexity {
        match node.kind() {
            "if_statement"
            | "for_statement"
            | "while_statement"
            | "do_statement"
            | "conditional_expression" => CognitiveComplexity::Nesting,
            "switch_statement" => CognitiveComplexity::Structural,
            "binary_expression" => CognitiveComplexity::Logical,
            _ => CognitiveComplexity::None,
        }
    }

    fn extract_name(&self, node: Node, source: &str) -> String {
        let Some(declarator) = first_child_of_kind(node, "function_declarator") else {
            return "<anonymous>".to_string();
        };
        let Some(identifier) = first_child_of_kind(declarator, "identifier") else {
            return "<anonymous>".to_string();
        };

        node_text_or_unknown(identifier, source)
    }
}

fn first_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let child = node
        .children(&mut cursor)
        .find(|child| child.kind() == kind);
    child
}

fn node_text_or_unknown(node: Node, source: &str) -> String {
    match node.utf8_text(source.as_bytes()) {
        Ok(text) => text.to_string(),
        Err(_) => "<unknown>".to_string(),
    }
}
