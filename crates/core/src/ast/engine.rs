use crate::metrics::{ChurnDetails, CouplingMetrics, FunctionMetrics, ReachabilityMetrics};
use anyhow::Result;
use tree_sitter::Node;
use xxhash_rust::xxh3::xxh3_128;

use super::{CognitiveComplexity, LanguageSupport};

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

struct NodeState {
    entered_function: bool,
    child_depth: u32,
    logical_operator: Option<&'static str>,
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

        self.drain_traversal_stack(&mut function_stack, &mut functions, &mut traversal_stack);

        Ok(functions.into_iter().flatten().collect())
    }

    fn drain_traversal_stack<'tree>(
        &self,
        function_stack: &mut Vec<FunctionState>,
        functions: &mut Vec<Option<FunctionMetrics>>,
        traversal_stack: &mut Vec<TraversalEvent<'tree>>,
    ) {
        while let Some(event) = traversal_stack.pop() {
            self.handle_traversal_event(event, function_stack, functions, traversal_stack);
        }
    }

    fn handle_traversal_event<'tree>(
        &self,
        event: TraversalEvent<'tree>,
        function_stack: &mut Vec<FunctionState>,
        functions: &mut Vec<Option<FunctionMetrics>>,
        traversal_stack: &mut Vec<TraversalEvent<'tree>>,
    ) {
        self.dispatch_traversal_event(event, function_stack, functions, traversal_stack);
    }

    fn dispatch_traversal_event<'tree>(
        &self,
        event: TraversalEvent<'tree>,
        function_stack: &mut Vec<FunctionState>,
        functions: &mut Vec<Option<FunctionMetrics>>,
        traversal_stack: &mut Vec<TraversalEvent<'tree>>,
    ) {
        match event {
            TraversalEvent::Enter(node, depth, last_op) => self.enter_node(
                node,
                function_stack,
                functions,
                traversal_stack,
                depth,
                last_op,
            ),
            TraversalEvent::ExitFunction(node) => {
                self.finish_function(node, function_stack, functions);
            }
        }
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
        let entered_function = self.enter_function_if_needed(node, stack, functions);
        let state = self.process_complexity_node(node, stack, depth, entered_function, last_op);
        if state.entered_function {
            traversal_stack.push(TraversalEvent::ExitFunction(node));
        }
        self.push_children(
            node,
            traversal_stack,
            state.child_depth,
            state.logical_operator,
        );
    }

    fn enter_function_if_needed(
        &self,
        node: Node,
        stack: &mut Vec<FunctionState>,
        functions: &mut Vec<Option<FunctionMetrics>>,
    ) -> bool {
        if !self.support.is_function(node) {
            return false;
        }

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
    }

    fn process_complexity_node(
        &self,
        node: Node,
        stack: &mut [FunctionState],
        depth: u32,
        entered_function: bool,
        last_op: Option<&'static str>,
    ) -> NodeState {
        let active_depth = if entered_function { 0 } else { depth };
        let mut state = NodeState {
            entered_function,
            child_depth: active_depth,
            logical_operator: None,
        };

        if let Some(function) = stack.last_mut() {
            self.increment_cyclomatic(node, function, entered_function);
            self.increment_cognitive(node, function, active_depth, last_op, &mut state);
            function.max_nesting = function.max_nesting.max(state.child_depth);
        }

        state
    }

    fn increment_cyclomatic(
        &self,
        node: Node,
        function: &mut FunctionState,
        entered_function: bool,
    ) {
        if !entered_function && self.support.is_complexity_increment(node) {
            function.cyclomatic += 1;
        }
    }

    fn increment_cognitive(
        &self,
        node: Node,
        function: &mut FunctionState,
        active_depth: u32,
        last_op: Option<&'static str>,
        state: &mut NodeState,
    ) {
        match self.support.cognitive_complexity(node) {
            CognitiveComplexity::None => {}
            CognitiveComplexity::Logical => {
                increment_logical_cognitive(node, function, last_op, state);
            }
            CognitiveComplexity::Structural => {
                increment_structural_cognitive(node, function, active_depth);
            }
            CognitiveComplexity::Nesting => {
                increment_nesting_cognitive(node, function, active_depth, state);
            }
        }
    }

    fn push_children<'tree>(
        &self,
        node: Node<'tree>,
        traversal_stack: &mut Vec<TraversalEvent<'tree>>,
        child_depth: u32,
        last_op: Option<&'static str>,
    ) {
        for index in (0..node.child_count()).rev() {
            if let Some(child) = node.child(index) {
                traversal_stack.push(TraversalEvent::Enter(child, child_depth, last_op));
            }
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
        let callees = collect_call_names(node, self.source, self.support);
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
            churn: ChurnDetails::default(),
            churn_score: 0.0,
            coverage: None,
            coupling: CouplingMetrics {
                callees,
                ..CouplingMetrics::default()
            },
            reachability: ReachabilityMetrics {
                is_reachable: false,
                kind: initial_reachability_kind(node, self.source),
            },
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
        let (comment_lines, comment_ratio) = self.body_line_quality(text);
        let placeholder_count = count_placeholders(text);
        let docstring_chars = leading_docstring_chars(node, self.source);
        let has_docstring = docstring_chars > 0;
        let documentation_quality = documentation_quality(docstring_chars, cognitive_complexity);
        let traversal = self.body_traversal_quality(node);

        let is_hollow = traversal.executable_statements == 0;
        let hollow_kind = self.hollow_kind(is_hollow, comment_lines);

        BodyQuality {
            body_hash,
            executable_statements: traversal.executable_statements,
            is_hollow,
            hollow_kind,
            comment_ratio,
            placeholder_count,
            has_docstring,
            documentation_quality,
            identifier_verbosity: traversal.identifier_verbosity,
        }
    }

    fn body_line_quality(&self, text: &str) -> (usize, f64) {
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
        (comment_lines, comment_ratio)
    }

    fn body_traversal_quality(&self, node: Node) -> BodyTraversalQuality {
        let mut executable_statements = 0;
        let mut identifier_total = 0usize;
        let mut identifier_count = 0usize;
        let mut stack = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }

        while let Some(current) = stack.pop() {
            if self.should_skip_body_node(current, node) {
                continue;
            }

            let kind = current.kind();
            if is_executable_node(kind) {
                executable_statements += 1;
            }
            self.add_identifier_quality(current, &mut identifier_total, &mut identifier_count);

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

        BodyTraversalQuality {
            executable_statements,
            identifier_verbosity,
        }
    }

    fn should_skip_body_node(&self, current: Node, root: Node) -> bool {
        current != root && self.support.is_function(current)
    }

    fn add_identifier_quality(
        &self,
        node: Node,
        identifier_total: &mut usize,
        identifier_count: &mut usize,
    ) {
        if !is_identifier_node(node.kind()) {
            return;
        }

        if let Ok(identifier) = node.utf8_text(self.source.as_bytes()) {
            *identifier_total += identifier.len();
            *identifier_count += 1;
        }
    }

    fn hollow_kind(&self, is_hollow: bool, comment_lines: usize) -> String {
        if is_hollow {
            if comment_lines > 0 {
                "comment_only".to_string()
            } else {
                "empty".to_string()
            }
        } else {
            "none".to_string()
        }
    }
}

struct BodyTraversalQuality {
    executable_statements: u32,
    identifier_verbosity: f64,
}

fn increment_structural_cognitive(node: Node, function: &mut FunctionState, active_depth: u32) {
    if is_else_if(node) {
        function.cognitive += 1;
    } else {
        function.cognitive += 1 + active_depth;
    }
}

fn increment_nesting_cognitive(
    node: Node,
    function: &mut FunctionState,
    active_depth: u32,
    state: &mut NodeState,
) {
    if is_else_if(node) {
        function.cognitive += 1;
    } else {
        increment_structural_cognitive(node, function, active_depth);
        state.child_depth += 1;
    }
}

fn increment_logical_cognitive(
    node: Node,
    function: &mut FunctionState,
    last_op: Option<&'static str>,
    state: &mut NodeState,
) {
    if let Some(op) = logical_operator(node) {
        if last_op != Some(op) {
            function.cognitive += 1;
        }
        state.logical_operator = Some(op);
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

fn is_else_if(node: Node) -> bool {
    node.kind() == "if_statement"
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == "else_clause")
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
        "identifier"
            | "property_identifier"
            | "field_identifier"
            | "type_identifier"
            | "scoped_identifier"
    )
}

fn collect_call_names(node: Node, source: &str, support: &dyn LanguageSupport) -> Vec<String> {
    let mut calls = Vec::new();
    let mut stack = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        stack.push(child);
    }

    while let Some(current) = stack.pop() {
        if current != node && support.is_function(current) {
            continue;
        }
        if is_identifier_node(current.kind()) {
            if let Ok(text) = current.utf8_text(source.as_bytes()) {
                let name = normalize_call_name(text);
                if !name.is_empty() {
                    calls.push(name);
                }
            }
        }
        if is_call_node(current.kind()) {
            if let Some(name) = call_name(current, source) {
                calls.push(name);
            }
        }

        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }

    calls.sort();
    calls.dedup();
    calls
}

fn is_call_node(kind: &str) -> bool {
    matches!(kind, "call_expression" | "macro_invocation")
}

fn call_name(node: Node, source: &str) -> Option<String> {
    let target = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("name"))
        .or_else(|| node.named_child(0))?;
    target
        .utf8_text(source.as_bytes())
        .ok()
        .map(normalize_call_name)
        .filter(|name| !name.is_empty())
}

fn normalize_call_name(text: &str) -> String {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .rfind(|part| !part.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod engine_tests {
    use super::normalize_call_name;

    #[test]
    fn test_normalize_call_name() {
        assert_eq!(normalize_call_name("Self::analyze"), "analyze");
        assert_eq!(normalize_call_name("git::commit::push"), "push");
        assert_eq!(normalize_call_name("this.process"), "process");
        assert_eq!(normalize_call_name("base_function"), "base_function");
        assert_eq!(normalize_call_name("Option::<T>::unwrap"), "unwrap");
        assert_eq!(normalize_call_name("ptr->method"), "method");
    }
}

fn initial_reachability_kind(node: Node, source: &str) -> String {
    if is_test_entry_point(node, source) {
        "test_entry".to_string()
    } else if is_exported_or_public(node) || is_trait_impl_method(node, source) {
        "unreachable_export".to_string()
    } else {
        "unreachable_private".to_string()
    }
}

fn is_exported_or_public(node: Node) -> bool {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if matches!(
            candidate.kind(),
            "export_statement" | "public_field_definition" | "visibility_modifier"
        ) {
            return true;
        }
        if has_child_kind(candidate, "visibility_modifier") {
            return true;
        }
        if let Some(previous) = candidate.prev_sibling() {
            if previous.kind() == "pub" || previous.kind() == "export" {
                return true;
            }
        }
        current = candidate.parent();
    }
    false
}

fn is_test_entry_point(node: Node, source: &str) -> bool {
    attribute_items(node)
        .into_iter()
        .any(|attribute| is_rust_test_attribute(attribute, source))
}

fn attribute_items(node: Node) -> Vec<Node> {
    let mut attributes = direct_attribute_items(node);
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        if sibling.kind() != "attribute_item" {
            break;
        }
        attributes.push(sibling);
        previous = sibling.prev_named_sibling();
    }
    attributes
}

fn direct_attribute_items(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| child.kind() == "attribute_item")
        .collect()
}

fn is_rust_test_attribute(node: Node, source: &str) -> bool {
    let Ok(text) = node.utf8_text(source.as_bytes()) else {
        return false;
    };
    let attribute = text.trim();
    attribute == "#[test]"
        || attribute.ends_with("::test]")
        || attribute.contains("::test(")
        || attribute.starts_with("#[test(")
}

fn is_trait_impl_method(node: Node, source: &str) -> bool {
    let mut current = node.parent();
    while let Some(candidate) = current {
        if candidate.kind() == "impl_item" {
            return candidate.child_by_field_name("trait").is_some()
                || impl_header_declares_trait(candidate, source);
        }
        current = candidate.parent();
    }
    false
}

fn impl_header_declares_trait(node: Node, source: &str) -> bool {
    let body_start = node
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte());
    source
        .get(node.start_byte()..body_start)
        .is_some_and(|header| header.split_whitespace().any(|part| part == "for"))
}

fn has_child_kind(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    let has_child = node.children(&mut cursor).any(|child| child.kind() == kind);
    has_child
}

#[cfg(test)]
mod tests {
    use super::super::parser::AstParser;
    use crate::ast::rust::RustSupport;
    use crate::ast::typescript::TypeScriptSupport;
    use crate::metrics::FunctionMetrics;

    fn analyze_typescript(source: &str) -> Vec<FunctionMetrics> {
        let support = TypeScriptSupport::new(false);
        AstParser::analyze_source(source, "file.ts", &support).expect("source should parse")
    }

    fn analyze_rust(source: &str) -> Vec<FunctionMetrics> {
        let support = RustSupport;
        AstParser::analyze_source(source, "file.rs", &support).expect("source should parse")
    }

    fn find<'a>(functions: &'a [FunctionMetrics], name: &str) -> &'a FunctionMetrics {
        functions
            .iter()
            .find(|function| function.name == name)
            .expect("function should exist")
    }

    #[test]
    fn normalize_call_name_strips_language_prefixes() {
        assert_eq!(super::normalize_call_name("Self::analyze"), "analyze");
        assert_eq!(super::normalize_call_name("git::commit"), "commit");
        assert_eq!(super::normalize_call_name("this.process"), "process");
        assert_eq!(super::normalize_call_name("base_function"), "base_function");
    }

    #[test]
    fn simple_function_counts_branch_complexity() {
        let functions = analyze_typescript(
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
        let functions = analyze_typescript(
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
        let functions = analyze_typescript(
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

    #[test]
    fn rust_flat_match_counts_once() {
        let functions = analyze_rust(
            r#"
            fn a(x: u8) -> u8 {
                match x {
                    1 => 1,
                    2 => 2,
                    _ => 0,
                }
            }
            "#,
        );

        let function = find(&functions, "a");

        assert_eq!(function.cognitive_complexity, 1);
    }

    #[test]
    fn rust_match_arm_does_not_add_nesting_penalty_to_nested_logic() {
        let functions = analyze_rust(
            r#"
            fn a(x: u8, cond: bool) {
                match x {
                    1 => {
                        if cond {
                            foo();
                        }
                    }
                    _ => {}
                }
            }
            "#,
        );

        let function = find(&functions, "a");

        assert_eq!(function.cognitive_complexity, 2);
    }

    #[test]
    fn rust_match_nested_inside_if_pays_outer_nesting_penalty() {
        let functions = analyze_rust(
            r#"
            fn a(x: u8, cond: bool) {
                if cond {
                    match x {
                        1 => {}
                        _ => {}
                    }
                }
            }
            "#,
        );

        let function = find(&functions, "a");

        assert_eq!(function.cognitive_complexity, 3);
    }

    #[test]
    fn rust_public_function_is_not_classified_as_private_dead_code() {
        let functions = analyze_rust(
            r#"
            pub fn get_all_file_metrics() {}

            impl GitAnalyzer {
                pub fn analyze() {}
            }
            "#,
        );

        let function = find(&functions, "get_all_file_metrics");
        let method = find(&functions, "analyze");

        assert_eq!(function.reachability.kind, "unreachable_export");
        assert_eq!(method.reachability.kind, "unreachable_export");
    }

    #[test]
    fn rust_test_attribute_is_classified_as_test_entry() {
        let functions = analyze_rust(
            r#"
            #[test]
            fn parses_report() {}

            #[tokio::test]
            async fn loads_async_report() {}

            #[cfg(test)]
            mod tests {
                #[test]
                fn nested_test() {}
            }
            "#,
        );

        let sync_test = find(&functions, "parses_report");
        let async_test = find(&functions, "loads_async_report");
        let nested_test = find(&functions, "nested_test");

        assert_eq!(sync_test.reachability.kind, "test_entry");
        assert_eq!(async_test.reachability.kind, "test_entry");
        assert_eq!(nested_test.reachability.kind, "test_entry");
    }

    #[test]
    fn rust_trait_impl_method_is_not_classified_as_private_dead_code() {
        let functions = analyze_rust(
            r#"
            impl std::fmt::Display for Report {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "report")
                }
            }
            "#,
        );

        let function = find(&functions, "fmt");

        assert_eq!(function.reachability.kind, "unreachable_export");
    }

    #[test]
    fn rust_callback_identifier_is_collected_as_callee() {
        let functions = analyze_rust(
            r#"
            pub fn caller(value: Result<(), ()>) -> Result<(), String> {
                value.map_err(callback)
            }

            fn callback(_: ()) -> String {
                "error".to_string()
            }
            "#,
        );

        let caller = find(&functions, "caller");

        assert!(caller
            .coupling
            .callees
            .iter()
            .any(|name| name == "callback"));
    }
}
