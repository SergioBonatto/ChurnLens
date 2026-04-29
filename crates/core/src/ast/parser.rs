use crate::metrics::FunctionMetrics;
use std::fs;
use tree_sitter::{Language, Parser};
use tree_sitter_typescript::{language_tsx, language_typescript};

use super::engine::ComplexityEngine;

pub struct TypeScriptAnalyzer;

impl TypeScriptAnalyzer {
    pub fn analyze_source(source: &str, file_path: &str) -> anyhow::Result<Vec<FunctionMetrics>> {
        let mut parser = Parser::new();
        parser.set_language(Self::language_for_path(file_path))?;

        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", file_path))?;
        if tree.root_node().has_error() {
            anyhow::bail!("Failed to parse {}: syntax errors detected", file_path);
        }

        let engine = ComplexityEngine::new(source, file_path);
        engine.analyze(tree.root_node())
    }

    pub fn parse_file(path: &str) -> anyhow::Result<Vec<FunctionMetrics>> {
        let source = fs::read_to_string(path)?;
        let mut parser = Parser::new();
        parser.set_language(Self::language_for_path(path))?;

        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", path))?;
        if tree.root_node().has_error() {
            anyhow::bail!("Failed to parse {}: syntax errors detected", path);
        }

        let engine = ComplexityEngine::new(&source, path);
        engine.analyze(tree.root_node())
    }

    fn language_for_path(path: &str) -> Language {
        if path.ends_with(".tsx") || path.ends_with(".jsx") {
            language_tsx()
        } else {
            language_typescript()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TypeScriptAnalyzer;

    #[test]
    fn parses_valid_typescript() {
        let source = r#"
            function a() {
                if (x) {}
            }
        "#;

        let functions = TypeScriptAnalyzer::analyze_source(source, "file.ts")
            .expect("valid TypeScript should parse");

        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "a");
    }

    #[test]
    fn parses_valid_tsx_with_jsx() {
        let source = r#"
            function View() {
                return <div>{value}</div>;
            }
        "#;

        let functions =
            TypeScriptAnalyzer::analyze_source(source, "file.tsx").expect("valid TSX should parse");

        assert_eq!(functions.len(), 1);
    }

    #[test]
    fn invalid_syntax_returns_error() {
        let source = "function broken( {";

        let result = TypeScriptAnalyzer::analyze_source(source, "file.ts");

        assert!(result.is_err());
    }
}
