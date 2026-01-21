use std::collections::{HashMap, HashSet};

use bytes::Bytes;
use lazy_static::lazy_static;
use notebook_meta::{Function, FunctionArg, MetaVersion, NotebookMeta};
use tree_sitter::{Node, Parser, Tree};

const NAME_APP_CLASS: &str = "App";
const NAME_APP_FUNCTION: &str = "function";
const MODULE_MARIMO: &str = "marimo";

const FIELD_ALIAS: &str = "alias";
const FIELD_ATTRIBUTE: &str = "attribute";
const FIELD_BODY: &str = "body";
const FIELD_DEFINITION: &str = "definition";
const FIELD_FUNCTION: &str = "function";
const FIELD_LEFT: &str = "left";
const FIELD_NAME: &str = "name";
const FIELD_MODULE_NAME: &str = "module_name";
const FIELD_OBJECT: &str = "object";
const FIELD_PARAMETERS: &str = "parameters";
const FIELD_RETURN_TYPE: &str = "return_type";
const FIELD_RIGHT: &str = "right";
const FIELD_TYPE: &str = "type";
const FIELD_VALUE: &str = "value";

const KIND_ASSIGNMENT: &str = "assignment";
const KIND_AUGMENTED_ASSIGNMENT: &str = "augmented_assignment";
const KIND_ASYNC: &str = "async";
const KIND_BLOCK: &str = "block";
const KIND_CALL: &str = "call";
const KIND_CONCATENATED_STRING: &str = "concatenated_string";
const KIND_DECORATED_DEFINITION: &str = "decorated_definition";
const KIND_DECORATOR: &str = "decorator";
const KIND_DEFAULT_PARAMETER: &str = "default_parameter";
const KIND_DICTIONARY_SPLAT_PATTERN: &str = "dictionary_splat_pattern";
const KIND_EXPRESSION_STATEMENT: &str = "expression_statement";
const KIND_IMPORT_STATEMENT: &str = "import_statement";
const KIND_IMPORT_FROM_STATEMENT: &str = "import_from_statement";
const KIND_KEYWORD_SEPARATOR: &str = "keyword_separator";
const KIND_LIST_SPLAT_PATTERN: &str = "list_splat_pattern";
const KIND_POSITIONAL_SEPARATOR: &str = "positional_separator";
const KIND_STRING: &str = "string";
const KIND_TYPED_DEFAULT_PARAMETER: &str = "typed_default_parameter";
const KIND_TYPED_PARAMETER: &str = "typed_parameter";
const KIND_FUNCTION_DEFINITION: &str = "function_definition";
const KIND_CLASS_DEFINITION: &str = "class_definition";
const KIND_LAMBDA: &str = "lambda";
const KIND_IDENTIFIER: &str = "identifier";
const KIND_ATTRIBUTE: &str = "attribute";
const KIND_SUBSCRIPT: &str = "subscript";
const KIND_DOTTED_NAME: &str = "dotted_name";
const KIND_ALIASED_IMPORT: &str = "aliased_import";
const KIND_RELATIVE_IMPORT: &str = "relative_import";
const KIND_EXPRESSION_LIST: &str = "expression_list";
const KIND_PARENTHESIZED_EXPRESSION: &str = "parenthesized_expression";

struct NodeKinds {
    assignment: u16,
    augmented_assignment: u16,
    async_token: u16,
    block: u16,
    call: u16,
    concatenated_string: u16,
    decorated_definition: u16,
    decorator: u16,
    default_parameter: u16,
    dictionary_splat_pattern: u16,
    expression_statement: u16,
    import_statement: u16,
    import_from_statement: u16,
    keyword_separator: u16,
    list_splat_pattern: u16,
    positional_separator: u16,
    string: u16,
    typed_default_parameter: u16,
    typed_parameter: u16,
    function_definition: u16,
    class_definition: u16,
    lambda: u16,
    identifier: u16,
    attribute: u16,
    subscript: u16,
    dotted_name: u16,
    aliased_import: u16,
    relative_import: u16,
    expression_list: u16,
    parenthesized_expression: u16,
}

impl NodeKinds {
    fn new(language: &tree_sitter::Language) -> Self {
        Self {
            assignment: language.id_for_node_kind(KIND_ASSIGNMENT, true),
            augmented_assignment: language.id_for_node_kind(KIND_AUGMENTED_ASSIGNMENT, true),
            async_token: language.id_for_node_kind(KIND_ASYNC, false),
            block: language.id_for_node_kind(KIND_BLOCK, true),
            call: language.id_for_node_kind(KIND_CALL, true),
            concatenated_string: language.id_for_node_kind(KIND_CONCATENATED_STRING, true),
            decorated_definition: language.id_for_node_kind(KIND_DECORATED_DEFINITION, true),
            decorator: language.id_for_node_kind(KIND_DECORATOR, true),
            default_parameter: language.id_for_node_kind(KIND_DEFAULT_PARAMETER, true),
            dictionary_splat_pattern: language
                .id_for_node_kind(KIND_DICTIONARY_SPLAT_PATTERN, true),
            expression_statement: language.id_for_node_kind(KIND_EXPRESSION_STATEMENT, true),
            import_statement: language.id_for_node_kind(KIND_IMPORT_STATEMENT, true),
            import_from_statement: language.id_for_node_kind(KIND_IMPORT_FROM_STATEMENT, true),
            keyword_separator: language.id_for_node_kind(KIND_KEYWORD_SEPARATOR, true),
            list_splat_pattern: language.id_for_node_kind(KIND_LIST_SPLAT_PATTERN, true),
            positional_separator: language.id_for_node_kind(KIND_POSITIONAL_SEPARATOR, true),
            string: language.id_for_node_kind(KIND_STRING, true),
            typed_default_parameter: language.id_for_node_kind(KIND_TYPED_DEFAULT_PARAMETER, true),
            typed_parameter: language.id_for_node_kind(KIND_TYPED_PARAMETER, true),
            function_definition: language.id_for_node_kind(KIND_FUNCTION_DEFINITION, true),
            class_definition: language.id_for_node_kind(KIND_CLASS_DEFINITION, true),
            lambda: language.id_for_node_kind(KIND_LAMBDA, true),
            identifier: language.id_for_node_kind(KIND_IDENTIFIER, true),
            attribute: language.id_for_node_kind(KIND_ATTRIBUTE, true),
            subscript: language.id_for_node_kind(KIND_SUBSCRIPT, true),
            dotted_name: language.id_for_node_kind(KIND_DOTTED_NAME, true),
            aliased_import: language.id_for_node_kind(KIND_ALIASED_IMPORT, true),
            relative_import: language.id_for_node_kind(KIND_RELATIVE_IMPORT, true),
            expression_list: language.id_for_node_kind(KIND_EXPRESSION_LIST, true),
            parenthesized_expression: language
                .id_for_node_kind(KIND_PARENTHESIZED_EXPRESSION, true),
        }
    }
}

lazy_static! {
    static ref NODE_KINDS: NodeKinds = {
        let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        NodeKinds::new(&language)
    };
}

const MAX_FUNCTION_CALL_DEPTH: usize = 6;

#[derive(Default)]
struct ImportInfo {
    module_aliases: HashSet<String>,
    app_aliases: HashSet<String>,
}

impl ImportInfo {
    fn has_imports(&self) -> bool {
        !self.module_aliases.is_empty() || !self.app_aliases.is_empty()
    }
}

struct ModuleInfo<'a> {
    imports: ImportInfo,
    functions: HashMap<String, Node<'a>>,
    assignments: Vec<Node<'a>>,
}

pub struct Notebook {
    tree: Tree,
    source: Bytes,
    app_names: HashSet<String>,
}

impl Notebook {
    pub fn meta(&self) -> NotebookMeta {
        let funcs =
            collect_marimo_functions(self.tree.root_node(), self.source.as_ref(), &self.app_names);
        NotebookMeta {
            version: MetaVersion::V1,
            funcs,
        }
    }
}

pub fn get_marimo_notebook(source: Bytes) -> Option<Notebook> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    if let Err(err) = parser.set_language(&language) {
        tracing::error!("Failed to set Tree-sitter language: {}", err);
        return None;
    }
    let tree = parser.parse(&source, None)?;
    let module_info = collect_module_info(tree.root_node(), &source);
    if !module_info.imports.has_imports() {
        return None;
    }
    let app_names = collect_app_names(&module_info, &source);
    if app_names.is_empty() {
        return None;
    }
    Some(Notebook {
        tree,
        source,
        app_names,
    })
}

fn collect_module_info<'a>(root: Node<'a>, source: &[u8]) -> ModuleInfo<'a> {
    let mut imports = ImportInfo::default();
    let mut functions = HashMap::new();
    let mut assignments = Vec::new();

    collect_module_scope(root, source, &mut imports, &mut functions, &mut assignments);

    ModuleInfo {
        imports,
        functions,
        assignments,
    }
}

fn collect_module_scope<'a>(
    node: Node<'a>,
    source: &[u8],
    imports: &mut ImportInfo,
    functions: &mut HashMap<String, Node<'a>>,
    assignments: &mut Vec<Node<'a>>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind_id = child.kind_id();
        if kind_id == NODE_KINDS.function_definition {
            if let Some(name) = function_name(child, source) {
                functions.insert(name, child);
            }
            continue;
        }
        if kind_id == NODE_KINDS.class_definition || kind_id == NODE_KINDS.lambda {
            continue;
        }
        if kind_id == NODE_KINDS.import_statement {
            collect_import_statement(child, source, imports);
        } else if kind_id == NODE_KINDS.import_from_statement {
            collect_import_from_statement(child, source, imports);
        } else if kind_id == NODE_KINDS.assignment || kind_id == NODE_KINDS.augmented_assignment {
            assignments.push(child);
        }
        collect_module_scope(child, source, imports, functions, assignments);
    }
}

fn function_name(node: Node, source: &[u8]) -> Option<String> {
    let name_node = node.child_by_field_name(FIELD_NAME)?;
    let name = node_text(name_node, source)?;
    Some(name.to_string())
}

fn collect_import_statement(node: Node, source: &[u8], imports: &mut ImportInfo) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind_id = child.kind_id();
        if kind_id == NODE_KINDS.dotted_name {
            if dotted_name_is_marimo(child, source) {
                imports.module_aliases.insert(MODULE_MARIMO.to_string());
            }
        } else if kind_id == NODE_KINDS.aliased_import {
            let Some(name) = child.child_by_field_name(FIELD_NAME) else {
                continue;
            };
            if !dotted_name_is_marimo(name, source) {
                continue;
            }
            let alias = child
                .child_by_field_name(FIELD_ALIAS)
                .and_then(|node| node_text(node, source))
                .unwrap_or(MODULE_MARIMO);
            imports.module_aliases.insert(alias.to_string());
        }
    }
}

fn collect_import_from_statement(node: Node, source: &[u8], imports: &mut ImportInfo) {
    if !import_from_is_marimo(node, source) {
        return;
    }
    let mut cursor = node.walk();
    for name in node.children_by_field_name(FIELD_NAME, &mut cursor) {
        let kind_id = name.kind_id();
        if kind_id == NODE_KINDS.dotted_name {
            if node_text(name, source).is_some_and(|text| text == NAME_APP_CLASS) {
                imports.app_aliases.insert(NAME_APP_CLASS.to_string());
            }
        } else if kind_id == NODE_KINDS.aliased_import {
            let Some(import_name) = name.child_by_field_name(FIELD_NAME) else {
                continue;
            };
            if node_text(import_name, source).is_some_and(|text| text == NAME_APP_CLASS) {
                let alias = name
                    .child_by_field_name(FIELD_ALIAS)
                    .and_then(|node| node_text(node, source))
                    .unwrap_or(NAME_APP_CLASS);
                imports.app_aliases.insert(alias.to_string());
            }
        }
    }
}

fn import_from_is_marimo(node: Node, source: &[u8]) -> bool {
    let Some(module_name) = node.child_by_field_name(FIELD_MODULE_NAME) else {
        return false;
    };
    let kind_id = module_name.kind_id();
    if kind_id == NODE_KINDS.dotted_name {
        return dotted_name_is_marimo(module_name, source);
    }
    if kind_id == NODE_KINDS.relative_import {
        let mut cursor = module_name.walk();
        for child in module_name.children(&mut cursor) {
            if child.kind_id() == NODE_KINDS.dotted_name && dotted_name_is_marimo(child, source) {
                return true;
            }
        }
    }
    false
}

fn collect_app_names(module_info: &ModuleInfo<'_>, source: &[u8]) -> HashSet<String> {
    let mut app_names = HashSet::new();
    for assignment in &module_info.assignments {
        let mut visited = HashSet::new();
        if assignment_rhs_yields_app(*assignment, source, module_info, 0, &mut visited) {
            app_names.extend(assignment_target_identifiers(*assignment, source));
        }
    }
    app_names
}

fn assignment_target_identifiers(node: Node, source: &[u8]) -> HashSet<String> {
    let Some(left) = node.child_by_field_name(FIELD_LEFT) else {
        return HashSet::new();
    };
    let mut out = HashSet::new();
    collect_identifier_names(left, source, &mut out);
    out
}

fn collect_identifier_names(node: Node, source: &[u8], out: &mut HashSet<String>) {
    let kind_id = node.kind_id();
    if kind_id == NODE_KINDS.identifier {
        if let Some(name) = node_text(node, source) {
            out.insert(name.to_string());
        }
        return;
    }
    if kind_id == NODE_KINDS.attribute || kind_id == NODE_KINDS.subscript {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_names(child, source, out);
    }
}

fn assignment_rhs_yields_app(
    node: Node,
    source: &[u8],
    module_info: &ModuleInfo<'_>,
    depth: usize,
    visited: &mut HashSet<String>,
) -> bool {
    let Some(right) = node.child_by_field_name(FIELD_RIGHT) else {
        return false;
    };
    expression_yields_app(right, source, module_info, depth, visited)
}

fn expression_yields_app(
    node: Node,
    source: &[u8],
    module_info: &ModuleInfo<'_>,
    depth: usize,
    visited: &mut HashSet<String>,
) -> bool {
    let kind_id = node.kind_id();
    if kind_id == NODE_KINDS.call {
        return call_yields_app(node, source, module_info, depth, visited);
    }
    if kind_id == NODE_KINDS.assignment || kind_id == NODE_KINDS.augmented_assignment {
        return assignment_rhs_yields_app(node, source, module_info, depth, visited);
    }
    if kind_id == NODE_KINDS.expression_list {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if expression_yields_app(child, source, module_info, depth, visited) {
                return true;
            }
        }
        return false;
    }
    if kind_id == NODE_KINDS.parenthesized_expression {
        let mut cursor = node.walk();
        return node
            .named_children(&mut cursor)
            .any(|child| expression_yields_app(child, source, module_info, depth, visited));
    }
    false
}

fn call_yields_app(
    node: Node,
    source: &[u8],
    module_info: &ModuleInfo<'_>,
    depth: usize,
    visited: &mut HashSet<String>,
) -> bool {
    if call_is_app_constructor(node, source, &module_info.imports) {
        return true;
    }
    if depth >= MAX_FUNCTION_CALL_DEPTH {
        return false;
    }
    let Some(function_node) = node.child_by_field_name(FIELD_FUNCTION) else {
        return false;
    };
    if function_node.kind_id() != NODE_KINDS.identifier {
        return false;
    }
    let Some(function_name) = node_text(function_node, source) else {
        return false;
    };
    let Some(function_def) = module_info.functions.get(function_name) else {
        return false;
    };
    function_constructs_app(
        function_name,
        *function_def,
        source,
        module_info,
        depth + 1,
        visited,
    )
}

fn call_is_app_constructor(node: Node, source: &[u8], imports: &ImportInfo) -> bool {
    let Some(function_node) = node.child_by_field_name(FIELD_FUNCTION) else {
        return false;
    };
    let kind_id = function_node.kind_id();
    if kind_id == NODE_KINDS.identifier {
        let Some(name) = node_text(function_node, source) else {
            return false;
        };
        return imports.app_aliases.contains(name);
    }
    if kind_id == NODE_KINDS.attribute {
        let Some(object) = function_node.child_by_field_name(FIELD_OBJECT) else {
            return false;
        };
        let Some(attribute) = function_node.child_by_field_name(FIELD_ATTRIBUTE) else {
            return false;
        };
        if object.kind_id() != NODE_KINDS.identifier || attribute.kind_id() != NODE_KINDS.identifier
        {
            return false;
        }
        let Some(object_name) = node_text(object, source) else {
            return false;
        };
        let Some(attribute_name) = node_text(attribute, source) else {
            return false;
        };
        return imports.module_aliases.contains(object_name) && attribute_name == NAME_APP_CLASS;
    }
    false
}

fn collect_marimo_functions(
    root: Node<'_>,
    source: &[u8],
    app_names: &HashSet<String>,
) -> Vec<Function> {
    let mut functions = Vec::new();
    collect_marimo_functions_in_scope(root, source, app_names, &mut functions);
    functions
}

fn collect_marimo_functions_in_scope(
    node: Node<'_>,
    source: &[u8],
    app_names: &HashSet<String>,
    functions: &mut Vec<Function>,
) {
    let mut cursor = node.walk();
    let mut pending_app_decorator = false;
    for child in node.children(&mut cursor) {
        let kind_id = child.kind_id();
        if kind_id == NODE_KINDS.decorator {
            if decorator_is_app_function(child, source, app_names) {
                pending_app_decorator = true;
            }
            continue;
        }
        if kind_id == NODE_KINDS.function_definition {
            if pending_app_decorator {
                functions.push(parse_function_definition(child, source));
            }
            pending_app_decorator = false;
            continue;
        }
        if kind_id == NODE_KINDS.class_definition || kind_id == NODE_KINDS.lambda {
            pending_app_decorator = false;
            continue;
        }
        if kind_id == NODE_KINDS.decorated_definition {
            if let Some(function) = marimo_function_from_decorated(child, source, app_names) {
                functions.push(function);
            }
            pending_app_decorator = false;
            continue;
        }
        pending_app_decorator = false;
        collect_marimo_functions_in_scope(child, source, app_names, functions);
    }
}

fn marimo_function_from_decorated(
    node: Node<'_>,
    source: &[u8],
    app_names: &HashSet<String>,
) -> Option<Function> {
    if !decorated_definition_has_app_function(node, source, app_names) {
        return None;
    }
    let definition = node.child_by_field_name(FIELD_DEFINITION)?;
    if definition.kind_id() != NODE_KINDS.function_definition {
        return None;
    }
    Some(parse_function_definition(definition, source))
}

fn decorated_definition_has_app_function(
    node: Node<'_>,
    source: &[u8],
    app_names: &HashSet<String>,
) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind_id() != NODE_KINDS.decorator {
            continue;
        }
        if decorator_is_app_function(child, source, app_names) {
            return true;
        }
    }
    false
}

fn decorator_is_app_function(node: Node<'_>, source: &[u8], app_names: &HashSet<String>) -> bool {
    let mut cursor = node.walk();
    let Some(expr) = node.named_children(&mut cursor).next() else {
        return false;
    };
    decorator_expression_is_app_function(expr, source, app_names)
}

fn decorator_expression_is_app_function(
    node: Node<'_>,
    source: &[u8],
    app_names: &HashSet<String>,
) -> bool {
    let kind_id = node.kind_id();
    if kind_id == NODE_KINDS.attribute {
        return attribute_is_app_function(node, source, app_names);
    }
    if kind_id == NODE_KINDS.call {
        let Some(function_node) = node.child_by_field_name(FIELD_FUNCTION) else {
            return false;
        };
        return attribute_is_app_function(function_node, source, app_names);
    }
    false
}

fn attribute_is_app_function(node: Node<'_>, source: &[u8], app_names: &HashSet<String>) -> bool {
    if node.kind_id() != NODE_KINDS.attribute {
        return false;
    }
    let Some(object) = node.child_by_field_name(FIELD_OBJECT) else {
        return false;
    };
    let Some(attribute) = node.child_by_field_name(FIELD_ATTRIBUTE) else {
        return false;
    };
    if object.kind_id() != NODE_KINDS.identifier || attribute.kind_id() != NODE_KINDS.identifier {
        return false;
    }
    let Some(object_name) = node_text(object, source) else {
        return false;
    };
    let Some(attribute_name) = node_text(attribute, source) else {
        return false;
    };
    app_names.contains(object_name) && attribute_name == NAME_APP_FUNCTION
}

fn parse_function_definition(node: Node<'_>, source: &[u8]) -> Function {
    let name = node
        .child_by_field_name(FIELD_NAME)
        .and_then(|name_node| node_text(name_node, source))
        .unwrap_or_default()
        .to_string();
    let is_async = function_is_async(node);
    let args = node
        .child_by_field_name(FIELD_PARAMETERS)
        .map(|parameters| parse_parameters(parameters, source))
        .unwrap_or_default();
    let return_ty = node
        .child_by_field_name(FIELD_RETURN_TYPE)
        .and_then(|return_type| node_text(return_type, source))
        .map(str::to_string);
    let docs = node
        .child_by_field_name(FIELD_BODY)
        .and_then(|body| extract_docstring(body, source));
    Function {
        is_async,
        name,
        args,
        return_ty,
        docs,
    }
}

fn extract_docstring(body: Node<'_>, source: &[u8]) -> Option<String> {
    if body.kind_id() != NODE_KINDS.block {
        return None;
    }
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        return docstring_from_statement(child, source);
    }
    None
}

fn docstring_from_statement(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind_id() == NODE_KINDS.expression_statement {
        return expression_statement_docstring(node, source);
    }
    let mut cursor = node.walk();
    let mut named = node.named_children(&mut cursor);
    let child = named.next()?;
    if named.next().is_some() {
        return None;
    }
    if child.kind_id() == NODE_KINDS.expression_statement {
        return expression_statement_docstring(child, source);
    }
    None
}

fn expression_statement_docstring(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let expr = node.named_children(&mut cursor).next()?;
    if expr.kind_id() == NODE_KINDS.string {
        return Some(unquote_string_literal(expr, source));
    }
    if expr.kind_id() == NODE_KINDS.concatenated_string {
        return Some(concatenated_string_literal(expr, source));
    }
    None
}

fn concatenated_string_literal(node: Node<'_>, source: &[u8]) -> String {
    let mut cursor = node.walk();
    let mut out = String::new();
    for child in node.named_children(&mut cursor) {
        if child.kind_id() == NODE_KINDS.string {
            out.push_str(&unquote_string_literal(child, source));
        }
    }
    out
}

fn unquote_string_literal(node: Node<'_>, source: &[u8]) -> String {
    let Some(text) = node_text(node, source) else {
        return String::new();
    };
    unquote_python_string(text)
}

fn unquote_python_string(text: &str) -> String {
    let Some(start) = text.find(['"', '\'']) else {
        return text.to_string();
    };
    let slice = &text[start..];
    if slice.len() < 2 {
        return slice.to_string();
    }
    let quote = slice.chars().next().unwrap();
    let triple = slice.starts_with(&format!("{quote}{quote}{quote}"));
    let (open_len, close_len) = if triple { (3, 3) } else { (1, 1) };
    if slice.len() < open_len + close_len {
        return slice.to_string();
    }
    slice[open_len..slice.len() - close_len].to_string()
}

fn function_is_async(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind_id() == NODE_KINDS.async_token {
            return true;
        }
    }
    false
}

fn parse_parameters(node: Node<'_>, source: &[u8]) -> Vec<FunctionArg> {
    let mut args = Vec::new();
    let mut keyword_only = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind_id = child.kind_id();
        if kind_id == NODE_KINDS.positional_separator {
            continue;
        }
        if kind_id == NODE_KINDS.keyword_separator {
            keyword_only = true;
            continue;
        }
        let Some(arg) = parameter_to_arg(child, source, keyword_only) else {
            continue;
        };
        if arg.name.starts_with('*') {
            keyword_only = true;
        }
        args.push(arg);
    }
    args
}

fn parameter_to_arg(node: Node<'_>, source: &[u8], is_keyword: bool) -> Option<FunctionArg> {
    let kind_id = node.kind_id();
    if kind_id == NODE_KINDS.identifier {
        let name = node_text(node, source)?.to_string();
        return Some(FunctionArg {
            name,
            is_keyword,
            ty: None,
            default_value: None,
        });
    }
    if kind_id == NODE_KINDS.default_parameter {
        let name_node = node.child_by_field_name(FIELD_NAME)?;
        let default_value = node
            .child_by_field_name(FIELD_VALUE)
            .and_then(|node| node_text(node, source))
            .map(str::to_string);
        return parameter_name_with_type(name_node, None, default_value, source, is_keyword);
    }
    if kind_id == NODE_KINDS.typed_parameter {
        let type_node = node.child_by_field_name(FIELD_TYPE);
        let type_text = type_node
            .and_then(|node| node_text(node, source))
            .map(str::to_string);
        let name_node = typed_parameter_name(node)?;
        return parameter_name_with_type(name_node, type_text, None, source, is_keyword);
    }
    if kind_id == NODE_KINDS.typed_default_parameter {
        let name_node = node.child_by_field_name(FIELD_NAME)?;
        let type_node = node.child_by_field_name(FIELD_TYPE);
        let type_text = type_node
            .and_then(|node| node_text(node, source))
            .map(str::to_string);
        let default_value = node
            .child_by_field_name(FIELD_VALUE)
            .and_then(|node| node_text(node, source))
            .map(str::to_string);
        return parameter_name_with_type(name_node, type_text, default_value, source, is_keyword);
    }
    if kind_id == NODE_KINDS.list_splat_pattern {
        let name = splat_pattern_name(node, source, "*");
        return Some(FunctionArg {
            name,
            is_keyword,
            ty: None,
            default_value: None,
        });
    }
    if kind_id == NODE_KINDS.dictionary_splat_pattern {
        let name = splat_pattern_name(node, source, "**");
        return Some(FunctionArg {
            name,
            is_keyword: true,
            ty: None,
            default_value: None,
        });
    }
    None
}

fn typed_parameter_name(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind_id = child.kind_id();
        if kind_id == NODE_KINDS.identifier
            || kind_id == NODE_KINDS.list_splat_pattern
            || kind_id == NODE_KINDS.dictionary_splat_pattern
        {
            return Some(child);
        }
    }
    None
}

fn parameter_name_with_type(
    name_node: Node<'_>,
    ty: Option<String>,
    default_value: Option<String>,
    source: &[u8],
    is_keyword: bool,
) -> Option<FunctionArg> {
    let kind_id = name_node.kind_id();
    if kind_id == NODE_KINDS.identifier {
        return Some(FunctionArg {
            name: node_text(name_node, source)?.to_string(),
            is_keyword,
            ty,
            default_value,
        });
    }
    if kind_id == NODE_KINDS.list_splat_pattern {
        return Some(FunctionArg {
            name: splat_pattern_name(name_node, source, "*"),
            is_keyword,
            ty,
            default_value,
        });
    }
    if kind_id == NODE_KINDS.dictionary_splat_pattern {
        return Some(FunctionArg {
            name: splat_pattern_name(name_node, source, "**"),
            is_keyword: true,
            ty,
            default_value,
        });
    }
    None
}

fn splat_pattern_name(node: Node<'_>, source: &[u8], prefix: &str) -> String {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind_id() == NODE_KINDS.identifier
            && let Some(name) = node_text(child, source)
        {
            return format!("{prefix}{name}");
        }
    }
    node_text(node, source)
        .map(|text| format!("{prefix}{}", text.trim_start_matches('*').trim()))
        .unwrap_or_else(|| prefix.to_string())
}

fn function_constructs_app(
    function_name: &str,
    function_node: Node,
    source: &[u8],
    module_info: &ModuleInfo<'_>,
    depth: usize,
    visited: &mut HashSet<String>,
) -> bool {
    if depth > MAX_FUNCTION_CALL_DEPTH {
        return false;
    }
    if !visited.insert(function_name.to_string()) {
        return false;
    }
    let Some(body) = function_node.child_by_field_name(FIELD_BODY) else {
        return false;
    };
    scope_contains_app(body, source, module_info, depth, visited)
}

fn scope_contains_app(
    node: Node,
    source: &[u8],
    module_info: &ModuleInfo<'_>,
    depth: usize,
    visited: &mut HashSet<String>,
) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind_id = child.kind_id();
        if is_scope_boundary(kind_id) {
            continue;
        }
        if kind_id == NODE_KINDS.call && call_yields_app(child, source, module_info, depth, visited)
        {
            return true;
        }
        if scope_contains_app(child, source, module_info, depth, visited) {
            return true;
        }
    }
    false
}

fn is_scope_boundary(kind_id: u16) -> bool {
    kind_id == NODE_KINDS.function_definition
        || kind_id == NODE_KINDS.class_definition
        || kind_id == NODE_KINDS.lambda
}

fn dotted_name_is_marimo(node: Node, source: &[u8]) -> bool {
    let Some(text) = node_text(node, source) else {
        return false;
    };
    text == MODULE_MARIMO
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    const NODE_KIND_NAMES: &[&str] = &[
        KIND_ASSIGNMENT,
        KIND_AUGMENTED_ASSIGNMENT,
        KIND_ASYNC,
        KIND_BLOCK,
        KIND_CALL,
        KIND_CONCATENATED_STRING,
        KIND_DECORATED_DEFINITION,
        KIND_DECORATOR,
        KIND_DEFAULT_PARAMETER,
        KIND_DICTIONARY_SPLAT_PATTERN,
        KIND_EXPRESSION_STATEMENT,
        KIND_IMPORT_STATEMENT,
        KIND_IMPORT_FROM_STATEMENT,
        KIND_KEYWORD_SEPARATOR,
        KIND_LIST_SPLAT_PATTERN,
        KIND_POSITIONAL_SEPARATOR,
        KIND_STRING,
        KIND_TYPED_DEFAULT_PARAMETER,
        KIND_TYPED_PARAMETER,
        KIND_FUNCTION_DEFINITION,
        KIND_CLASS_DEFINITION,
        KIND_LAMBDA,
        KIND_IDENTIFIER,
        KIND_ATTRIBUTE,
        KIND_SUBSCRIPT,
        KIND_DOTTED_NAME,
        KIND_ALIASED_IMPORT,
        KIND_RELATIVE_IMPORT,
        KIND_EXPRESSION_LIST,
        KIND_PARENTHESIZED_EXPRESSION,
    ];

    const NODE_FIELD_NAMES: &[(&str, &str)] = &[
        (KIND_ASSIGNMENT, FIELD_LEFT),
        (KIND_ASSIGNMENT, FIELD_RIGHT),
        (KIND_AUGMENTED_ASSIGNMENT, FIELD_LEFT),
        (KIND_AUGMENTED_ASSIGNMENT, FIELD_RIGHT),
        (KIND_CALL, FIELD_FUNCTION),
        (KIND_ATTRIBUTE, FIELD_OBJECT),
        (KIND_ATTRIBUTE, FIELD_ATTRIBUTE),
        (KIND_DECORATED_DEFINITION, FIELD_DEFINITION),
        (KIND_DEFAULT_PARAMETER, FIELD_NAME),
        (KIND_DEFAULT_PARAMETER, FIELD_VALUE),
        (KIND_FUNCTION_DEFINITION, FIELD_BODY),
        (KIND_FUNCTION_DEFINITION, FIELD_NAME),
        (KIND_FUNCTION_DEFINITION, FIELD_PARAMETERS),
        (KIND_FUNCTION_DEFINITION, FIELD_RETURN_TYPE),
        (KIND_ALIASED_IMPORT, FIELD_NAME),
        (KIND_ALIASED_IMPORT, FIELD_ALIAS),
        (KIND_IMPORT_FROM_STATEMENT, FIELD_MODULE_NAME),
        (KIND_IMPORT_FROM_STATEMENT, FIELD_NAME),
        (KIND_TYPED_DEFAULT_PARAMETER, FIELD_NAME),
        (KIND_TYPED_DEFAULT_PARAMETER, FIELD_TYPE),
        (KIND_TYPED_DEFAULT_PARAMETER, FIELD_VALUE),
        (KIND_TYPED_PARAMETER, FIELD_TYPE),
    ];

    fn is_notebook(source: &str) -> bool {
        get_marimo_notebook(Bytes::copy_from_slice(source.as_bytes())).is_some()
    }

    fn notebook_meta(source: &str) -> Option<NotebookMeta> {
        get_marimo_notebook(Bytes::copy_from_slice(source.as_bytes()))
            .map(|notebook| notebook.meta())
    }

    fn assert_arg(
        arg: &FunctionArg,
        name: &str,
        is_keyword: bool,
        ty: Option<&str>,
        default_value: Option<&str>,
    ) {
        assert_eq!(arg.name, name);
        assert_eq!(arg.is_keyword, is_keyword);
        assert_eq!(arg.ty.as_deref(), ty);
        assert_eq!(arg.default_value.as_deref(), default_value);
    }

    #[test]
    fn python_node_kinds_are_valid() {
        let node_types: serde_json::Value =
            serde_json::from_str(tree_sitter_python::NODE_TYPES).expect("parse node-types.json");
        let types = node_types
            .as_array()
            .expect("node-types.json should be a JSON array");

        let mut kinds = HashSet::new();
        let mut fields: HashMap<String, HashSet<String>> = HashMap::new();

        for item in types {
            let kind = item
                .get("type")
                .and_then(|value| value.as_str())
                .expect("node type missing 'type'");
            kinds.insert(kind.to_string());

            if let Some(fields_obj) = item.get("fields").and_then(|value| value.as_object()) {
                let entry = fields.entry(kind.to_string()).or_default();
                for field in fields_obj.keys() {
                    entry.insert(field.clone());
                }
            }
        }

        for kind in NODE_KIND_NAMES {
            assert!(
                kinds.contains(*kind),
                "expected node kind '{kind}' in node-types.json"
            );
        }

        for (node_kind, field) in NODE_FIELD_NAMES {
            let Some(node_fields) = fields.get(*node_kind) else {
                panic!("expected fields for node kind '{node_kind}' in node-types.json");
            };
            assert!(
                node_fields.contains(*field),
                "expected field '{field}' in node kind '{node_kind}'"
            );
        }
    }

    #[test]
    fn detects_import_marimo_and_app_assignment() {
        let source = r#"
import marimo

app = marimo.App()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_from_marimo_import_and_app_assignment() {
        let source = r#"
from marimo import App as M

app = M()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn rejects_missing_marimo_import() {
        let source = r#"
app = object()
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn rejects_missing_app_assignment() {
        let source = r#"
import marimo

def make():
    return marimo.App()
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn rejects_app_assignment_in_function_scope() {
        let source = r#"
import marimo

def build():
    app = marimo.App()
    return app
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn rejects_attribute_assignment_only() {
        let source = r#"
import marimo as mo

config.app = mo.App()
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn detects_tuple_assignment_with_app() {
        let source = r#"
import marimo

app, other = marimo.App(), 1
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_annotated_assignment() {
        let source = r#"
import marimo

app: marimo.App = marimo.App()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_import_marimo_as_alias() {
        let source = r#"
import marimo as mo

app = mo.App()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn rejects_similar_module_name() {
        let source = r#"
import marimo_tools

app = marimo_tools.App()
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn finds_differently_named_app() {
        let source = r#"
import marimo

notebook = marimo.App()
     "#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_import_alias_and_assignment() {
        let source = r#"
import marimo as mo

notebook = mo.App()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_from_import_app_assignment() {
        let source = r#"
from marimo import App

notebook = App()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_from_import_app_alias_assignment() {
        let source = r#"
from marimo import App as A

notebook = A()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_function_returned_app_assigned_globally() {
        let source = r#"
import marimo

def make():
    return marimo.App()

notebook = make()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn detects_chained_function_returned_app_assigned_globally() {
        let source = r#"
import marimo

def inner():
    return marimo.App()

def outer():
    return inner()

app = outer()
"#;
        assert!(is_notebook(source));
    }

    #[test]
    fn rejects_function_without_global_assignment() {
        let source = r#"
import marimo

def make():
    return marimo.App()
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn rejects_recursive_function_without_app_construction() {
        let source = r#"
import marimo

def loop():
    return loop()

app = loop()
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn rejects_nested_function_app_without_call() {
        let source = r#"
import marimo

def make():
    def inner():
        return marimo.App()
    return inner

app = make()
"#;
        assert!(!is_notebook(source));
    }

    #[test]
    fn extracts_decorated_function_signature() {
        let source = r#"
import marimo

app = marimo.App()

@app.function
def add(a: int, b, *args: float, c: str = "x", **kwargs: bool) -> float:
    return 0.0
"#;
        let notebook = notebook_meta(source).expect("expected marimo notebook");
        assert_eq!(notebook.funcs.len(), 1);
        let function = &notebook.funcs[0];
        assert_eq!(function.name, "add");
        assert!(!function.is_async);
        assert_eq!(function.return_ty.as_deref(), Some("float"));
        assert_eq!(function.args.len(), 5);
        assert_arg(&function.args[0], "a", false, Some("int"), None);
        assert_arg(&function.args[1], "b", false, None, None);
        assert_arg(&function.args[2], "*args", false, Some("float"), None);
        assert_arg(&function.args[3], "c", true, Some("str"), Some("\"x\""));
        assert_arg(&function.args[4], "**kwargs", true, Some("bool"), None);
    }

    #[test]
    fn detects_app_function_with_alias_and_decorator_call() {
        let source = r#"
import marimo

def build():
    return marimo.App()

notebook = build()

@notebook.function()
def run():
    return 1
"#;
        let notebook = notebook_meta(source).expect("expected marimo notebook");
        assert_eq!(notebook.funcs.len(), 1);
        let function = &notebook.funcs[0];
        assert_eq!(function.name, "run");
    }

    #[test]
    fn detects_async_app_function() {
        let source = r#"
import marimo

app = marimo.App()

@app.function
async def fetch(url: str) -> str:
    return url
"#;
        let notebook = notebook_meta(source).expect("expected marimo notebook");
        assert_eq!(notebook.funcs.len(), 1);
        let function = &notebook.funcs[0];
        assert!(function.is_async);
        assert_eq!(function.name, "fetch");
        assert_eq!(function.return_ty.as_deref(), Some("str"));
        assert_eq!(function.args.len(), 1);
        assert_arg(&function.args[0], "url", false, Some("str"), None);
    }

    #[test]
    fn detects_keyword_only_args_without_types() {
        let source = r#"
import marimo

app = marimo.App()

@app.function
def add(a: int, b: int, *, abs = True):
    return a + b
"#;
        let notebook = notebook_meta(source).expect("expected marimo notebook");
        assert_eq!(notebook.funcs.len(), 1);
        let function = &notebook.funcs[0];
        assert_eq!(function.name, "add");
        assert_eq!(function.args.len(), 3);
        assert_arg(&function.args[0], "a", false, Some("int"), None);
        assert_arg(&function.args[1], "b", false, Some("int"), None);
        assert_arg(&function.args[2], "abs", true, None, Some("True"));
    }

    #[test]
    fn extracts_function_docstring() {
        let source = r#"
import marimo

app = marimo.App()

@app.function
def greet():
    """Say hi."""
    return "hi"
"#;
        let notebook = notebook_meta(source).expect("expected marimo notebook");
        assert_eq!(notebook.funcs.len(), 1);
        let function = &notebook.funcs[0];
        assert_eq!(function.name, "greet");
        assert_eq!(function.docs.as_deref(), Some("Say hi."));
    }
}
