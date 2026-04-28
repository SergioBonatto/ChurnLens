use crate::metrics::FunctionMetrics;
use std::fs;
use tree_sitter::Parser;
use tree_sitter_typescript::language_typescript;

use super::engine::ComplexityEngine;

pub struct TypeScriptAnalyzer;

impl TypeScriptAnalyzer {
    pub fn analyze_source<'a>(source: &'a str, file_path: &'a str) -> anyhow::Result<Vec<FunctionMetrics<'a>>> {
        let mut parser = Parser::new();
        parser.set_language(language_typescript())?;

        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", file_path))?;

        let engine = ComplexityEngine::new(source, file_path);
        Ok(engine.analyze(tree.root_node()))
    }

    pub fn parse_file(path: &str) -> anyhow::Result<Vec<FunctionMetrics<'static>>> {
        let source = fs::read_to_string(path)?;
        let mut parser = Parser::new();
        parser.set_language(language_typescript())?;
        
        let tree = parser.parse(&source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", path))?;
            
        let engine = ComplexityEngine::new(&source, path);
        let functions = engine.analyze(tree.root_node());
        
        Ok(functions.into_iter().map(|f| {
            FunctionMetrics {
                name: std::borrow::Cow::Owned(f.name.into_owned()),
                file: std::borrow::Cow::Owned(f.file.into_owned()),
                line: f.line,
                cyclomatic_complexity: f.cyclomatic_complexity,
                cognitive_complexity: f.cognitive_complexity,
                nesting_depth: f.nesting_depth,
                lines_of_code: f.lines_of_code,
                times_modified: f.times_modified,
                bug_fix_commits: f.bug_fix_commits,
                authors_count: f.authors_count,
                churn_score: f.churn_score,
            }
        }).collect())
    }
}
