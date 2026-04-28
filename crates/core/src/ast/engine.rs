use crate::metrics::FunctionMetrics;
use once_cell::sync::Lazy;
use tree_sitter::{Node, Query, QueryCursor};

static FUNCTION_QUERY: Lazy<Query> = Lazy::new(|| {
    let query_str = r#"
        [
            (function_declaration) @func
            (arrow_function) @func
            (method_definition) @func
            (function_expression) @func
        ]
    "#;
    let language = tree_sitter_typescript::language_typescript();
    Query::new(language, query_str).expect("Valid function query")
});

static COMPLEXITY_QUERY: Lazy<Query> = Lazy::new(|| {
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
    Query::new(language, query_str).expect("Valid complexity query")
});

pub struct ComplexityEngine<'a> {
    source: &'a str,
    file_path: &'a str,
}

impl<'a> ComplexityEngine<'a> {
    pub fn new(source: &'a str, file_path: &'a str) -> Self {
        Self { source, file_path }
    }

    pub fn analyze(&self, root_node: Node) -> Vec<FunctionMetrics> {
        let mut functions = Vec::new();
        let mut cursor = QueryCursor::new();

        let matches = cursor.matches(&FUNCTION_QUERY, root_node, self.source.as_bytes());

        for m in matches {
            for capture in m.captures {
                if let Some(metrics) = self.extract_metrics(capture.node) {
                    functions.push(metrics);
                }
            }
        }

        functions
    }

    fn extract_metrics(&self, node: Node) -> Option<FunctionMetrics> {
        let name = self.extract_name(node).to_string();
        let line = node.start_position().row as u32 + 1;
        let cyclomatic_complexity = self.calculate_cyclomatic_complexity(node);
        let (cognitive_complexity, nesting_depth) = self.calculate_cognitive_and_nesting(node);
        let lines_of_code = (node.end_position().row - node.start_position().row + 1) as u32;

        Some(FunctionMetrics {
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
        })
    }

    fn extract_name(&self, node: Node) -> &'a str {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" || child.kind() == "property_identifier" {
                return child
                    .utf8_text(self.source.as_bytes())
                    .unwrap_or("<unknown>");
            }
        }

        if let Some(parent) = node.parent() {
            if parent.kind() == "variable_declarator" || parent.kind() == "public_field_definition"
            {
                let mut p_cursor = parent.walk();
                for child in parent.children(&mut p_cursor) {
                    if child.kind() == "identifier" || child.kind() == "property_identifier" {
                        return child
                            .utf8_text(self.source.as_bytes())
                            .unwrap_or("<unknown>");
                    }
                }
            }
            if parent.kind() == "assignment_expression" {
                if let Some(left) = parent.child_by_field_name("left") {
                    return left
                        .utf8_text(self.source.as_bytes())
                        .unwrap_or("<unknown>");
                }
            }
        }

        "<anonymous>"
    }

    fn calculate_cyclomatic_complexity(&self, node: Node) -> u32 {
        let mut cursor = QueryCursor::new();
        let matches = cursor.matches(&COMPLEXITY_QUERY, node, self.source.as_bytes());
        1 + matches.count() as u32
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
        let mut increment = 0;

        match kind {
            "if_statement" | "for_statement" | "while_statement" | "do_statement"
            | "switch_statement" | "catch_clause" | "ternary_expression" => {
                let is_else_if = kind == "if_statement"
                    && node.parent().map_or(false, |p| p.kind() == "else_clause");

                if is_else_if {
                    increment = 1;
                } else {
                    increment = 1 + depth;
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

        *cognitive += increment;
        if new_depth > *max_depth {
            *max_depth = new_depth;
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_cognitive(child, new_depth, cognitive, max_depth, None);
        }
    }
}
