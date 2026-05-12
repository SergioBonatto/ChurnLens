use crate::metrics::FunctionMetrics;
use tree_sitter::Parser;
use super::LanguageSupport;
use super::engine::ComplexityEngine;

pub struct AstParser;

impl AstParser {
    pub fn analyze_source(
        source: &str,
        file_path: &str,
        support: &dyn LanguageSupport,
    ) -> anyhow::Result<Vec<FunctionMetrics>> {
        let mut parser = Parser::new();
        parser.set_language(support.language())?;

        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse {}", file_path))?;
        if tree.root_node().has_error() {
            anyhow::bail!("Failed to parse {}: syntax errors detected", file_path);
        }

        let engine = ComplexityEngine::new(source, file_path, support);
        engine.analyze(tree.root_node())
    }
}

#[cfg(test)]
mod tests {
    use super::AstParser;
    use crate::ast::typescript::TypeScriptSupport;

    #[test]
    fn parses_valid_typescript() {
        let source = r#"
            function a() {
                if (x) {}
            }
        "#;
        let support = TypeScriptSupport::new(false);

        let functions = AstParser::analyze_source(source, "file.ts", &support)
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
        let support = TypeScriptSupport::new(true);

        let functions = AstParser::analyze_source(source, "file.tsx", &support)
            .expect("valid TSX should parse");

        assert_eq!(functions.len(), 1);
    }

    #[test]
    fn invalid_syntax_returns_error() {
        let source = "function broken( {";
        let support = TypeScriptSupport::new(false);

        let result = AstParser::analyze_source(source, "file.ts", &support);

        assert!(result.is_err());
    }
}
