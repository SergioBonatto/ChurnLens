use crate::metrics::FunctionMetrics;
use anyhow::Result;
use tree_sitter::Node;

use super::LanguageSupport;

pub struct ComplexityEngine<'a> {
    source: &'a str,
    file_path: &'a str,
    support: &'a dyn LanguageSupport,
}

struct FunctionState {
    id: String,
    name: String,
    start_line: u32,
    cyclomatic: u32,
    cognitive: u32,
    max_nesting: u32,
    output_index: usize,
}

impl<'a> ComplexityEngine<'a> {
    pub fn new(source: &'a str, file_path: &'a str, support: &'a dyn LanguageSupport) -> Self {
        Self {
            source,
            file_path,
            support,
        }
    }

    pub fn analyze(&self, root_node: Node) -> Result<Vec<FunctionMetrics>> {
        let mut stack = Vec::new();
        let mut functions = Vec::new();
        self.visit(root_node, &mut stack, &mut functions, 0, None);

        Ok(functions.into_iter().flatten().collect())
    }

    fn visit(
        &self,
        node: Node,
        stack: &mut Vec<FunctionState>,
        functions: &mut Vec<Option<FunctionMetrics>>,
        depth: u32,
        last_op: Option<&str>,
    ) {
        let entered_function = if self.support.is_function(node) {
            let name = self.support.extract_name(node, self.source);
            let start_line = node.start_position().row as u32 + 1;
            let output_index = functions.len();
            functions.push(None);
            stack.push(FunctionState {
                id: format!("{}:{}:{}", self.file_path, name, start_line),
                name,
                start_line,
                cyclomatic: 1,
                cognitive: 0,
                max_nesting: 0,
                output_index,
            });
            true
        } else {
            false
        };

        let active_depth = if entered_function { 0 } else { depth };
        let mut child_depth = active_depth;

        if let Some(function) = stack.last_mut() {
            let kind = node.kind();
            if !entered_function && self.support.is_complexity_increment(node) {
                function.cyclomatic += 1;
            }

            match kind {
                "if_statement" | "for_statement" | "while_statement" | "do_statement"
                | "switch_statement" | "catch_clause" | "ternary_expression" | "if_expression"
                | "match_expression" | "match_arm" => {
                    let is_else_if = kind == "if_statement"
                        && node.parent().is_some_and(|p| p.kind() == "else_clause");

                    if is_else_if {
                        function.cognitive += 1;
                    } else {
                        function.cognitive += 1 + active_depth;
                        child_depth += 1;
                    }
                }
                "binary_expression" => {
                    if let Some(op) = logical_operator(node) {
                        if last_op != Some(op) {
                            function.cognitive += 1;
                        }

                        if child_depth > function.max_nesting {
                            function.max_nesting = child_depth;
                        }

                        let mut cursor = node.walk();
                        for child in node.children(&mut cursor) {
                            self.visit(child, stack, functions, child_depth, Some(op));
                        }

                        if entered_function {
                            self.finish_function(node, stack, functions);
                        }
                        return;
                    }
                }
                _ => {}
            }

            if child_depth > function.max_nesting {
                function.max_nesting = child_depth;
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.visit(child, stack, functions, child_depth, None);
        }

        if entered_function {
            self.finish_function(node, stack, functions);
        }
    }

    fn finish_function(
        &self,
        node: Node,
        stack: &mut Vec<FunctionState>,
        functions: &mut [Option<FunctionMetrics>],
    ) {
        let function = stack
            .pop()
            .expect("function stack should contain entered function");
        let lines_of_code = (node.end_position().row as u32 + 1) - function.start_line + 1;
        functions[function.output_index] = Some(FunctionMetrics {
            id: function.id,
            name: function.name,
            file: self.file_path.to_string(),
            line: function.start_line,
            cyclomatic_complexity: function.cyclomatic,
            cognitive_complexity: function.cognitive,
            nesting_depth: function.max_nesting,
            lines_of_code,
            times_modified: 0,
            bug_fix_commits: 0,
            authors_count: 0,
            churn_score: 0.0,
            normalized: None,
            risk: None,
            percentile: None,
        });
    }
}

fn logical_operator(node: Node) -> Option<&'static str> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "&&" => return Some("&&"),
            "||" => return Some("||"),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::super::parser::AstParser;
    use crate::ast::typescript::TypeScriptSupport;
    use crate::metrics::FunctionMetrics;

    fn analyze(source: &str) -> Vec<FunctionMetrics> {
        let support = TypeScriptSupport::new(false);
        AstParser::analyze_source(source, "file.ts", &support).expect("source should parse")
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

        assert_eq!(function.cyclomatic_complexity, 2);
        assert_eq!(function.cognitive_complexity, 1);
        assert_eq!(function.nesting_depth, 1);
        assert!(function.lines_of_code >= 3);
        assert!(function.lines_of_code <= 5);
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
        assert_eq!(outer.nesting_depth, 0);
        assert_eq!(inner.cyclomatic_complexity, 2);
        assert_eq!(inner.cognitive_complexity, 1);
        assert_eq!(inner.nesting_depth, 1);
        assert!(outer.lines_of_code > inner.lines_of_code);
        assert!(inner.lines_of_code >= 3);
        assert!(inner.lines_of_code <= 5);
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
        assert_eq!(function.cognitive_complexity, 1);
        assert_eq!(function.nesting_depth, 1);
        assert!(function.lines_of_code >= 3);
        assert!(function.lines_of_code <= 5);
    }
}
