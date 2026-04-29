use crate::metrics::FunctionMetrics;
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use tree_sitter::{Node, Query, QueryCursor};

static FUNCTION_QUERY: Lazy<std::result::Result<Query, String>> = Lazy::new(|| {
    let query_str = r#"
        [
            (function_declaration) @func
            (arrow_function) @func
            (method_definition) @func
            (function_expression) @func
        ]
    "#;
    let language = tree_sitter_typescript::language_typescript();
    Query::new(language, query_str).map_err(|err| err.to_string())
});

static COMPLEXITY_QUERY: Lazy<std::result::Result<Query, String>> = Lazy::new(|| {
    let query_str = r#"
        [
            "if"
            "for"
            "while"
            "do"
            "case"
            "catch"
            "&&"
            "||"
            "?"
        ] @item
    "#;
    let language = tree_sitter_typescript::language_typescript();
    Query::new(language, query_str).map_err(|err| err.to_string())
});

pub struct ComplexityEngine<'a> {
    source: &'a str,
    file_path: &'a str,
}

impl<'a> ComplexityEngine<'a> {
    pub fn new(source: &'a str, file_path: &'a str) -> Self {
        Self { source, file_path }
    }

    pub fn analyze(&self, root_node: Node) -> Result<Vec<FunctionMetrics>> {
        let mut functions = Vec::new();
        let mut cursor = QueryCursor::new();

        let matches = cursor.matches(function_query()?, root_node, self.source.as_bytes());

        for m in matches {
            for capture in m.captures {
                if let Some(metrics) = self.extract_metrics(capture.node)? {
                    functions.push(metrics);
                }
            }
        }

        Ok(functions)
    }

    fn extract_metrics(&self, node: Node) -> Result<Option<FunctionMetrics>> {
        let name = self.extract_name(node).to_string();
        let line = node.start_position().row as u32 + 1;
        let cyclomatic_complexity = self.calculate_cyclomatic_complexity(node)?;
        let (cognitive_complexity, nesting_depth) = self.calculate_cognitive_and_nesting(node);
        let lines_of_code = (node.end_position().row - node.start_position().row + 1) as u32;

        Ok(Some(FunctionMetrics {
            id: format!("{}:{}:{}", self.file_path, name, line),
            name,
            file: self.file_path.to_string(),
            line,
            cyclomatic_complexity,
            cognitive_complexity,
            nesting_depth,
            lines_of_code,
            times_modified: 0,
            bug_fix_commits: 0,
            authors_count: 0,
            churn_score: 0.0,
            normalized: None,
            risk: None,
            percentile: None,
        }))
    }

    fn extract_name(&self, node: Node) -> &'a str {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" || child.kind() == "property_identifier" {
                return match child.utf8_text(self.source.as_bytes()) {
                    Ok(text) => text,
                    Err(_) => "<unknown>",
                };
            }
        }

        if let Some(parent) = node.parent() {
            if parent.kind() == "variable_declarator" || parent.kind() == "public_field_definition"
            {
                let mut p_cursor = parent.walk();
                for child in parent.children(&mut p_cursor) {
                    if child.kind() == "identifier" || child.kind() == "property_identifier" {
                        return match child.utf8_text(self.source.as_bytes()) {
                            Ok(text) => text,
                            Err(_) => "<unknown>",
                        };
                    }
                }
            }
            if parent.kind() == "assignment_expression" {
                if let Some(left) = parent.child_by_field_name("left") {
                    return match left.utf8_text(self.source.as_bytes()) {
                        Ok(text) => text,
                        Err(_) => "<unknown>",
                    };
                }
            }
        }

        "<anonymous>"
    }

    fn calculate_cyclomatic_complexity(&self, node: Node) -> Result<u32> {
        let _ = complexity_query()?;
        Ok(1 + self.count_cyclomatic_items(node))
    }

    fn count_cyclomatic_items(&self, node: Node) -> u32 {
        let mut count = u32::from(is_complexity_item(node.kind()));
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            if is_function_node(child) {
                continue;
            }
            count += self.count_cyclomatic_items(child);
        }

        count
    }

    fn calculate_cognitive_and_nesting(&self, node: Node) -> (u32, u32) {
        let mut cognitive = 0;
        let mut max_depth = 0;
        self.walk_cognitive(node, 0, &mut cognitive, &mut max_depth, None);
        (cognitive, max_depth)
    }

    fn walk_cognitive(
        &self,
        node: Node,
        depth: u32,
        cognitive: &mut u32,
        max_depth: &mut u32,
        last_op: Option<&str>,
    ) {
        let kind = node.kind();
        let mut new_depth = depth;

        match kind {
            "if_statement" | "for_statement" | "while_statement" | "do_statement"
            | "switch_statement" | "catch_clause" | "ternary_expression" => {
                let is_else_if = kind == "if_statement"
                    && node.parent().is_some_and(|p| p.kind() == "else_clause");

                if is_else_if {
                    *cognitive += 1;
                } else {
                    *cognitive += 1 + depth;
                    new_depth += 1;
                }
            }
            "binary_expression" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    let op = child.kind();
                    if op == "&&" || op == "||" {
                        if last_op != Some(op) {
                            *cognitive += 1;
                        }
                        let mut walk_cursor = node.walk();
                        for inner_child in node.children(&mut walk_cursor) {
                            if is_function_node(inner_child) {
                                continue;
                            }
                            self.walk_cognitive(
                                inner_child,
                                new_depth,
                                cognitive,
                                max_depth,
                                Some(op),
                            );
                        }
                        return;
                    }
                }
            }
            _ => {}
        }

        if new_depth > *max_depth {
            *max_depth = new_depth;
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if is_function_node(child) {
                continue;
            }
            self.walk_cognitive(child, new_depth, cognitive, max_depth, None);
        }
    }
}

fn function_query() -> Result<&'static Query> {
    FUNCTION_QUERY
        .as_ref()
        .map_err(|err| anyhow!("Failed to initialize function query: {}", err))
}

fn complexity_query() -> Result<&'static Query> {
    COMPLEXITY_QUERY
        .as_ref()
        .map_err(|err| anyhow!("Failed to initialize complexity query: {}", err))
}

fn is_function_node(node: Node) -> bool {
    matches!(
        node.kind(),
        "function_declaration" | "function_expression" | "arrow_function" | "method_definition"
    )
}

fn is_complexity_item(kind: &str) -> bool {
    matches!(
        kind,
        "if" | "for" | "while" | "do" | "case" | "catch" | "&&" | "||" | "?"
    )
}

#[cfg(test)]
mod tests {
    use super::super::parser::TypeScriptAnalyzer;
    use crate::metrics::FunctionMetrics;

    fn analyze(source: &str) -> Vec<FunctionMetrics> {
        TypeScriptAnalyzer::analyze_source(source, "file.ts").expect("source should parse")
    }

    fn find<'a>(functions: &'a [FunctionMetrics], name: &str) -> &'a FunctionMetrics {
        functions
            .iter()
            .find(|function| function.name == name)
            .expect("function should exist")
    }

    #[test]
    fn simple_function_counts_branch_complexity() {
        let functions = analyze(
            r#"
            function a() {
                if (x) {}
            }
            "#,
        );

        let function = find(&functions, "a");

        assert!(function.cyclomatic_complexity > 1);
    }

    #[test]
    fn nested_function_does_not_inflate_parent_complexity() {
        let functions = analyze(
            r#"
            function outer() {
                function inner() {
                    if (x) {}
                }
            }
            "#,
        );

        let outer = find(&functions, "outer");
        let inner = find(&functions, "inner");

        assert_eq!(outer.cyclomatic_complexity, 1);
        assert_eq!(outer.cognitive_complexity, 0);
        assert_eq!(inner.cyclomatic_complexity, 2);
        assert_eq!(inner.cognitive_complexity, 1);
    }

    #[test]
    fn arrow_function_counts_branch_complexity() {
        let functions = analyze(
            r#"
            const f = () => {
                if (x) {}
            };
            "#,
        );

        let function = find(&functions, "f");

        assert_eq!(function.cyclomatic_complexity, 2);
    }
}
