pub mod c;
pub mod engine;
pub mod parser;
pub mod rust;
pub mod typescript;

use tree_sitter::{Language, Node};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CognitiveComplexity {
    None,
    Logical,
    Structural,
    Nesting,
}

pub trait LanguageSupport: Send + Sync {
    fn extensions(&self) -> &[&str];
    fn language(&self) -> Language;
    fn is_function(&self, node: Node) -> bool;
    fn is_complexity_increment(&self, node: Node) -> bool;
    fn cognitive_complexity(&self, node: Node) -> CognitiveComplexity;
    fn extract_name(&self, node: Node, source: &str) -> String;
}
