//! Scope-building walker.
//!
//! Walks a Program AST (stored as `serde_json::Value` since we go through
//! the acorn-shaped wire format) and populates a scope tree.
//!
//! Mirrors `create_scopes` and the `analyze_module` visitor map in
//! `packages/svelte/src/compiler/phases/scope.js`. We don't run the full
//! reference-resolution / rune-call inspection pass here — just declare
//! every binding in the right scope and classify them by their initializer.
//!
//! Nested scopes are created for:
//! - Function bodies (params declared in the new scope).
//! - Arrow function bodies (same).
//! - `BlockStatement` (for `let`/`const` scoping).
//! - `ForStatement` / `ForInStatement` / `ForOfStatement` init.
//! - `CatchClause` param.
//! - `ClassExpression` / `ClassDeclaration` body (class scope).
//! - Object/array destructuring patterns introduce bindings in the
//!   enclosing function/block scope.

use serde_json::Value;

use svelte_ast::Root;

use crate::scope::{BindingKind, DeclarationKind, Scope, ScopePtr};

/// Walk a Program node (the `content` field of a `Script`) and populate
/// `root_scope` and its descendants.
pub fn build_program_scope(program: &Value, root_scope: &ScopePtr) {
    let Some(body) = program.get("body").and_then(|v| v.as_array()) else {
        return;
    };
    // Two passes — first hoist function/var declarations (they're visible
    // before their textual position), then walk normally. Simplification of
    // upstream's full hoisting algorithm.
    for stmt in body {
        hoist_declarations(stmt, root_scope);
    }
    for stmt in body {
        visit_statement(stmt, root_scope);
    }
}

/// Pre-pass: declare hoisted bindings (`var` and `function` declarations
/// only) so they're visible to forward references.
fn hoist_declarations(node: &Value, scope: &ScopePtr) {
    let Some(t) = node.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match t {
        "VariableDeclaration" => {
            if node.get("kind").and_then(|v| v.as_str()) == Some("var") {
                visit_variable_declaration(node, scope);
            }
        }
        "FunctionDeclaration" => visit_function_declaration_decl_only(node, scope),
        "ExportNamedDeclaration" | "ExportDefaultDeclaration" => {
            if let Some(decl) = node.get("declaration") {
                hoist_declarations(decl, scope);
            }
        }
        _ => {}
    }
}

fn visit_statement(node: &Value, scope: &ScopePtr) {
    let Some(t) = node.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match t {
        // Declarations.
        "VariableDeclaration" => {
            if node.get("kind").and_then(|v| v.as_str()) != Some("var") {
                visit_variable_declaration(node, scope);
            }
        }
        "FunctionDeclaration" => visit_function(node, scope, /*as_decl=*/ true),
        "ClassDeclaration" => visit_class(node, scope, /*as_decl=*/ true),
        "ImportDeclaration" => visit_import_declaration(node, scope),
        "ExportNamedDeclaration" => {
            if let Some(decl) = node.get("declaration") {
                visit_statement(decl, scope);
            }
        }
        "ExportDefaultDeclaration" => {
            if let Some(decl) = node.get("declaration") {
                let dt = decl.get("type").and_then(|v| v.as_str());
                match dt {
                    Some("FunctionDeclaration") => visit_function(decl, scope, true),
                    Some("ClassDeclaration") => visit_class(decl, scope, true),
                    _ => visit_expression(decl, scope),
                }
            }
        }

        // Control flow / blocks that introduce nested scope.
        "BlockStatement" => {
            let block_scope = Scope::child(scope, /*is_block=*/ true);
            visit_block(node, &block_scope);
        }
        "IfStatement" => {
            if let Some(test) = node.get("test") {
                visit_expression(test, scope);
            }
            if let Some(cons) = node.get("consequent") {
                visit_statement(cons, scope);
            }
            if let Some(alt) = node.get("alternate") {
                visit_statement(alt, scope);
            }
        }
        "ForStatement" => {
            let for_scope = Scope::child(scope, true);
            if let Some(init) = node.get("init") {
                if init.get("type").and_then(|v| v.as_str()) == Some("VariableDeclaration") {
                    visit_variable_declaration(init, &for_scope);
                } else {
                    visit_expression(init, &for_scope);
                }
            }
            if let Some(test) = node.get("test") {
                visit_expression(test, &for_scope);
            }
            if let Some(update) = node.get("update") {
                visit_expression(update, &for_scope);
            }
            if let Some(body) = node.get("body") {
                visit_statement(body, &for_scope);
            }
        }
        "ForInStatement" | "ForOfStatement" => {
            let for_scope = Scope::child(scope, true);
            if let Some(left) = node.get("left") {
                if left.get("type").and_then(|v| v.as_str()) == Some("VariableDeclaration") {
                    visit_variable_declaration(left, &for_scope);
                } else {
                    visit_expression(left, &for_scope);
                }
            }
            if let Some(right) = node.get("right") {
                visit_expression(right, &for_scope);
            }
            if let Some(body) = node.get("body") {
                visit_statement(body, &for_scope);
            }
        }
        "WhileStatement" | "DoWhileStatement" => {
            if let Some(test) = node.get("test") {
                visit_expression(test, scope);
            }
            if let Some(body) = node.get("body") {
                visit_statement(body, scope);
            }
        }
        "TryStatement" => {
            if let Some(block) = node.get("block") {
                visit_statement(block, scope);
            }
            if let Some(handler) = node.get("handler") {
                let catch_scope = Scope::child(scope, true);
                if let Some(param) = handler.get("param") {
                    collect_pattern_names(param, &catch_scope, DeclarationKind::Let);
                }
                if let Some(body) = handler.get("body") {
                    visit_statement(body, &catch_scope);
                }
            }
            if let Some(finalizer) = node.get("finalizer") {
                visit_statement(finalizer, scope);
            }
        }
        "SwitchStatement" => {
            if let Some(disc) = node.get("discriminant") {
                visit_expression(disc, scope);
            }
            let switch_scope = Scope::child(scope, true);
            if let Some(cases) = node.get("cases").and_then(|v| v.as_array()) {
                for case in cases {
                    if let Some(test) = case.get("test") {
                        if !test.is_null() {
                            visit_expression(test, &switch_scope);
                        }
                    }
                    if let Some(consequent) = case.get("consequent").and_then(|v| v.as_array()) {
                        for stmt in consequent {
                            visit_statement(stmt, &switch_scope);
                        }
                    }
                }
            }
        }
        "LabeledStatement" => {
            if let Some(body) = node.get("body") {
                visit_statement(body, scope);
            }
        }
        "ReturnStatement" | "ThrowStatement" => {
            if let Some(arg) = node.get("argument") {
                if !arg.is_null() {
                    visit_expression(arg, scope);
                }
            }
        }
        "ExpressionStatement" => {
            if let Some(expr) = node.get("expression") {
                visit_expression(expr, scope);
            }
        }
        _ => {
            // Unknown / unhandled statement — descend into any child Values
            // that look like nodes so we don't miss references.
            descend_unknown(node, scope);
        }
    }
}

fn visit_block(block: &Value, scope: &ScopePtr) {
    // Hoist pass for the block (var + function declarations bubble up).
    if let Some(body) = block.get("body").and_then(|v| v.as_array()) {
        for stmt in body {
            hoist_declarations(stmt, scope);
        }
        for stmt in body {
            visit_statement(stmt, scope);
        }
    }
}

fn visit_variable_declaration(node: &Value, scope: &ScopePtr) {
    let kind = node
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|k| match k {
            "let" => DeclarationKind::Let,
            "const" => DeclarationKind::Const,
            "using" => DeclarationKind::Using,
            "await using" => DeclarationKind::AwaitUsing,
            _ => DeclarationKind::Var,
        })
        .unwrap_or(DeclarationKind::Var);

    let Some(decls) = node.get("declarations").and_then(|v| v.as_array()) else {
        return;
    };
    for d in decls {
        let init = d.get("init").filter(|v| !v.is_null());
        if let Some(id) = d.get("id") {
            declare_pattern(id, scope, kind, init);
        }
        // Walk into init for references.
        if let Some(init) = init {
            visit_expression(init, scope);
        }
    }
}

fn visit_function_declaration_decl_only(node: &Value, scope: &ScopePtr) {
    let Some(id) = node.get("id") else { return };
    if id.is_null() {
        return;
    }
    let Some(name) = id.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    scope.borrow_mut().declare(
        name.to_string(),
        BindingKind::Normal,
        DeclarationKind::Function,
        id.clone(),
    );
}

fn visit_function(node: &Value, parent_scope: &ScopePtr, as_decl: bool) {
    // For declarations, the name binds in the enclosing scope. (For
    // function expressions, the name binds inside the function's own scope
    // — but we don't yet model that distinction.)
    if as_decl {
        visit_function_declaration_decl_only(node, parent_scope);
    }
    let fn_scope = Scope::child(parent_scope, /*is_block=*/ false);
    if let Some(params) = node.get("params").and_then(|v| v.as_array()) {
        for p in params {
            declare_pattern(p, &fn_scope, DeclarationKind::Param, None);
        }
    }
    if let Some(body) = node.get("body") {
        let body_type = body.get("type").and_then(|v| v.as_str());
        match body_type {
            Some("BlockStatement") => visit_block(body, &fn_scope),
            _ => visit_expression(body, &fn_scope), // arrow expr body
        }
    }
}

fn visit_class(node: &Value, parent_scope: &ScopePtr, as_decl: bool) {
    if as_decl {
        if let Some(id) = node.get("id") {
            if !id.is_null() {
                if let Some(name) = id.get("name").and_then(|v| v.as_str()) {
                    parent_scope.borrow_mut().declare(
                        name.to_string(),
                        BindingKind::Normal,
                        DeclarationKind::Let,
                        id.clone(),
                    );
                }
            }
        }
    }
    // SuperClass + decorators live in parent scope.
    if let Some(sc) = node.get("superClass") {
        if !sc.is_null() {
            visit_expression(sc, parent_scope);
        }
    }
    let class_scope = Scope::child(parent_scope, /*is_block=*/ false);
    if let Some(body) = node.get("body") {
        if let Some(body_arr) = body.get("body").and_then(|v| v.as_array()) {
            for member in body_arr {
                let mt = member.get("type").and_then(|v| v.as_str());
                match mt {
                    Some("MethodDefinition") | Some("PropertyDefinition") => {
                        if let Some(value) = member.get("value") {
                            if !value.is_null() {
                                visit_expression(value, &class_scope);
                            }
                        }
                        if let Some(key) = member.get("key") {
                            if member.get("computed").and_then(|v| v.as_bool()) == Some(true) {
                                visit_expression(key, &class_scope);
                            }
                        }
                    }
                    _ => {
                        descend_unknown(member, &class_scope);
                    }
                }
            }
        }
    }
}

fn visit_import_declaration(node: &Value, scope: &ScopePtr) {
    let Some(specifiers) = node.get("specifiers").and_then(|v| v.as_array()) else {
        return;
    };
    for spec in specifiers {
        let Some(local) = spec.get("local") else { continue };
        let Some(name) = local.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        scope.borrow_mut().declare(
            name.to_string(),
            BindingKind::Normal,
            DeclarationKind::Import,
            local.clone(),
        );
    }
}

/// Walk an Expression. Records identifier references against `scope` (when
/// the resolver lands) and recurses into nested function/class scopes.
fn visit_expression(node: &Value, scope: &ScopePtr) {
    let Some(t) = node.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match t {
        "Identifier" => {
            if let Some(name) = node.get("name").and_then(|v| v.as_str()) {
                Scope::reference_chain(
                    scope,
                    name.to_string(),
                    crate::scope::Reference {
                        node: node.clone(),
                        path: Vec::new(),
                    },
                );
            }
        }
        "Literal" | "TemplateElement" | "ThisExpression" | "Super" | "MetaProperty" => {}
        "FunctionExpression" => visit_function(node, scope, /*as_decl=*/ false),
        "ArrowFunctionExpression" => visit_function(node, scope, /*as_decl=*/ false),
        "ClassExpression" => visit_class(node, scope, /*as_decl=*/ false),
        // MemberExpression: walk object; only walk property when computed.
        "MemberExpression" => {
            if let Some(obj) = node.get("object") {
                visit_expression(obj, scope);
            }
            if node.get("computed").and_then(|v| v.as_bool()) == Some(true) {
                if let Some(prop) = node.get("property") {
                    visit_expression(prop, scope);
                }
            }
        }
        "ObjectExpression" => {
            if let Some(props) = node.get("properties").and_then(|v| v.as_array()) {
                for p in props {
                    let pt = p.get("type").and_then(|v| v.as_str());
                    if pt == Some("Property") {
                        if p.get("computed").and_then(|v| v.as_bool()) == Some(true) {
                            if let Some(key) = p.get("key") {
                                visit_expression(key, scope);
                            }
                        }
                        if let Some(value) = p.get("value") {
                            visit_expression(value, scope);
                        }
                    } else if pt == Some("SpreadElement") {
                        if let Some(arg) = p.get("argument") {
                            visit_expression(arg, scope);
                        }
                    }
                }
            }
        }
        _ => descend_unknown(node, scope),
    }
}

/// Last-resort: recursively visit every child Value, treating anything with
/// a `type` field as an expression-like node. Used for AST shapes we don't
/// have a dedicated handler for yet.
fn descend_unknown(node: &Value, scope: &ScopePtr) {
    match node {
        Value::Object(map) => {
            for (key, v) in map {
                // Skip metadata-ish fields that shouldn't be walked.
                if matches!(
                    key.as_str(),
                    "loc"
                        | "type"
                        | "start"
                        | "end"
                        | "kind"
                        | "operator"
                        | "name"
                        | "value"
                        | "raw"
                        | "regex"
                        | "bigint"
                        | "sourceType"
                        | "directive"
                        | "shorthand"
                        | "computed"
                        | "method"
                        | "static"
                        | "generator"
                        | "async"
                        | "expression"
                        | "delegate"
                        | "prefix"
                        | "tail"
                        | "leadingComments"
                        | "trailingComments"
                ) {
                    continue;
                }
                match v {
                    Value::Object(_) if v.get("type").is_some() => {
                        // Looks like a node — visit as expression.
                        visit_expression(v, scope);
                    }
                    Value::Array(arr) => {
                        for item in arr {
                            if item.is_object() && item.get("type").is_some() {
                                visit_expression(item, scope);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Walk a pattern and declare every binding identifier inside, with the
/// kind inferred from `init` (so `let foo = $state(...)` declares a
/// `State` binding). Mirrors `extract_identifiers` + the rune-classifying
/// step in `phases/2-analyze/visitors/CallExpression.js`.
fn declare_pattern(
    pattern: &Value,
    scope: &ScopePtr,
    decl_kind: DeclarationKind,
    init: Option<&Value>,
) {
    let pat_type = pattern.get("type").and_then(|v| v.as_str());
    let init_rune = init.and_then(get_rune_keypath);

    // For destructured props (`let { foo } = $props()`) every name in the
    // pattern is a Prop. For `let foo = $state(...)` it's State / Derived /
    // etc. For other init shapes, the kind depends per-binder (could be
    // BindableProp if the init has `$bindable()`).
    if matches!(pat_type, Some("Identifier")) {
        if let Some(name) = pattern.get("name").and_then(|v| v.as_str()) {
            let kind = init_rune
                .as_deref()
                .map(rune_to_binding_kind)
                .unwrap_or(BindingKind::Normal);
            scope.borrow_mut().declare(
                name.to_string(),
                kind,
                decl_kind,
                pattern.clone(),
            );
        }
        return;
    }

    // For destructuring patterns, walk children and classify each.
    let pattern_kind = match init_rune.as_deref() {
        Some("$props") => BindingKind::Prop,
        Some(other) => rune_to_binding_kind(other),
        None => BindingKind::Normal,
    };

    declare_destructuring(pattern, scope, decl_kind, pattern_kind);
}

fn declare_destructuring(
    pattern: &Value,
    scope: &ScopePtr,
    decl_kind: DeclarationKind,
    default_kind: BindingKind,
) {
    let Some(t) = pattern.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    match t {
        "Identifier" => {
            if let Some(name) = pattern.get("name").and_then(|v| v.as_str()) {
                scope.borrow_mut().declare(
                    name.to_string(),
                    default_kind,
                    decl_kind,
                    pattern.clone(),
                );
            }
        }
        "ObjectPattern" => {
            if let Some(props) = pattern.get("properties").and_then(|v| v.as_array()) {
                for p in props {
                    let pt = p.get("type").and_then(|v| v.as_str());
                    match pt {
                        Some("Property") => {
                            let value = p.get("value");
                            // If the property's value is `AssignmentPattern`
                            // whose `right` is `$bindable()`, the destructured
                            // name is a BindableProp.
                            let elem_kind =
                                value.and_then(detect_bindable).unwrap_or(default_kind);
                            if let Some(value) = value {
                                declare_destructuring(value, scope, decl_kind, elem_kind);
                            }
                        }
                        Some("RestElement") => {
                            if let Some(arg) = p.get("argument") {
                                let rest_kind = if matches!(default_kind, BindingKind::Prop) {
                                    BindingKind::RestProp
                                } else {
                                    default_kind
                                };
                                declare_destructuring(arg, scope, decl_kind, rest_kind);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        "ArrayPattern" => {
            if let Some(elements) = pattern.get("elements").and_then(|v| v.as_array()) {
                for el in elements {
                    if el.is_null() {
                        continue;
                    }
                    declare_destructuring(el, scope, decl_kind, default_kind);
                }
            }
        }
        "RestElement" => {
            if let Some(arg) = pattern.get("argument") {
                let rest_kind = if matches!(default_kind, BindingKind::Prop) {
                    BindingKind::RestProp
                } else {
                    default_kind
                };
                declare_destructuring(arg, scope, decl_kind, rest_kind);
            }
        }
        "AssignmentPattern" => {
            if let Some(left) = pattern.get("left") {
                let elem_kind =
                    detect_bindable(pattern).unwrap_or(default_kind);
                declare_destructuring(left, scope, decl_kind, elem_kind);
            }
        }
        _ => {}
    }
}

/// If `node` is an `AssignmentPattern` whose `right` is a `$bindable()`
/// call, return `Some(BindingKind::BindableProp)`. Mirrors the upstream
/// `$bindable` detection in CallExpression / VariableDeclarator visitors.
fn detect_bindable(node: &Value) -> Option<BindingKind> {
    if node.get("type").and_then(|v| v.as_str()) != Some("AssignmentPattern") {
        return None;
    }
    let right = node.get("right")?;
    let rune = get_rune_keypath(right)?;
    match rune.as_str() {
        "$bindable" => Some(BindingKind::BindableProp),
        _ => None,
    }
}

/// Pattern variant of `collect_pattern_names` retained for callers that
/// don't have an init (e.g. function params, `catch` clauses) and so
/// shouldn't classify by rune.
fn collect_pattern_names(pattern: &Value, scope: &ScopePtr, kind: DeclarationKind) {
    declare_destructuring(pattern, scope, kind, BindingKind::Normal);
}

/// If `node` is a `CallExpression` whose callee resolves to a rune, return
/// the dotted keypath (e.g. `$state.raw`, `$derived`). Mirrors `get_rune`
/// + `get_global_keypath` in scope.js:1429-1480.
///
/// This is a simplified version that doesn't consult the scope chain — we
/// trust the caller to only invoke this on initializers where the rune
/// keyword wasn't shadowed by a local binding. (Upstream's full version
/// checks the scope; we'll add that when reference resolution lands.)
fn get_rune_keypath(node: &Value) -> Option<String> {
    if node.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return None;
    }
    let callee = node.get("callee")?;
    let key = global_keypath(callee)?;
    if is_rune(&key) {
        Some(key)
    } else {
        None
    }
}

fn global_keypath(node: &Value) -> Option<String> {
    let mut n = node;
    let mut joined = String::new();
    while n.get("type").and_then(|v| v.as_str()) == Some("MemberExpression") {
        if n.get("computed").and_then(|v| v.as_bool()) == Some(true) {
            return None;
        }
        let prop = n.get("property")?;
        if prop.get("type").and_then(|v| v.as_str()) != Some("Identifier") {
            return None;
        }
        let name = prop.get("name").and_then(|v| v.as_str())?;
        joined = format!(".{name}{joined}");
        n = n.get("object")?;
    }
    if n.get("type").and_then(|v| v.as_str()) != Some("Identifier") {
        return None;
    }
    let base = n.get("name").and_then(|v| v.as_str())?;
    Some(format!("{base}{joined}"))
}

fn is_rune(name: &str) -> bool {
    matches!(
        name,
        "$state"
            | "$state.raw"
            | "$state.eager"
            | "$state.snapshot"
            | "$derived"
            | "$derived.by"
            | "$props"
            | "$props.id"
            | "$bindable"
            | "$effect"
            | "$effect.pre"
            | "$effect.tracking"
            | "$effect.root"
            | "$effect.pending"
            | "$inspect"
            | "$inspect().with"
            | "$inspect.trace"
            | "$host"
    )
}

/// Map a rune keypath like `$state.raw` to the binding kind it produces
/// when used as the initializer of a `let` / `const` declarator.
fn rune_to_binding_kind(rune: &str) -> BindingKind {
    match rune {
        "$state" => BindingKind::State,
        "$state.raw" => BindingKind::RawState,
        "$derived" | "$derived.by" => BindingKind::Derived,
        "$props" => BindingKind::Prop,
        _ => BindingKind::Normal,
    }
}

/// Returns true if the component uses any rune (`$state`, `$derived`,
/// `$effect`, `$props`, `$bindable`, `$inspect`, `$host`, plus `.raw` /
/// `.by` / etc. variants).
pub fn detect_runes(root: &Root) -> bool {
    fn walk(node: &Value) -> bool {
        match node {
            Value::Object(map) => {
                if map.get("type").and_then(|v| v.as_str()) == Some("CallExpression") {
                    if let Some(callee) = map.get("callee") {
                        if let Some(key) = global_keypath(callee) {
                            if is_rune(&key) {
                                return true;
                            }
                        }
                    }
                }
                for (_, v) in map.iter() {
                    if walk(v) {
                        return true;
                    }
                }
                false
            }
            Value::Array(arr) => arr.iter().any(walk),
            _ => false,
        }
    }
    if let Some(s) = root.module.as_ref() {
        if walk(&s.content) {
            return true;
        }
    }
    if let Some(s) = root.instance.as_ref() {
        if walk(&s.content) {
            return true;
        }
    }
    false
}
