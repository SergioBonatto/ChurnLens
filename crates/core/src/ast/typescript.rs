use super::{CognitiveComplexity, LanguageSupport};
use tree_sitter::{Language, Node};
use tree_sitter_typescript::{language_tsx, language_typescript};

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

    fn cognitive_complexity(&self, node: Node) -> CognitiveComplexity {
        match node.kind() {
            "if_statement" | "for_statement" | "while_statement" | "do_statement"
            | "catch_clause" | "ternary_expression" => CognitiveComplexity::Nesting,
            "switch_statement" => CognitiveComplexity::Structural,
            "binary_expression" => CognitiveComplexity::Logical,
            _ => CognitiveComplexity::None,
        }
    }

    fn extract_name(&self, node: Node, source: &str) -> String {
        direct_identifier(node, source)
            .or_else(|| parent_binding_name(node, source))
            .or_else(|| assignment_name(node, source))
            .unwrap_or_else(|| "<anonymous>".to_string())
    }
}

fn direct_identifier(node: Node, source: &str) -> Option<String> {
    first_identifier_child(node, source)
}

fn parent_binding_name(node: Node, source: &str) -> Option<String> {
    let parent = node.parent()?;
    if matches!(
        parent.kind(),
        "variable_declarator" | "public_field_definition"
    ) {
        first_identifier_child(parent, source)
    } else {
        None
    }
}

fn assignment_name(node: Node, source: &str) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() != "assignment_expression" {
        return None;
    }
    parent
        .child_by_field_name("left")
        .and_then(|left| node_text(left, source))
}

fn first_identifier_child(node: Node, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    let identifier = node
        .children(&mut cursor)
        .find(|child| is_identifier_kind(child.kind()))
        .and_then(|child| node_text(child, source));
    identifier
}

fn is_identifier_kind(kind: &str) -> bool {
    matches!(kind, "identifier" | "property_identifier")
}

fn node_text(node: Node, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes())
        .ok()
        .map(str::to_string)
        .or_else(|| Some("<unknown>".to_string()))
}
