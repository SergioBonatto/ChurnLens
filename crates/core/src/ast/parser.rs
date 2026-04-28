use crate::metrics::FunctionMetrics;
use std::fs;
use tree_sitter::Parser;
use tree_sitter_typescript::language_typescript;

use super::engine::ComplexityEngine;

pub struct TypeScriptAnalyzer;

impl TypeScriptAnalyzer {
    pub fn analyze_source(source: &str, file_path: &str) -> anyhow::Result<Vec<FunctionMetrics>> {
        let mut parser = Parser::new();
        parser.set_language(language_typescript())?;

        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", file_path))?;

        let engine = ComplexityEngine::new(source, file_path);
        Ok(engine.analyze(tree.root_node()))
    }

    pub fn parse_file(path: &str) -> anyhow::Result<Vec<FunctionMetrics>> {
        let source = fs::read_to_string(path)?;
        let mut parser = Parser::new();
        parser.set_language(language_typescript())?;

        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", path))?;

        let engine = ComplexityEngine::new(&source, path);
        let functions = engine.analyze(tree.root_node());

        Ok(functions)
    }
}
