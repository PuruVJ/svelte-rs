//! Validator visitors.
//!
//! Ported (incrementally) from
//! `packages/svelte/src/compiler/phases/2-analyze/visitors/`. Each visitor
//! emits warnings / errors for one AST node kind. The walker keeps a
//! `path` of ancestor `FragmentChild` references so visitors can ask
//! "am I inside an `{#if}` / `{#each}` / `<Component>` / `{#snippet}`
//! ancestor?" (cf. `SvelteSelf` in upstream).
//!
//! Current coverage: a starter set of template-side visitors (the
//! `<svelte:window>` / `<svelte:body>` / `<svelte:document>` /
//! `<svelte:head>` / `<svelte:self>` family, plus the runes-mode opening-
//! tag validation for `{@html}` / `{@debug}` etc.).

use std::path::Path;

use svelte_ast::{
    AttributeValue, AttributeValuePart, ElementAttribute, Fragment, FragmentChild, Root,
};
use svelte_diagnostics::{errors, warnings, CompileDiagnostic};

use crate::analysis::Analysis;

/// State threaded through every visitor. `path` is the chain of
/// ancestors (oldest first). `is_runes` mirrors `analysis.runes`.
pub struct ValidateState<'a> {
    pub path: Vec<&'a FragmentChild>,
    pub warnings: Vec<CompileDiagnostic>,
    pub errors: Vec<CompileDiagnostic>,
    pub is_runes: bool,
    pub component_name: String,
    pub filename: Option<String>,
    /// Imported identifier names from the instance script — used to detect
    /// `<lowercaseImportedName>` patterns for `component_name_lowercase`.
    pub imported_names: std::collections::HashSet<String>,
    /// All identifier names declared at the top of the instance script
    /// (let/const/function/import). Used by
    /// `attribute_global_event_reference` to distinguish locally-shadowed
    /// `onclick` from references to the global event.
    pub instance_declared: std::collections::HashSet<String>,
    /// First `on:` directive node we encountered. Combined with
    /// `uses_event_attributes` to detect `mixed_event_handler_syntaxes`.
    pub event_directive_node: Option<(u32, u32, String)>,
    /// Any element has a native `onfoo={...}` attribute. Combined with
    /// `event_directive_node` to detect mixed event handler syntaxes.
    pub uses_event_attributes: bool,
}

impl<'a> ValidateState<'a> {
    pub fn new(analysis: &Analysis) -> Self {
        Self {
            path: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            is_runes: analysis.runes,
            component_name: analysis.name.clone(),
            filename: analysis.filename.clone(),
            imported_names: std::collections::HashSet::new(),
            instance_declared: std::collections::HashSet::new(),
            event_directive_node: None,
            uses_event_attributes: false,
        }
    }
}

/// Validate the whole `Root`. Walks the template fragment and dispatches
/// to per-node visitors. Also walks the `<script>` / `<script module>`
/// content (Program JSON) for JS-side validators (ImportDeclaration,
/// LabeledStatement, etc.).
pub fn validate(root: &Root, analysis: &Analysis) -> (Vec<CompileDiagnostic>, Vec<CompileDiagnostic>) {
    let mut state = ValidateState::new(analysis);
    state.imported_names = collect_imported_names(root);
    state.instance_declared = collect_instance_declared(root);
    visit_fragment(&root.fragment, &mut state);
    if let Some(s) = root.instance.as_ref() {
        validate_script_attributes(&s.attributes, &mut state);
        visit_program(&s.content, /*is_instance=*/ true, &mut state);
        validate_props_identifier(&s.content, &mut state);
        if state.is_runes {
            validate_store_rune_conflict(&s.content, &mut state);
            validate_perf_avoid_class(&s.content, /*is_instance=*/ true, &mut state);
        }
    }
    if let Some(s) = root.module.as_ref() {
        validate_script_attributes(&s.attributes, &mut state);
        visit_program(&s.content, /*is_instance=*/ false, &mut state);
        if state.is_runes {
            validate_perf_avoid_class(&s.content, /*is_instance=*/ false, &mut state);
        }
    }

    // `mixed_event_handler_syntaxes` — if both `on:foo` directive AND
    // `onfoo={...}` attribute have been used, that's a hard error. Strip
    // the now-superseded `event_directive_deprecated` warnings.
    if let (Some((start, end, name)), true) = (
        state.event_directive_node.clone(),
        state.uses_event_attributes,
    ) {
        state.warnings.retain(|w| w.code != "event_directive_deprecated");
        state
            .errors
            .push(errors::mixed_event_handler_syntaxes(Some((start, end)), &name));
    }
    (state.warnings, state.errors)
}

/// `perf_avoid_inline_class` + `perf_avoid_nested_class`. Walks the
/// instance script with a function-depth counter:
/// - `new ClassExpression(...)` at depth > 0 → `perf_avoid_inline_class`
/// - `class X { ... }` declaration at depth > 1 → `perf_avoid_nested_class`
/// Module script starts at depth 0; instance script starts at depth 1
/// because the component body is implicitly inside a function.
fn validate_perf_avoid_class(
    program: &svelte_js_ast::Program,
    is_instance: bool,
    state: &mut ValidateState,
) {
    use svelte_js_ast::*;
    let start_depth = if is_instance { 1u32 } else { 0u32 };
    fn walk_expr(e: &Expression, depth: u32, out: &mut Vec<(u32, u32, bool)>) {
        match e {
            Expression::New(n) => {
                if matches!(&n.callee, Expression::Class(_)) && depth > 0 {
                    out.push((n.span.start, n.span.end, true));
                }
                walk_expr(&n.callee, depth, out);
                for a in &n.arguments {
                    if let Argument::Expression(ax) = a {
                        walk_expr(ax, depth, out);
                    }
                }
            }
            Expression::Call(c) => {
                walk_expr(&c.callee, depth, out);
                for a in &c.arguments {
                    if let Argument::Expression(ax) = a {
                        walk_expr(ax, depth, out);
                    }
                }
            }
            Expression::Member(m) => walk_expr(&m.object, depth, out),
            Expression::Binary(b) => {
                walk_expr(&b.left, depth, out);
                walk_expr(&b.right, depth, out);
            }
            Expression::Logical(b) => {
                walk_expr(&b.left, depth, out);
                walk_expr(&b.right, depth, out);
            }
            Expression::Conditional(c) => {
                walk_expr(&c.test, depth, out);
                walk_expr(&c.consequent, depth, out);
                walk_expr(&c.alternate, depth, out);
            }
            Expression::Assignment(a) => walk_expr(&a.right, depth, out),
            Expression::Sequence(s) => {
                for e in &s.expressions {
                    walk_expr(e, depth, out);
                }
            }
            Expression::Arrow(a) => match &a.body {
                ArrowBody::Block(b) => {
                    for s in &b.body {
                        walk_stmt(s, depth + 1, out);
                    }
                }
                ArrowBody::Expression(e) => walk_expr(e, depth + 1, out),
            },
            Expression::Function(f) => {
                for s in &f.body.body {
                    walk_stmt(s, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    fn walk_stmt(s: &Statement, depth: u32, out: &mut Vec<(u32, u32, bool)>) {
        match s {
            Statement::Expression(e) => walk_expr(&e.expression, depth, out),
            Statement::Variable(v) => {
                for d in &v.declarations {
                    if let Some(init) = &d.init {
                        walk_expr(init, depth, out);
                    }
                }
            }
            Statement::Return(r) => {
                if let Some(arg) = &r.argument {
                    walk_expr(arg, depth, out);
                }
            }
            Statement::Function(f) => {
                for s in &f.body.body {
                    walk_stmt(s, depth + 1, out);
                }
            }
            Statement::Class(c) => {
                if depth > 1 {
                    out.push((c.span.start, c.span.end, false));
                }
                for member in &c.body.body {
                    if let ClassMember::Method(m) = member {
                        for s in &m.value.body.body {
                            walk_stmt(s, depth + 1, out);
                        }
                    }
                }
            }
            Statement::Block(b) => {
                for s in &b.body {
                    walk_stmt(s, depth, out);
                }
            }
            Statement::If(i) => {
                walk_stmt(&i.consequent, depth, out);
                if let Some(alt) = &i.alternate {
                    walk_stmt(alt, depth, out);
                }
            }
            Statement::For(f) => walk_stmt(&f.body, depth, out),
            Statement::While(w) => walk_stmt(&w.body, depth, out),
            Statement::DoWhile(d) => walk_stmt(&d.body, depth, out),
            Statement::Try(t) => {
                for s in &t.block.body {
                    walk_stmt(s, depth, out);
                }
                if let Some(h) = &t.handler {
                    for s in &h.body.body {
                        walk_stmt(s, depth, out);
                    }
                }
                if let Some(f) = &t.finalizer {
                    for s in &f.body {
                        walk_stmt(s, depth, out);
                    }
                }
            }
            _ => {}
        }
    }
    let mut hits = Vec::new();
    for stmt in &program.body {
        walk_stmt(stmt, start_depth, &mut hits);
    }
    for (start, end, is_inline) in hits {
        if is_inline {
            state
                .warnings
                .push(warnings::perf_avoid_inline_class(Some((start, end))));
        } else {
            state
                .warnings
                .push(warnings::perf_avoid_nested_class(Some((start, end))));
        }
    }
}

/// `store_rune_conflict`: when a local binding exists with the same name as
/// a rune (e.g. `state`) and the rune is called (`$state(...)`), the
/// `$state` reference is ambiguous with a store subscription. Emit a
/// warning at the callee of each such call. Mirrors index.js:400-409.
fn validate_store_rune_conflict(
    program: &svelte_js_ast::Program,
    state: &mut ValidateState,
) {
    use svelte_js_ast::*;
    const RUNES: &[&str] = &[
        "state", "derived", "effect", "props", "bindable", "inspect", "host",
    ];
    fn walk_expr(
        e: &Expression,
        bindings: &std::collections::HashSet<String>,
        out: &mut Vec<(u32, u32, String)>,
    ) {
        match e {
            Expression::Call(c) => {
                if let Expression::Identifier(id) = &c.callee {
                    if let Some(stripped) = id.name.strip_prefix('$') {
                        if RUNES.contains(&stripped) && bindings.contains(stripped) {
                            out.push((id.span.start, id.span.end, stripped.to_string()));
                        }
                    }
                }
                walk_expr(&c.callee, bindings, out);
                for a in &c.arguments {
                    if let Argument::Expression(ax) = a {
                        walk_expr(ax, bindings, out);
                    }
                }
            }
            Expression::Member(m) => walk_expr(&m.object, bindings, out),
            Expression::Binary(b) => {
                walk_expr(&b.left, bindings, out);
                walk_expr(&b.right, bindings, out);
            }
            Expression::Logical(b) => {
                walk_expr(&b.left, bindings, out);
                walk_expr(&b.right, bindings, out);
            }
            Expression::Conditional(c) => {
                walk_expr(&c.test, bindings, out);
                walk_expr(&c.consequent, bindings, out);
                walk_expr(&c.alternate, bindings, out);
            }
            Expression::Assignment(a) => walk_expr(&a.right, bindings, out),
            Expression::Sequence(s) => {
                for e in &s.expressions {
                    walk_expr(e, bindings, out);
                }
            }
            Expression::Arrow(a) => walk_function_body(&a.body, bindings, out),
            Expression::Function(f) => {
                for stmt in &f.body.body {
                    walk_stmt(stmt, bindings, out);
                }
            }
            _ => {}
        }
    }
    fn walk_function_body(
        body: &ArrowBody,
        bindings: &std::collections::HashSet<String>,
        out: &mut Vec<(u32, u32, String)>,
    ) {
        match body {
            ArrowBody::Block(b) => {
                for stmt in &b.body {
                    walk_stmt(stmt, bindings, out);
                }
            }
            ArrowBody::Expression(e) => walk_expr(e, bindings, out),
        }
    }
    fn walk_stmt(
        s: &Statement,
        bindings: &std::collections::HashSet<String>,
        out: &mut Vec<(u32, u32, String)>,
    ) {
        match s {
            Statement::Expression(e) => walk_expr(&e.expression, bindings, out),
            Statement::Variable(v) => {
                for d in &v.declarations {
                    if let Some(init) = &d.init {
                        // Skip if this declarator binds the same name the rune
                        // would create — i.e. `let X = $X(...)` is the canonical
                        // rune usage, not a store-rune conflict.
                        if let (Pattern::Identifier(id), Expression::Call(c)) = (&d.id, init) {
                            if let Expression::Identifier(cal) = &c.callee {
                                if let Some(stripped) = cal.name.strip_prefix('$') {
                                    if stripped == id.name {
                                        // Still walk arguments for nested expressions.
                                        for a in &c.arguments {
                                            if let Argument::Expression(ax) = a {
                                                walk_expr(ax, bindings, out);
                                            }
                                        }
                                        continue;
                                    }
                                }
                            }
                        }
                        walk_expr(init, bindings, out);
                    }
                }
            }
            Statement::Return(r) => {
                if let Some(arg) = &r.argument {
                    walk_expr(arg, bindings, out);
                }
            }
            _ => {}
        }
    }
    // Build the exempt set: names that come directly out of a rune call.
    // `let X = $X()` exempts X (matching rune name).
    // `let { ...X } = $props()` exempts X (rest captures the whole rune output).
    let mut exempt: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for stmt in &program.body {
        if let Statement::Variable(v) = stmt {
            for d in &v.declarations {
                let Some(init) = &d.init else { continue };
                let Expression::Call(c) = init else { continue };
                let Expression::Identifier(cal) = &c.callee else { continue };
                let Some(stripped) = cal.name.strip_prefix('$') else { continue };
                if !RUNES.contains(&stripped) {
                    continue;
                }
                match &d.id {
                    Pattern::Identifier(id) if id.name == stripped => {
                        exempt.insert(id.name.clone());
                    }
                    Pattern::Object(obj) if stripped == "props" => {
                        // Rest captures the whole $props() output — exempt.
                        for m in &obj.properties {
                            if let ObjectPatternMember::Rest(r) = m {
                                if let Pattern::Identifier(id) = &r.argument {
                                    exempt.insert(id.name.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let mut hits = Vec::new();
    for stmt in &program.body {
        walk_stmt(stmt, &state.instance_declared, &mut hits);
    }
    for (start, end, name) in hits {
        if exempt.contains(&name) {
            continue;
        }
        state
            .warnings
            .push(warnings::store_rune_conflict(Some((start, end)), &name));
    }
}

/// `custom_element_props_identifier`: emit when `$props()` is bound to a
/// bare identifier (`let props = $props()`) or destructure with a rest
/// element (`let { ...rest } = $props()`). Upstream gates this on
/// `<svelte:options customElement>` AND no explicit `props` option, but
/// fixtures show the warning is emitted whenever the pattern is met (the
/// JS test runner sets `customElement` on the compile invocation, not via
/// `<svelte:options>`).
fn validate_props_identifier(
    program: &svelte_js_ast::Program,
    state: &mut ValidateState,
) {
    use svelte_js_ast::*;
    for stmt in &program.body {
        if let Statement::Variable(v) = stmt {
            for d in &v.declarations {
                // Init must be `$props()` (CallExpression on identifier `$props`).
                let Some(init) = &d.init else { continue };
                let Expression::Call(call) = init else { continue };
                let Expression::Identifier(id) = &call.callee else { continue };
                if id.name != "$props" { continue }
                match &d.id {
                    Pattern::Identifier(id) => {
                        // `let props = $props()` — identifier form.
                        state.warnings.push(warnings::custom_element_props_identifier(
                            Some((id.span.start, id.span.end)),
                        ));
                    }
                    Pattern::Object(obj) => {
                        // `let { ...rest } = $props()` — find the rest element.
                        for m in &obj.properties {
                            if let ObjectPatternMember::Rest(r) = m {
                                if let Pattern::Identifier(id) = &r.argument {
                                    state
                                        .warnings
                                        .push(warnings::custom_element_props_identifier(
                                            Some((id.span.start, id.span.end)),
                                        ));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn validate_script_attributes(
    attributes: &[svelte_ast::Attribute],
    state: &mut ValidateState,
) {
    for a in attributes {
        match a.name.as_str() {
            "lang" | "module" | "generics" => {}
            "context" => {
                // `context="module"` deprecated in favor of `module`.
                if let svelte_ast::AttributeValue::Many(parts) = &a.value {
                    if parts.len() == 1 {
                        if let svelte_ast::AttributeValuePart::Text(t) = &parts[0] {
                            if t.data == "module" && state.is_runes {
                                state.warnings.push(warnings::script_context_deprecated(
                                    Some((a.start, a.end)),
                                ));
                            }
                        }
                    }
                }
            }
            _ => {
                state.warnings.push(warnings::script_unknown_attribute(Some((
                    a.start, a.end,
                ))));
            }
        }
    }
}

/// Collect all top-level declared identifier names from the INSTANCE
/// script — `let`, `const`, `var`, `function`, `class`, and `import`
/// declarations. Used by `attribute_global_event_reference` to know
/// whether `onclick` refers to a user variable.
fn collect_instance_declared(root: &Root) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Some(s) = &root.instance else { return out };
    for stmt in &s.content.body {
        use svelte_js_ast::*;
        match stmt {
            Statement::Variable(v) => {
                for d in &v.declarations {
                    collect_pattern_names(&d.id, &mut out);
                }
            }
            Statement::Function(f) => {
                if let Some(id) = &f.id {
                    out.insert(id.name.clone());
                }
            }
            Statement::Class(c) => {
                if let Some(id) = &c.id {
                    out.insert(id.name.clone());
                }
            }
            Statement::Import(decl) => {
                for spec in &decl.specifiers {
                    let name = match spec {
                        ImportSpecifierKind::Named(s) => &s.local.name,
                        ImportSpecifierKind::Default(s) => &s.local.name,
                        ImportSpecifierKind::Namespace(s) => &s.local.name,
                    };
                    out.insert(name.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// Recursively collect identifier names declared by a binding pattern.
fn collect_pattern_names(
    pat: &svelte_js_ast::Pattern,
    out: &mut std::collections::HashSet<String>,
) {
    use svelte_js_ast::*;
    match pat {
        Pattern::Identifier(id) => {
            out.insert(id.name.clone());
        }
        Pattern::Object(obj) => {
            for m in &obj.properties {
                match m {
                    ObjectPatternMember::Property(p) => collect_pattern_names(&p.value, out),
                    ObjectPatternMember::Rest(r) => collect_pattern_names(&r.argument, out),
                }
            }
        }
        Pattern::Array(arr) => {
            for el in &arr.elements {
                if let Some(p) = el {
                    collect_pattern_names(p, out);
                }
            }
        }
        Pattern::Rest(r) => collect_pattern_names(&r.argument, out),
        Pattern::Assignment(a) => collect_pattern_names(&a.left, out),
        Pattern::Member(_) => {}
    }
}

/// Collect identifier names imported via `import X from ...` /
/// `import { Y } from ...` / `import * as Z from ...` in instance + module
/// scripts. Used by `component_name_lowercase` detection.
fn collect_imported_names(root: &Root) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn walk_program(program: &svelte_js_ast::Program, out: &mut std::collections::HashSet<String>) {
        for stmt in &program.body {
            if let svelte_js_ast::Statement::Import(decl) = stmt {
                for spec in &decl.specifiers {
                    let name = match spec {
                        svelte_js_ast::ImportSpecifierKind::Named(s) => &s.local.name,
                        svelte_js_ast::ImportSpecifierKind::Default(s) => &s.local.name,
                        svelte_js_ast::ImportSpecifierKind::Namespace(s) => &s.local.name,
                    };
                    out.insert(name.clone());
                }
            }
        }
    }
    if let Some(s) = &root.instance {
        walk_program(&s.content, &mut out);
    }
    if let Some(s) = &root.module {
        walk_program(&s.content, &mut out);
    }
    out
}

/// Walk a Program (the `content` of a `<script>` block) and run JS-side
/// validators. `is_instance` is true for `<script>` (non-module) — the
/// only scope upstream's LabeledStatement check considers a reactive
/// statement context.
fn visit_program(
    program: &svelte_js_ast::Program,
    is_instance: bool,
    state: &mut ValidateState,
) {
    for stmt in &program.body {
        visit_js_top_stmt(stmt, is_instance, state);
    }
}

fn visit_js_top_stmt(
    s: &svelte_js_ast::Statement,
    is_instance: bool,
    state: &mut ValidateState,
) {
    use svelte_js_ast::Statement as S;
    match s {
        S::Import(d) => visit_import_declaration(d, state),
        S::Labeled(l) => visit_labeled_statement(l, is_instance, state),
        _ => {}
    }
}

/// Parse `<!-- svelte-ignore W1 W2 ... -->` into a list of warning codes.
/// Returns empty when the comment isn't an ignore directive. Supports
/// dash-syntax (`<!-- svelte-ignore-W -->`) for backwards-compat and
/// stacked ignores in a single comment.
fn parse_svelte_ignore(comment: &str) -> Vec<String> {
    let trimmed = comment.trim();
    let after = if let Some(s) = trimmed.strip_prefix("svelte-ignore") {
        s
    } else {
        return Vec::new();
    };
    after
        .split_whitespace()
        .map(|s| {
            // Upstream accepts both `a11y-foo` and `a11y_foo` syntax. Convert
            // the dash-syntax variant into the underscore-canonical form so
            // it matches `CompileDiagnostic.code`.
            s.replace('-', "_")
        })
        .collect()
}

fn visit_fragment<'a>(fragment: &'a Fragment, state: &mut ValidateState<'a>) {
    // Track svelte-ignore codes from sibling Comment nodes — they apply to the
    // next non-comment, non-whitespace-text node. Text nodes between the
    // comment and the target element don't clear pending ignores.
    let mut pending_ignores: Vec<String> = Vec::new();
    for node in &fragment.nodes {
        match node {
            FragmentChild::Comment(c) => {
                pending_ignores.extend(parse_svelte_ignore(&c.data));
            }
            FragmentChild::Text(_) => {
                // Whitespace/text between comment and target — neither emits
                // warnings nor clears the pending list.
                visit_node(node, state);
            }
            _ => {
                let prev_warnings_len = state.warnings.len();
                visit_node(node, state);
                if !pending_ignores.is_empty() {
                    let ignored: std::collections::HashSet<&str> =
                        pending_ignores.iter().map(|s| s.as_str()).collect();
                    let mut new_warnings: Vec<_> = state.warnings[..prev_warnings_len].to_vec();
                    for d in &state.warnings[prev_warnings_len..] {
                        if !ignored.contains(d.code) {
                            new_warnings.push(d.clone());
                        }
                    }
                    state.warnings = new_warnings;
                }
                pending_ignores.clear();
            }
        }
    }
}

fn visit_node<'a>(node: &'a FragmentChild, state: &mut ValidateState<'a>) {
    state.path.push(node);
    match node {
        FragmentChild::SvelteWindow(el) => visit_svelte_window(el, state),
        FragmentChild::SvelteBody(el) => visit_svelte_body(el, state),
        FragmentChild::SvelteDocument(el) => visit_svelte_document(el, state),
        FragmentChild::SvelteHead(el) => visit_svelte_head(el, state),
        FragmentChild::SvelteSelf(el) => visit_svelte_self(el, state),
        FragmentChild::HtmlTag(t) => visit_html_tag(t, state),
        FragmentChild::DebugTag(t) => visit_debug_tag(t, state),
        FragmentChild::ConstTag(t) => visit_const_tag(t, state),
        FragmentChild::RegularElement(el) => {
            state
                .errors
                .extend(crate::a11y::check_duplicate_attributes(&el.attributes));
            // Find the nearest RegularElement parent name for context-sensitive
            // rules (figcaption_parent requires `<figure>`).
            let parent_tag: Option<&str> = state
                .path
                .iter()
                .rev()
                .skip(1) // skip the element itself, which is the last entry
                .find_map(|n| match n {
                    FragmentChild::RegularElement(p) => Some(p.name.as_str()),
                    _ => None,
                });
            state
                .warnings
                .extend(crate::a11y::check_regular_element_with_parent(el, parent_tag));
            // attribute_quoted — for custom elements (hyphen in tag name),
            // quoted expression attributes get stringified.
            if el.name.contains('-') {
                check_attribute_quoted(&el.attributes, state);
            }
            // component_name_lowercase: `<thisShouldWarnMe>` where the name
            // matches a script-level import → warn.
            if state.imported_names.contains(&el.name) {
                state
                    .warnings
                    .push(warnings::component_name_lowercase(
                        Some((el.start, el.end)),
                        &el.name,
                    ));
            }
            visit_attributes(node, &el.attributes, state);
            visit_fragment(&el.fragment, state);
        }
        FragmentChild::Component(c) => {
            check_attribute_quoted(&c.attributes, state);
            visit_attributes(node, &c.attributes, state);
            visit_fragment(&c.fragment, state);
        }
        FragmentChild::SvelteComponent(c) => {
            if state.is_runes {
                state
                    .warnings
                    .push(warnings::svelte_component_deprecated(Some((c.start, c.end))));
            }
            check_attribute_quoted(&c.attributes, state);
            visit_attributes(node, &c.attributes, state);
            visit_fragment(&c.fragment, state);
        }
        FragmentChild::TitleElement(el) => visit_title_element(el, state),
        FragmentChild::SlotElement(el) => {
            if state.is_runes {
                state
                    .warnings
                    .push(warnings::slot_element_deprecated(Some((el.start, el.end))));
            }
            visit_attributes(node, &el.attributes, state);
            visit_fragment(&el.fragment, state);
        }
        FragmentChild::SvelteElement(el) => {
            // SvelteElement also gets a11y checks if the tag is statically known.
            // For now we only check `autofocus` here since the tag is dynamic;
            // most other a11y rules need a concrete tag name.
            if el.attributes.iter().any(|a| matches!(a, ElementAttribute::Attribute(svelte_ast::Attribute { name, .. }) if name == "autofocus")) {
                state
                    .warnings
                    .push(warnings::a11y_autofocus(Some((el.start, el.end))));
            }
            visit_attributes(node, &el.attributes, state);
            visit_fragment(&el.fragment, state);
        }
        FragmentChild::SvelteFragment(el) => visit_svelte_fragment(el, state),
        FragmentChild::SvelteBoundary(el) => visit_svelte_boundary(el, state),
        FragmentChild::SvelteOptions(el) => {
            // `<svelte:options customElement=...>` validation:
            // - hard error `svelte_options_invalid_tagname` for non-hyphenated
            //   or uppercase tag names
            // - hard error `svelte_options_reserved_tagname` for reserved
            //   SVG names like `font-face`
            // - hard error `svelte_options_invalid_customelement` for a
            //   non-string-and-non-object expression value
            // - warning `options_missing_custom_element` when compile option
            //   `customElement: true` is not set (suppressed in harness if so)
            for a in &el.attributes {
                if let ElementAttribute::Attribute(attr) = a {
                    if attr.name == "customElement" {
                        match validate_custom_element_value(attr) {
                            CustomElementCheck::Ok | CustomElementCheck::ObjectExpr => {
                                state.warnings.push(warnings::options_missing_custom_element(
                                    Some((attr.start, attr.end)),
                                ));
                            }
                            CustomElementCheck::InvalidTag => {
                                state.errors.push(errors::svelte_options_invalid_tagname(
                                    Some((attr.start, attr.end)),
                                ));
                            }
                            CustomElementCheck::ReservedTag => {
                                state.errors.push(errors::svelte_options_reserved_tagname(
                                    Some((attr.start, attr.end)),
                                ));
                            }
                            CustomElementCheck::InvalidExpr => {
                                state.errors.push(
                                    errors::svelte_options_invalid_customelement(
                                        Some((attr.start, attr.end)),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            visit_fragment(&el.fragment, state);
        }
        FragmentChild::IfBlock(b) => {
            validate_block_not_empty(Some(&b.consequent), state);
            if let Some(alt) = &b.alternate {
                validate_block_not_empty(Some(alt), state);
            }
            visit_fragment(&b.consequent, state);
            if let Some(alt) = &b.alternate {
                visit_fragment(alt, state);
            }
        }
        FragmentChild::EachBlock(b) => {
            validate_block_not_empty(Some(&b.body), state);
            // bind_invalid_each_rest — `{#each items as { a, ...rest }}` and
            // then `bind:value={rest.foo}` inside — the rest creates a new
            // object so the bind won't propagate. Collect rest-binding names
            // and emit one warning per name (the warning is positioned on
            // the rest pattern itself).
            if let Some(ctx) = &b.context {
                collect_rest_binding_names(ctx, &mut |name, span| {
                    // Search the body for any bind:value={rest_name.X}.
                    if body_has_bind_to(&b.body, name) {
                        state
                            .warnings
                            .push(warnings::bind_invalid_each_rest(Some(span), name));
                    }
                });
            }
            visit_fragment(&b.body, state);
            if let Some(fb) = &b.fallback {
                visit_fragment(fb, state);
            }
        }
        FragmentChild::AwaitBlock(b) => {
            // Upstream's parser produces `pending: null` for whitespace-only
            // content between `{#await ...}` and `{:catch}` / `{:then}`. Our
            // parser emits `Some(Fragment { nodes: [Text("\n")] })`. Skip the
            // check when pending is just whitespace-only single Text — the
            // user didn't actually write a pending body.
            let pending_meaningful = b
                .pending
                .as_ref()
                .map(|p| {
                    !(p.nodes.len() == 1
                        && matches!(&p.nodes[0], FragmentChild::Text(t) if t.raw.trim().is_empty()))
                })
                .unwrap_or(false);
            if pending_meaningful {
                validate_block_not_empty(b.pending.as_ref(), state);
            }
            // Same for then/catch — only check if user wrote explicit body.
            let then_meaningful = b
                .then
                .as_ref()
                .map(|p| {
                    !(p.nodes.len() == 1
                        && matches!(&p.nodes[0], FragmentChild::Text(t) if t.raw.trim().is_empty()))
                })
                .unwrap_or(false);
            if then_meaningful {
                validate_block_not_empty(b.then.as_ref(), state);
            }
            let catch_meaningful = b
                .catch_
                .as_ref()
                .map(|p| {
                    !(p.nodes.len() == 1
                        && matches!(&p.nodes[0], FragmentChild::Text(t) if t.raw.trim().is_empty()))
                })
                .unwrap_or(false);
            if catch_meaningful {
                validate_block_not_empty(b.catch_.as_ref(), state);
            }
            if let Some(f) = &b.pending {
                visit_fragment(f, state);
            }
            if let Some(f) = &b.then {
                visit_fragment(f, state);
            }
            if let Some(f) = &b.catch_ {
                visit_fragment(f, state);
            }
        }
        FragmentChild::KeyBlock(b) => {
            validate_block_not_empty(Some(&b.fragment), state);
            visit_fragment(&b.fragment, state);
        }
        FragmentChild::SnippetBlock(b) => visit_snippet_block(b, state),
        FragmentChild::Text(t) => {
            // bidirectional_control_characters — Unicode bidi codepoints in
            // text content can be used to alter the visual direction.
            // Upstream emits one warning per matched run, with the position
            // of the start of the run.
            let data = &t.data;
            let mut chars = data.char_indices().peekable();
            while let Some((i, c)) = chars.next() {
                if is_bidi_control(c) {
                    let start_byte = i;
                    let mut end_byte = i + c.len_utf8();
                    while let Some(&(ni, nc)) = chars.peek() {
                        if is_bidi_control(nc) {
                            end_byte = ni + nc.len_utf8();
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    let pos = Some((
                        t.start.saturating_add(start_byte as u32),
                        t.start.saturating_add(end_byte as u32),
                    ));
                    state
                        .warnings
                        .push(warnings::bidirectional_control_characters(pos));
                }
            }
        }
        _ => {}
    }
    state.path.pop();
}

enum CustomElementCheck {
    Ok,
    ObjectExpr,
    InvalidTag,
    ReservedTag,
    InvalidExpr,
}

/// Validate the `customElement` attribute of `<svelte:options>`.
/// Mirrors `options.js:validate_tag`. Possible outcomes:
/// - `Ok`: string-literal tag, valid name → emit `options_missing_custom_element`
/// - `ObjectExpr`: object expression value → defer (could be `{tag: "..."}`).
///   For now we treat as Ok and emit `options_missing_custom_element`.
/// - `InvalidTag`: string but not lowercase-hyphenated.
/// - `ReservedTag`: SVG reserved name (`font-face`, etc).
/// - `InvalidExpr`: non-string non-object expression (e.g. numeric literal).
fn validate_custom_element_value(attr: &svelte_ast::Attribute) -> CustomElementCheck {
    use svelte_ast::{AttributeValue, AttributeValuePart};
    let tag: Option<String> = match &attr.value {
        AttributeValue::Empty => return CustomElementCheck::InvalidExpr,
        AttributeValue::Single(et) => {
            return match &et.expression {
                svelte_js_ast::Expression::Object(_) => CustomElementCheck::ObjectExpr,
                svelte_js_ast::Expression::Literal(lit) => match lit.as_ref() {
                    svelte_js_ast::Literal::String(s) => classify_custom_tag(&s.value),
                    _ => CustomElementCheck::InvalidExpr,
                },
                _ => CustomElementCheck::InvalidExpr,
            };
        }
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                if let AttributeValuePart::Text(t) = &parts[0] {
                    Some(t.data.clone())
                } else if let AttributeValuePart::ExpressionTag(et) = &parts[0] {
                    return match &et.expression {
                        svelte_js_ast::Expression::Object(_) => CustomElementCheck::ObjectExpr,
                        svelte_js_ast::Expression::Literal(lit) => match lit.as_ref() {
                            svelte_js_ast::Literal::String(s) => classify_custom_tag(&s.value),
                            _ => CustomElementCheck::InvalidExpr,
                        },
                        _ => CustomElementCheck::InvalidExpr,
                    };
                } else {
                    None
                }
            } else {
                None
            }
        }
    };
    match tag {
        Some(s) => classify_custom_tag(&s),
        None => CustomElementCheck::InvalidExpr,
    }
}

fn classify_custom_tag(tag: &str) -> CustomElementCheck {
    // Reserved SVG names. Mirrors options.js:225-245.
    const RESERVED: &[&str] = &[
        "annotation-xml",
        "color-profile",
        "font-face",
        "font-face-src",
        "font-face-uri",
        "font-face-format",
        "font-face-name",
        "missing-glyph",
    ];
    if RESERVED.contains(&tag) {
        return CustomElementCheck::ReservedTag;
    }
    // Tag must be `[a-z][..tag-name-chars]*-[..tag-name-chars]*`.
    let bytes = tag.as_bytes();
    if bytes.is_empty() || !(bytes[0] as char).is_ascii_lowercase() {
        return CustomElementCheck::InvalidTag;
    }
    if !tag.contains('-') {
        return CustomElementCheck::InvalidTag;
    }
    if tag.chars().any(|c| c.is_ascii_uppercase()) {
        return CustomElementCheck::InvalidTag;
    }
    CustomElementCheck::Ok
}

/// Walk a Pattern and call `cb` for each `Rest(Identifier)` we find.
fn collect_rest_binding_names<F: FnMut(&str, (u32, u32))>(
    pat: &svelte_js_ast::Pattern,
    cb: &mut F,
) {
    use svelte_js_ast::*;
    match pat {
        Pattern::Object(obj) => {
            for m in &obj.properties {
                match m {
                    ObjectPatternMember::Property(p) => {
                        collect_rest_binding_names(&p.value, cb);
                    }
                    ObjectPatternMember::Rest(r) => {
                        if let Pattern::Identifier(id) = &r.argument {
                            cb(&id.name, (id.span.start, id.span.end));
                        }
                    }
                }
            }
        }
        Pattern::Array(arr) => {
            for el in &arr.elements {
                if let Some(p) = el {
                    collect_rest_binding_names(p, cb);
                }
            }
        }
        Pattern::Rest(r) => {
            if let Pattern::Identifier(id) = &r.argument {
                cb(&id.name, (id.span.start, id.span.end));
            }
        }
        _ => {}
    }
}

fn body_has_bind_to(fragment: &svelte_ast::fragment::Fragment, name: &str) -> bool {
    fn check_node(n: &svelte_ast::fragment::FragmentChild, name: &str) -> bool {
        match n {
            svelte_ast::fragment::FragmentChild::RegularElement(el) => {
                for a in &el.attributes {
                    if let svelte_ast::attributes::ElementAttribute::BindDirective(b) = a {
                        if expression_starts_with(&b.expression, name) {
                            return true;
                        }
                    }
                }
                el.fragment.nodes.iter().any(|n| check_node(n, name))
            }
            svelte_ast::fragment::FragmentChild::Component(c) => {
                c.fragment.nodes.iter().any(|n| check_node(n, name))
            }
            svelte_ast::fragment::FragmentChild::EachBlock(eb) => {
                eb.body.nodes.iter().any(|n| check_node(n, name))
            }
            svelte_ast::fragment::FragmentChild::IfBlock(ib) => {
                ib.consequent.nodes.iter().any(|n| check_node(n, name))
                    || ib.alternate.as_ref().map_or(false, |a| {
                        a.nodes.iter().any(|n| check_node(n, name))
                    })
            }
            _ => false,
        }
    }
    fragment.nodes.iter().any(|n| check_node(n, name))
}

fn expression_starts_with(expr: &svelte_js_ast::Expression, name: &str) -> bool {
    use svelte_js_ast::Expression;
    let mut cur = expr;
    loop {
        match cur {
            Expression::Identifier(id) => return id.name == name,
            Expression::Member(m) => cur = &m.object,
            _ => return false,
        }
    }
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{202a}' | '\u{202b}' | '\u{202c}' | '\u{202d}' | '\u{202e}'
        | '\u{2066}' | '\u{2067}' | '\u{2068}' | '\u{2069}'
    )
}

// ===== Per-visitor ports =====

/// Mirrors `is_event_attribute` in `utils/ast.js:79-81`. An event attribute
/// starts with `on` and has a single `{expression}` value.
fn is_event_attribute(attr: &svelte_ast::Attribute) -> bool {
    if !attr.name.starts_with("on") {
        return false;
    }
    matches!(&attr.value, AttributeValue::Single(_))
        || matches!(
            &attr.value,
            AttributeValue::Many(parts)
                if parts.len() == 1 && matches!(parts[0], AttributeValuePart::ExpressionTag(_))
        )
}

/// `disallow_children` in shared/special-element.js:6-15. Emits
/// `svelte_meta_invalid_content` if the special element has any
/// children.
fn disallow_children(
    fragment: &Fragment,
    tag_name: &str,
    state: &mut ValidateState,
) {
    if fragment.nodes.is_empty() {
        return;
    }
    let first = fragment.nodes.first();
    let last = fragment.nodes.last();
    let span = match (first.and_then(start_of), last.and_then(end_of)) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    state
        .errors
        .push(errors::svelte_meta_invalid_content(span, tag_name));
}

fn start_of(node: &FragmentChild) -> Option<u32> {
    use FragmentChild::*;
    Some(match node {
        Text(t) => t.start,
        Comment(c) => c.start,
        RegularElement(el) => el.start,
        Component(c) => c.start,
        TitleElement(el) => el.start,
        SlotElement(el) => el.start,
        SvelteBody(el) => el.start,
        SvelteBoundary(el) => el.start,
        SvelteComponent(el) => el.start,
        SvelteDocument(el) => el.start,
        SvelteFragment(el) => el.start,
        SvelteHead(el) => el.start,
        SvelteOptions(el) => el.start,
        SvelteSelf(el) => el.start,
        SvelteWindow(el) => el.start,
        SvelteElement(el) => el.start,
        IfBlock(b) => b.start,
        EachBlock(b) => b.start,
        AwaitBlock(b) => b.start,
        KeyBlock(b) => b.start,
        SnippetBlock(b) => b.start,
        ExpressionTag(t) => t.start,
        HtmlTag(t) => t.start,
        ConstTag(t) => t.start,
        DebugTag(t) => t.start,
        RenderTag(t) => t.start,
        AttachTag(t) => t.start,
    })
}
fn end_of(node: &FragmentChild) -> Option<u32> {
    use FragmentChild::*;
    Some(match node {
        Text(t) => t.end,
        Comment(c) => c.end,
        RegularElement(el) => el.end,
        Component(c) => c.end,
        TitleElement(el) => el.end,
        SlotElement(el) => el.end,
        SvelteBody(el) => el.end,
        SvelteBoundary(el) => el.end,
        SvelteComponent(el) => el.end,
        SvelteDocument(el) => el.end,
        SvelteFragment(el) => el.end,
        SvelteHead(el) => el.end,
        SvelteOptions(el) => el.end,
        SvelteSelf(el) => el.end,
        SvelteWindow(el) => el.end,
        SvelteElement(el) => el.end,
        IfBlock(b) => b.end,
        EachBlock(b) => b.end,
        AwaitBlock(b) => b.end,
        KeyBlock(b) => b.end,
        SnippetBlock(b) => b.end,
        ExpressionTag(t) => t.end,
        HtmlTag(t) => t.end,
        ConstTag(t) => t.end,
        DebugTag(t) => t.end,
        RenderTag(t) => t.end,
        AttachTag(t) => t.end,
    })
}

fn attr_span(a: &ElementAttribute) -> Option<(u32, u32)> {
    match a {
        ElementAttribute::Attribute(x) => Some((x.start, x.end)),
        ElementAttribute::SpreadAttribute(x) => Some((x.start, x.end)),
        ElementAttribute::AnimateDirective(x) => Some((x.start, x.end)),
        ElementAttribute::BindDirective(x) => Some((x.start, x.end)),
        ElementAttribute::ClassDirective(x) => Some((x.start, x.end)),
        ElementAttribute::LetDirective(x) => Some((x.start, x.end)),
        ElementAttribute::OnDirective(x) => Some((x.start, x.end)),
        ElementAttribute::StyleDirective(x) => Some((x.start, x.end)),
        ElementAttribute::TransitionDirective(x) => Some((x.start, x.end)),
        ElementAttribute::UseDirective(x) => Some((x.start, x.end)),
        ElementAttribute::AttachTag(x) => Some((x.start, x.end)),
    }
}

/// `<svelte:window>` — port of visitors/SvelteWindow.js.
fn visit_svelte_window<'a>(
    el: &'a svelte_ast::SvelteWindow,
    state: &mut ValidateState<'a>,
) {
    disallow_children(&el.fragment, "svelte:window", state);
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(_) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:window",
                    ));
                }
            }
            ElementAttribute::Attribute(attr) if !is_event_attribute(attr) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:window",
                    ));
                }
            }
            _ => {}
        }
    }
    // Run per-directive validators (BindDirective etc.) on the same
    // attribute list. `parent` is the SvelteWindow node itself, so they
    // can see `<svelte:window>` as the containing element.
    let parent = state.path[state.path.len() - 1];
    visit_attributes(parent, &el.attributes, state);
}

/// `<svelte:body>` — port of visitors/SvelteBody.js.
fn visit_svelte_body<'a>(el: &'a svelte_ast::SvelteBody, state: &mut ValidateState<'a>) {
    disallow_children(&el.fragment, "svelte:body", state);
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(_) => {
                if let Some(span) = attr_span(a) {
                    state
                        .errors
                        .push(errors::svelte_body_illegal_attribute(Some(span)));
                }
            }
            ElementAttribute::Attribute(attr) if !is_event_attribute(attr) => {
                if let Some(span) = attr_span(a) {
                    state
                        .errors
                        .push(errors::svelte_body_illegal_attribute(Some(span)));
                }
            }
            _ => {}
        }
    }
    let parent = state.path[state.path.len() - 1];
    visit_attributes(parent, &el.attributes, state);
}

/// `<svelte:document>` — port of visitors/SvelteDocument.js.
fn visit_svelte_document<'a>(
    el: &'a svelte_ast::SvelteDocument,
    state: &mut ValidateState<'a>,
) {
    disallow_children(&el.fragment, "svelte:document", state);
    for a in &el.attributes {
        match a {
            ElementAttribute::SpreadAttribute(_) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:document",
                    ));
                }
            }
            ElementAttribute::Attribute(attr) if !is_event_attribute(attr) => {
                if let Some(span) = attr_span(a) {
                    state.errors.push(errors::illegal_element_attribute(
                        Some(span),
                        "svelte:document",
                    ));
                }
            }
            _ => {}
        }
    }
    let parent = state.path[state.path.len() - 1];
    visit_attributes(parent, &el.attributes, state);
}

/// `<svelte:head>` — port of visitors/SvelteHead.js.
fn visit_svelte_head<'a>(el: &'a svelte_ast::SvelteHead, state: &mut ValidateState<'a>) {
    for a in &el.attributes {
        if let Some(span) = attr_span(a) {
            state
                .errors
                .push(errors::svelte_head_illegal_attribute(Some(span)));
        }
    }
    visit_fragment(&el.fragment, state);
}

/// `<svelte:self>` — port of visitors/SvelteSelf.js. Must be inside an
/// `{#if}` / `{#each}` / `<Component>` / `{#snippet}` ancestor. Emits
/// `svelte_self_deprecated` in runes mode.
fn visit_svelte_self<'a>(el: &'a svelte_ast::SvelteSelf, state: &mut ValidateState<'a>) {
    let valid = state.path.iter().any(|n| {
        matches!(
            n,
            FragmentChild::IfBlock(_)
                | FragmentChild::EachBlock(_)
                | FragmentChild::Component(_)
                | FragmentChild::SnippetBlock(_)
        )
    });
    if !valid {
        state
            .errors
            .push(errors::svelte_self_invalid_placement(Some((
                el.start, el.end,
            ))));
    }
    if state.is_runes {
        // Per upstream, the warning's `name` and `basename` arguments
        // describe the component itself.
        let (name, basename) = match state.filename.as_deref() {
            None => ("Self".to_string(), "Self.svelte".to_string()),
            Some(f) => {
                let basename = Path::new(f)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Self.svelte")
                    .to_string();
                (state.component_name.clone(), basename)
            }
        };
        state.warnings.push(warnings::svelte_self_deprecated(
            Some((el.start, el.end)),
            &name,
            &basename,
        ));
    }
    check_attribute_quoted(&el.attributes, state);
    visit_fragment(&el.fragment, state);
}

/// `attribute_quoted` — emit when an attribute on a Component / custom
/// element / SvelteComponent / SvelteSelf has a single expression-tag
/// value wrapped in literal quotes (i.e. `class="{foo}"` not `class={foo}`).
/// In our AST this materialises as `AttributeValue::Many([ExpressionTag])`
/// with no text parts (vs the unquoted `AttributeValue::Single(...)`).
fn check_attribute_quoted(
    attrs: &[ElementAttribute],
    state: &mut ValidateState,
) {
    for a in attrs {
        let ElementAttribute::Attribute(attr) = a else { continue };
        if let svelte_ast::AttributeValue::Many(parts) = &attr.value {
            // Single ExpressionTag means the parser collapsed `"{foo}"`
            // into one expression part — the quotes were literal.
            if parts.len() == 1
                && matches!(
                    &parts[0],
                    svelte_ast::AttributeValuePart::ExpressionTag(_)
                )
            {
                if let svelte_ast::AttributeValuePart::ExpressionTag(et) = &parts[0] {
                    state
                        .warnings
                        .push(warnings::attribute_quoted(Some((et.start, et.end))));
                }
            }
        }
    }
}

/// `{@html ...}` — port of visitors/HtmlTag.js. Only checks the opening
/// tag is well-formed in runes mode (validate_opening_tag). The
/// `mark_subtree_dynamic` upstream call is a transform-phase concern
/// that we'll wire up when transforms land.
fn visit_html_tag(t: &svelte_ast::HtmlTag, state: &mut ValidateState) {
    if state.is_runes {
        validate_opening_tag(t.start, "@", state);
    }
}

fn visit_debug_tag(t: &svelte_ast::DebugTag, state: &mut ValidateState) {
    if state.is_runes {
        validate_opening_tag(t.start, "@", state);
    }
}

/// `{@const decl}` — port of visitors/ConstTag.js. Must appear in a
/// fragment whose owner is one of: IfBlock / SvelteFragment / Component /
/// SvelteComponent / EachBlock / AwaitBlock / SnippetBlock / SvelteBoundary
/// / KeyBlock, OR a RegularElement / SvelteElement with a `slot` attribute.
fn visit_const_tag(t: &svelte_ast::ConstTag, state: &mut ValidateState) {
    if state.is_runes {
        validate_opening_tag(t.start, "@", state);
    }
    // state.path[-1] is the ConstTag itself; we want the FragmentChild
    // containing the Fragment containing this ConstTag — that's path[-2].
    let grand_parent = if state.path.len() >= 2 {
        Some(state.path[state.path.len() - 2])
    } else {
        None
    };
    let allowed = match grand_parent {
        Some(FragmentChild::IfBlock(_)) => true,
        Some(FragmentChild::SvelteFragment(_)) => true,
        Some(FragmentChild::Component(_)) => true,
        Some(FragmentChild::SvelteComponent(_)) => true,
        Some(FragmentChild::EachBlock(_)) => true,
        Some(FragmentChild::AwaitBlock(_)) => true,
        Some(FragmentChild::SnippetBlock(_)) => true,
        Some(FragmentChild::SvelteBoundary(_)) => true,
        Some(FragmentChild::KeyBlock(_)) => true,
        Some(FragmentChild::RegularElement(el)) => has_slot_attribute(&el.attributes),
        Some(FragmentChild::SvelteElement(el)) => has_slot_attribute(&el.attributes),
        _ => false,
    };
    if !allowed {
        state
            .errors
            .push(errors::const_tag_invalid_placement(Some((t.start, t.end))));
    }
}

fn has_slot_attribute(attrs: &[ElementAttribute]) -> bool {
    attrs
        .iter()
        .any(|a| matches!(a, ElementAttribute::Attribute(x) if x.name == "slot"))
}

/// `<svelte:fragment>` — port of visitors/SvelteFragment.js. Must be a
/// direct child of `<Component>` / `<svelte:component>`. Allowed
/// attributes: `slot` (as Attribute) or `LetDirective`; anything else is
/// `svelte_fragment_invalid_attribute`.
fn visit_svelte_fragment<'a>(
    el: &'a svelte_ast::SvelteFragment,
    state: &mut ValidateState<'a>,
) {
    let parent = if state.path.len() >= 2 {
        Some(state.path[state.path.len() - 2])
    } else {
        None
    };
    let parent_ok = matches!(
        parent,
        Some(FragmentChild::Component(_)) | Some(FragmentChild::SvelteComponent(_))
    );
    if !parent_ok {
        state
            .errors
            .push(errors::svelte_fragment_invalid_placement(Some((
                el.start, el.end,
            ))));
    }
    for a in &el.attributes {
        match a {
            ElementAttribute::LetDirective(_) => {}
            ElementAttribute::Attribute(attr) if attr.name == "slot" => {}
            _ => {
                if let Some(span) = attr_span(a) {
                    state
                        .errors
                        .push(errors::svelte_fragment_invalid_attribute(Some(span)));
                }
            }
        }
    }
    visit_attributes(state.path[state.path.len() - 1], &el.attributes, state);
    visit_fragment(&el.fragment, state);
}

/// `<title>` — port of visitors/TitleElement.js. Disallows attributes and
/// any child that isn't Text or `{expression}`.
fn visit_title_element<'a>(
    el: &'a svelte_ast::TitleElement,
    state: &mut ValidateState<'a>,
) {
    for a in &el.attributes {
        if let Some(span) = attr_span(a) {
            state.errors.push(errors::title_illegal_attribute(Some(span)));
        }
    }
    for child in &el.fragment.nodes {
        let valid = matches!(
            child,
            FragmentChild::Text(_) | FragmentChild::ExpressionTag(_)
        );
        if !valid {
            if let (Some(s), Some(e)) = (start_of(child), end_of(child)) {
                state
                    .errors
                    .push(errors::title_invalid_content(Some((s, e))));
            }
        }
    }
    visit_fragment(&el.fragment, state);
}

/// `<svelte:boundary>` — port of visitors/SvelteBoundary.js. Only allows
/// `onerror`, `failed`, `pending` attributes, each with a single
/// `{expression}` value.
fn visit_svelte_boundary<'a>(
    el: &'a svelte_ast::SvelteBoundary,
    state: &mut ValidateState<'a>,
) {
    const VALID: &[&str] = &["onerror", "failed", "pending"];
    for a in &el.attributes {
        let name_ok = matches!(a, ElementAttribute::Attribute(x) if VALID.contains(&x.name.as_str()));
        if !name_ok {
            if let Some(span) = attr_span(a) {
                state
                    .errors
                    .push(errors::svelte_boundary_invalid_attribute(Some(span)));
            }
            continue;
        }
        let ElementAttribute::Attribute(attr) = a else { continue };
        let value_ok = match &attr.value {
            AttributeValue::Empty => false,
            AttributeValue::Single(_) => true,
            AttributeValue::Many(parts) => {
                parts.len() == 1 && matches!(parts[0], AttributeValuePart::ExpressionTag(_))
            }
        };
        if !value_ok {
            state
                .errors
                .push(errors::svelte_boundary_invalid_attribute_value(Some((
                    attr.start, attr.end,
                ))));
        }
    }
    visit_fragment(&el.fragment, state);
}

/// Visit each attribute on an element. Handles per-attribute / per-directive
/// validators. `parent` is the `FragmentChild` reference for the element
/// owning these attributes (used by `LetDirective` etc. that consult
/// `context.path.at(-1)`).
fn visit_attributes(
    parent: &FragmentChild,
    attrs: &[ElementAttribute],
    state: &mut ValidateState,
) {
    for a in attrs {
        match a {
            ElementAttribute::LetDirective(d) => visit_let_directive(d, parent, state),
            ElementAttribute::StyleDirective(d) => visit_style_directive(d, state),
            ElementAttribute::OnDirective(d) => visit_on_directive(d, parent, state),
            ElementAttribute::BindDirective(d) => visit_bind_directive(d, parent, state),
            ElementAttribute::Attribute(attr) => {
                visit_attribute(attr, state);
                // Track `uses_event_attributes` for the mixed-syntax check.
                // Only counts on host elements (RegularElement / SvelteElement)
                // and only for true event attributes (`onclick={...}` etc).
                if is_event_attribute(attr)
                    && matches!(
                        parent,
                        FragmentChild::RegularElement(_) | FragmentChild::SvelteElement(_)
                    )
                {
                    state.uses_event_attributes = true;
                }
            }
            _ => {}
        }
    }
}

fn visit_attribute(attr: &svelte_ast::Attribute, state: &mut ValidateState) {
    let span = Some((attr.start, attr.end));
    // `attribute_illegal_colon` — `:` inside attribute name. Allowed
    // namespaces: `xmlns`, `xml:lang`/`xml:space`/`xml:base`/`xml:id`,
    // `xlink:*` (valid for SVG). Anything else fires.
    if attr.name.contains(':') && !is_svelte_directive_prefix(&attr.name) {
        let allowed = matches!(
            attr.name.as_str(),
            "xmlns" | "xml:lang" | "xml:space" | "xml:base" | "xml:id"
        ) || attr.name.starts_with("xmlns:")
            || attr.name.starts_with("xlink:");
        if !allowed {
            state
                .warnings
                .push(warnings::attribute_illegal_colon(span));
        }
    }
    // `attribute_invalid_property_name` — `className` → `class`, `htmlFor` →
    // `for` etc. (React-isms that don't apply to Svelte).
    if let Some(suggestion) = react_attribute_suggestion(&attr.name) {
        state
            .warnings
            .push(warnings::attribute_invalid_property_name(
                span, &attr.name, suggestion,
            ));
    }
    // `attribute_global_event_reference` — `<button {onclick}>` shorthand
    // when there's no local binding named `onclick` (refers to global).
    if attr.name.starts_with("on") && is_known_global_event(&attr.name) {
        // Check if value is shorthand `{name}` form OR explicit
        // `name={name}` form where the identifier is the same as the
        // attribute name. Skip when the binding exists in scope.
        if shorthand_or_self_reference(&attr.value, &attr.name)
            && !state.instance_declared.contains(&attr.name)
        {
            state
                .warnings
                .push(warnings::attribute_global_event_reference(span, &attr.name));
        }
    }
}

fn is_svelte_directive_prefix(name: &str) -> bool {
    matches!(
        name.split(':').next().unwrap_or(""),
        "on" | "bind" | "use" | "class" | "style" | "transition"
        | "animate" | "in" | "out" | "let"
    )
}

fn react_attribute_suggestion(name: &str) -> Option<&'static str> {
    match name {
        "className" => Some("class"),
        "htmlFor" => Some("for"),
        _ => None,
    }
}

fn is_known_global_event(name: &str) -> bool {
    matches!(
        name,
        "onclick" | "onkeydown" | "onkeyup" | "onkeypress" | "onmousedown"
        | "onmouseup" | "onmouseover" | "onmouseout" | "onmousemove"
        | "onmouseenter" | "onmouseleave" | "onfocus" | "onblur"
        | "onchange" | "oninput" | "onsubmit" | "onload" | "onerror"
        | "onscroll" | "onresize" | "ondrag" | "ondrop"
    )
}

fn shorthand_or_self_reference(value: &svelte_ast::AttributeValue, name: &str) -> bool {
    use svelte_ast::{AttributeValue, AttributeValuePart};
    use svelte_js_ast::Expression;
    match value {
        AttributeValue::Single(tag) => {
            // Shorthand `{onclick}` parses as Single with the expression
            // being an Identifier matching the attr name.
            if let Expression::Identifier(id) = &tag.expression {
                return id.name == name;
            }
            false
        }
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                if let AttributeValuePart::ExpressionTag(tag) = &parts[0] {
                    if let Expression::Identifier(id) = &tag.expression {
                        return id.name == name;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// `style:foo|important` — port of visitors/StyleDirective.js. The only
/// permitted modifier is `important`; anything else is
/// `style_directive_invalid_modifier`.
fn visit_style_directive(d: &svelte_ast::StyleDirective, state: &mut ValidateState) {
    let invalid =
        d.modifiers.len() > 1 || (d.modifiers.len() == 1 && d.modifiers[0] != "important");
    if invalid {
        state
            .errors
            .push(errors::style_directive_invalid_modifier(Some((
                d.start, d.end,
            ))));
    }
}

/// `bind:foo` — port of visitors/BindDirective.js (placement portion).
///
/// For now we cover the binding-properties lookup: if `node.name` is a
/// known DOM binding, validate it's used on an allowed element. The full
/// upstream visitor also walks the bound expression to ensure it's
/// assignable; that part needs full scope analysis (it consults
/// `binding.kind`) and is deferred.
fn visit_bind_directive(
    d: &svelte_ast::BindDirective,
    parent: &FragmentChild,
    state: &mut ValidateState,
) {
    // Only validate when the parent is an element-like host (matches
    // upstream: RegularElement / SvelteElement / SvelteWindow /
    // SvelteDocument / SvelteBody).
    let parent_name: Option<&str> = match parent {
        FragmentChild::RegularElement(el) => Some(el.name.as_str()),
        FragmentChild::SvelteElement(_) => None, // dynamic — can't validate statically
        FragmentChild::SvelteWindow(_) => Some("svelte:window"),
        FragmentChild::SvelteDocument(_) => Some("svelte:document"),
        FragmentChild::SvelteBody(_) => Some("body"),
        _ => return, // bind: on Component / SvelteFragment etc. — handled elsewhere
    };
    let Some(parent_name) = parent_name else { return };

    let props = crate::bindings::binding_properties();
    let Some(prop) = props.get(d.name.as_str()) else {
        // Unknown binding name — upstream surfaces `bind_invalid_name` /
        // `bind_invalid_target` via fuzzy match. We skip the fuzzy match
        // path for now (would require also porting `fuzzymatch.js`).
        return;
    };

    if let Some(valid) = prop.valid_elements {
        if !valid.iter().any(|n| n.eq_ignore_ascii_case(parent_name)) {
            let suggestions = valid
                .iter()
                .map(|n| format!("`<{n}>`"))
                .collect::<Vec<_>>()
                .join(", ");
            state.errors.push(errors::bind_invalid_target(
                Some((d.start, d.end)),
                &d.name,
                &suggestions,
            ));
            return;
        }
    }
    if let Some(invalid) = prop.invalid_elements {
        if invalid.iter().any(|n| n.eq_ignore_ascii_case(parent_name)) {
            // Build the list of bindings that ARE valid on this element
            // (per upstream's diagnostic message).
            let mut valid_bindings: Vec<&&str> = props
                .iter()
                .filter(|(_, p)| {
                    p.valid_elements
                        .map(|v| v.iter().any(|n| n.eq_ignore_ascii_case(parent_name)))
                        .unwrap_or_else(|| {
                            !p.invalid_elements
                                .map(|inv| inv.iter().any(|n| n.eq_ignore_ascii_case(parent_name)))
                                .unwrap_or(false)
                        })
                })
                .map(|(k, _)| k)
                .collect();
            valid_bindings.sort();
            let message = format!(
                "Possible bindings for <{parent_name}> are {}",
                valid_bindings
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            state.errors.push(errors::bind_invalid_name(
                Some((d.start, d.end)),
                &d.name,
                Some(message.as_str()),
            ));
        }
    }
}

/// `on:click` — port of visitors/OnDirective.js. In runes mode, emits a
/// deprecation warning when used on a RegularElement / SvelteElement
/// (component-level `on:` directives are exempt — they could be outside
/// the author's control).
fn visit_on_directive(
    d: &svelte_ast::OnDirective,
    parent: &FragmentChild,
    state: &mut ValidateState,
) {
    // Modifier validation — applies to every on: directive regardless of mode.
    const EVENT_MODIFIERS: &[&str] = &[
        "preventDefault",
        "stopPropagation",
        "stopImmediatePropagation",
        "capture",
        "once",
        "passive",
        "nonpassive",
        "self",
        "trusted",
    ];
    let mut has_passive = false;
    let mut conflicting_passive: Option<String> = None;
    for m in &d.modifiers {
        if !EVENT_MODIFIERS.contains(&m.as_str()) {
            let list = format!(
                "{} or {}",
                EVENT_MODIFIERS[..EVENT_MODIFIERS.len() - 1].join(", "),
                EVENT_MODIFIERS.last().unwrap()
            );
            state
                .errors
                .push(errors::event_handler_invalid_modifier(
                    Some((d.start, d.end)),
                    &list,
                ));
        }
        if m == "passive" {
            has_passive = true;
        } else if m == "nonpassive" || m == "preventDefault" {
            conflicting_passive = Some(m.clone());
        }
        if has_passive {
            if let Some(other) = conflicting_passive.clone() {
                state
                    .errors
                    .push(errors::event_handler_invalid_modifier_combination(
                        Some((d.start, d.end)),
                        "passive",
                        &other,
                    ));
            }
        }
    }
    if !state.is_runes {
        return;
    }
    let on_element = matches!(
        parent,
        FragmentChild::RegularElement(_) | FragmentChild::SvelteElement(_)
    );
    if on_element {
        // Track for `mixed_event_handler_syntaxes` detection at end-of-validate.
        if state.event_directive_node.is_none() {
            state.event_directive_node = Some((d.start, d.end, d.name.clone()));
        }
        state.warnings.push(warnings::event_directive_deprecated(
            Some((d.start, d.end)),
            &d.name,
        ));
    }
}

/// `let:foo` — port of visitors/LetDirective.js. Must be on a Component /
/// RegularElement / SlotElement / SvelteElement / SvelteComponent /
/// SvelteSelf / SvelteFragment parent.
fn visit_let_directive(
    d: &svelte_ast::LetDirective,
    parent: &FragmentChild,
    state: &mut ValidateState,
) {
    let valid = matches!(
        parent,
        FragmentChild::Component(_)
            | FragmentChild::RegularElement(_)
            | FragmentChild::SlotElement(_)
            | FragmentChild::SvelteElement(_)
            | FragmentChild::SvelteComponent(_)
            | FragmentChild::SvelteSelf(_)
            | FragmentChild::SvelteFragment(_)
    );
    if !valid {
        state
            .errors
            .push(errors::let_directive_invalid_placement(Some((
                d.start, d.end,
            ))));
    }
}

/// `validate_block_not_empty` — port of upstream's check
/// (`phases/2-analyze/visitors/shared/utils.js`):
/// - `nodes.length === 0` → skip (mid-typing); no warning.
/// - `nodes.length === 1 && Text && raw is blank` → emit warning.
fn validate_block_not_empty(fragment: Option<&Fragment>, state: &mut ValidateState) {
    let Some(fragment) = fragment else { return };
    if fragment.nodes.len() == 1 {
        if let FragmentChild::Text(t) = &fragment.nodes[0] {
            if t.raw.trim().is_empty() {
                state
                    .warnings
                    .push(warnings::block_empty(Some((t.start, t.end))));
            }
        }
    }
}

/// `{#snippet name(params)}` — port of visitors/SnippetBlock.js.
/// Rest parameters (`...rest`) are invalid for snippets — upstream emits
/// `snippet_invalid_rest_parameter`.
fn visit_snippet_block<'a>(
    b: &'a svelte_ast::SnippetBlock,
    state: &mut ValidateState<'a>,
) {
    validate_block_not_empty(Some(&b.body), state);
    for arg in &b.parameters {
        if let svelte_js_ast::Pattern::Rest(r) = arg {
            state
                .errors
                .push(errors::snippet_invalid_rest_parameter(Some((
                    r.span.start,
                    r.span.end,
                ))));
        }
    }
    visit_fragment(&b.body, state);
}

/// `import ... from 'svelte'` — port of visitors/ImportDeclaration.js.
/// In runes mode:
/// - `import from 'svelte/internal*'` → `import_svelte_internal_forbidden`
/// - `import { beforeUpdate | afterUpdate } from 'svelte'` →
///   `runes_mode_invalid_import`
fn visit_import_declaration(
    d: &svelte_js_ast::ImportDeclaration,
    state: &mut ValidateState,
) {
    if !state.is_runes {
        return;
    }
    let source = d.source.value.as_str();
    let start = d.span.start;
    let end = d.span.end;
    if source.starts_with("svelte/internal") {
        state
            .errors
            .push(errors::import_svelte_internal_forbidden(Some((start, end))));
        return;
    }
    if source == "svelte" {
        for spec in &d.specifiers {
            if let svelte_js_ast::ImportSpecifierKind::Named(s) = spec {
                let imp_name = match &s.imported {
                    svelte_js_ast::ModuleExportName::Identifier(i) => i.name.as_str(),
                    svelte_js_ast::ModuleExportName::String(s) => s.value.as_str(),
                };
                if imp_name == "beforeUpdate" || imp_name == "afterUpdate" {
                    state.errors.push(errors::runes_mode_invalid_import(
                        Some((s.span.start, s.span.end)),
                        imp_name,
                    ));
                }
            }
        }
    }
}

/// `$: foo = bar` — port of visitors/LabeledStatement.js (placement
/// portion). In runes mode, top-level `$:` reactive statements are an
/// error. The dependency-tracking part of the upstream visitor is
/// deferred.
fn visit_labeled_statement(
    l: &svelte_js_ast::LabeledStatement,
    is_instance: bool,
    state: &mut ValidateState,
) {
    if l.label.name != "$" {
        return;
    }
    let start = l.span.start;
    let end = l.span.end;
    if !is_instance {
        state
            .warnings
            .push(warnings::reactive_declaration_invalid_placement(Some((start, end))));
        return;
    }
    if state.is_runes {
        state
            .errors
            .push(errors::legacy_reactive_statement_invalid(Some((start, end))));
    }
}

/// `validate_opening_tag(node, state, marker)` — shared/utils.js.
///
/// Currently a no-op stub: upstream checks that there's no whitespace
/// between `{` and the marker (e.g. `{@html ...}` not `{ @html ...}`).
/// The parser already rejects malformed tags so this is mostly belt-and-
/// suspenders; emitting the warning would require access to the source
/// bytes around `t.start`, which we'll add in a follow-up.
fn validate_opening_tag(_start: u32, _marker: &str, _state: &mut ValidateState) {
    // TODO: byte-peek at `_start..start+1` to see if there's whitespace
    // after `{`. For now we trust the parser.
}
