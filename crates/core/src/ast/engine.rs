use crate::metrics::FunctionMetrics;
use anyhow::Result;
use tree_sitter::Node;
use xxhash_rust::xxh3::xxh3_128;

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

enum TraversalEvent<'tree> {
    Enter(Node<'tree>, u32, Option<&'static str>),
    ExitFunction(Node<'tree>),
}

struct BodyQuality {
    body_hash: String,
    executable_statements: u32,
    is_hollow: bool,
    hollow_kind: String,
    comment_ratio: f64,
    placeholder_count: usize,
    has_docstring: bool,
    documentation_quality: String,
    identifier_verbosity: f64,
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
        let mut function_stack = Vec::new();
        let mut functions = Vec::new();
        let mut traversal_stack = vec![TraversalEvent::Enter(root_node, 0, None)];

        while let Some(event) = traversal_stack.pop() {
            match event {
                TraversalEvent::Enter(node, depth, last_op) => self.enter_node(
                    node,
                    &mut function_stack,
                    &mut functions,
                    &mut traversal_stack,
                    depth,
                    last_op,
                ),
                TraversalEvent::ExitFunction(node) => {
                    self.finish_function(node, &mut function_stack, &mut functions);
                }
            }
        }

        Ok(functions.into_iter().flatten().collect())
    }

    fn enter_node<'tree>(
        &self,
        node: Node<'tree>,
        stack: &mut Vec<FunctionState>,
        functions: &mut Vec<Option<FunctionMetrics>>,
        traversal_stack: &mut Vec<TraversalEvent<'tree>>,
        depth: u32,
        last_op: Option<&'static str>,
    ) {
        let entered_function = if self.support.is_function(node) {
            let name = self.support.extract_name(node, self.source);
            let start_line = node.start_position().row as u32 + 1;
            let output_index = functions.len();
            functions.push(None);
            stack.push(FunctionState {
                id: self.function_id(node, &name),
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

                        if entered_function {
                            traversal_stack.push(TraversalEvent::ExitFunction(node));
                        }
                        self.push_children(node, traversal_stack, child_depth, Some(op));
                        return;
                    }
                }
                _ => {}
            }

            if child_depth > function.max_nesting {
                function.max_nesting = child_depth;
            }
        }

        if entered_function {
            traversal_stack.push(TraversalEvent::ExitFunction(node));
        }
        self.push_children(node, traversal_stack, child_depth, None);
    }

    fn push_children<'tree>(
        &self,
        node: Node<'tree>,
        traversal_stack: &mut Vec<TraversalEvent<'tree>>,
        child_depth: u32,
        last_op: Option<&'static str>,
    ) {
        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        for child in children.into_iter().rev() {
            traversal_stack.push(TraversalEvent::Enter(child, child_depth, last_op));
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
        let end_line = node.end_position().row as u32 + 1;
        let lines_of_code = end_line - function.start_line + 1;
        let quality = self.analyze_body_quality(node, function.cognitive);
        functions[function.output_index] = Some(FunctionMetrics {
            id: function.id,
            name: function.name,
            file: self.file_path.to_string(),
            line: function.start_line,
            end_line,
            body_hash: quality.body_hash,
            cyclomatic_complexity: function.cyclomatic,
            cognitive_complexity: function.cognitive,
            nesting_depth: function.max_nesting,
            lines_of_code,
            executable_statements: quality.executable_statements,
            is_hollow: quality.is_hollow,
            hollow_kind: quality.hollow_kind,
            comment_ratio: quality.comment_ratio,
            placeholder_count: quality.placeholder_count,
            has_docstring: quality.has_docstring,
            documentation_quality: quality.documentation_quality,
            identifier_verbosity: quality.identifier_verbosity,
            times_modified: 0,
            bug_fix_commits: 0,
            authors_count: 0,
            authors: None,
            churn_score: 0.0,
            normalized: None,
            risk: None,
            percentile: None,
        });
    }

    fn function_id(&self, node: Node, name: &str) -> String {
        let signature = function_signature_text(node, self.source);
        format!(
            "{}:{}:{}",
            self.file_path,
            name,
            stable_hash_hex(signature.as_bytes())
        )
    }

    fn analyze_body_quality(&self, node: Node, cognitive_complexity: u32) -> BodyQuality {
        let text = node.utf8_text(self.source.as_bytes()).unwrap_or_default();
        let body_hash = stable_hash_hex(text.as_bytes());
        let comment_lines = text
            .lines()
            .filter(|line| is_comment_line(line.trim()))
            .count();
        let total_lines = text.lines().filter(|line| !line.trim().is_empty()).count();
        let comment_ratio = if total_lines == 0 {
            0.0
        } else {
            comment_lines as f64 / total_lines as f64
        };
        let placeholder_count = count_placeholders(text);
        let docstring_chars = leading_docstring_chars(node, self.source);
        let has_docstring = docstring_chars > 0;
        let documentation_quality = documentation_quality(docstring_chars, cognitive_complexity);

        let mut executable_statements = 0;
        let mut identifier_total = 0usize;
        let mut identifier_count = 0usize;
        let mut stack = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }

        while let Some(current) = stack.pop() {
            if current != node && self.support.is_function(current) {
                continue;
            }

            let kind = current.kind();
            if is_executable_node(kind) {
                executable_statements += 1;
            }
            if is_identifier_node(kind) {
                if let Ok(identifier) = current.utf8_text(self.source.as_bytes()) {
                    identifier_total += identifier.len();
                    identifier_count += 1;
                }
            }

            let mut cursor = current.walk();
            for child in current.children(&mut cursor) {
                stack.push(child);
            }
        }

        let identifier_verbosity = if identifier_count == 0 {
            0.0
        } else {
            identifier_total as f64 / identifier_count as f64
        };
        let is_hollow = executable_statements == 0;
        let hollow_kind = if is_hollow {
            if comment_lines > 0 {
                "comment_only".to_string()
            } else {
                "empty".to_string()
            }
        } else {
            "none".to_string()
        };

        BodyQuality {
            body_hash,
            executable_statements,
            is_hollow,
            hollow_kind,
            comment_ratio,
            placeholder_count,
            has_docstring,
            documentation_quality,
            identifier_verbosity,
        }
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

fn function_signature_text(node: Node, source: &str) -> String {
    let start = node.start_byte();
    let end = function_body_start(node).unwrap_or_else(|| node.end_byte());
    source
        .get(start..end)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn function_body_start(node: Node) -> Option<usize> {
    let mut cursor = node.walk();
    let body_start = node
        .children(&mut cursor)
        .find(|child| is_body_node(child.kind()))
        .map(|child| child.start_byte());
    body_start
}

fn stable_hash_hex(bytes: &[u8]) -> String {
    format!("{:032x}", xxh3_128(bytes))
}

fn is_body_node(kind: &str) -> bool {
    matches!(
        kind,
        "statement_block" | "block" | "compound_statement" | "declaration_list"
    )
}

fn is_comment_line(line: &str) -> bool {
    line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with('*')
        || line.starts_with("*/")
        || line.starts_with("///")
        || line.starts_with("//!")
}

fn count_placeholders(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    [
        "todo",
        "fixme",
        "placeholder",
        "insert code here",
        "not implemented",
        "arg1",
        "arg2",
        "foo",
        "bar",
        "baz",
    ]
    .iter()
    .map(|needle| lower.matches(needle).count())
    .sum()
}

fn leading_docstring_chars(node: Node, source: &str) -> usize {
    let Some(previous) = node.prev_named_sibling() else {
        return 0;
    };
    if previous.end_position().row + 1 < node.start_position().row {
        return 0;
    }
    if previous.kind() != "comment" {
        return 0;
    }
    previous
        .utf8_text(source.as_bytes())
        .ok()
        .map(str::trim)
        .filter(|text| is_doc_comment(text))
        .map(|text| text.len())
        .unwrap_or(0)
}

fn is_doc_comment(text: &str) -> bool {
    text.starts_with("///")
        || text.starts_with("/**")
        || text.starts_with("//!")
        || text.starts_with("/*!")
}

fn documentation_quality(docstring_chars: usize, cognitive_complexity: u32) -> String {
    if docstring_chars == 0 {
        return "missing".to_string();
    }

    if cognitive_complexity >= 8 && docstring_chars < 80 {
        "sparse".to_string()
    } else {
        "adequate".to_string()
    }
}

fn is_executable_node(kind: &str) -> bool {
    matches!(
        kind,
        "return_statement"
            | "expression_statement"
            | "lexical_declaration"
            | "variable_declaration"
            | "let_declaration"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "call_expression"
            | "await_expression"
            | "yield_expression"
            | "throw_statement"
            | "break_statement"
            | "continue_statement"
            | "for_statement"
            | "while_statement"
            | "do_statement"
            | "if_statement"
            | "switch_statement"
            | "for_expression"
            | "while_expression"
            | "loop_expression"
            | "if_expression"
            | "match_expression"
            | "macro_invocation"
            | "declaration"
    )
}

fn is_identifier_node(kind: &str) -> bool {
    matches!(
        kind,
        "identifier" | "property_identifier" | "field_identifier" | "type_identifier"
    )
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
