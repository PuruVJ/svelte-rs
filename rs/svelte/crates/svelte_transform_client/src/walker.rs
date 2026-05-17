//! Client template walker.
//!
//! Handles a growing set of common shapes — single- or multi-root templates
//! whose top-level nodes are RegularElements, Components, or a `<svelte:element>`,
//! optionally backed by a script body that contains only plain `let`/`const`/
//! function declarations + `$state(LIT)` bindings whose targets are never
//! assigned (so they erase to plain values).
//!
//! Reactivity machinery built so far:
//! - `el.textContent = EXPR` for elements with a single non-reactive
//!   expression child.
//! - `$.child(parent)` + `$.reset(parent)` + `$.template_effect((args...) =>
//!   $.set_text(text, \`...\`), [() => expr1, ...])` for elements with two
//!   or more expression children (reactive or not — upstream uses the
//!   template_effect path uniformly past one expression).
//!
//! Anything outside this contract returns `None` so the next visitor can
//! take over, currently surfacing as `typed_client_unsupported`.

use std::collections::{HashMap, HashSet};

use svelte_ast::attributes::{Attribute, AttributeValue, AttributeValuePart, ElementAttribute};
use svelte_ast::elements::{Component, RegularElement};
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

pub fn try_typed_client_walker(root: &Root, component_name: &str) -> Option<Program> {
    if root.css.is_some() || root.module.is_some() {
        return None;
    }

    // Script analysis: collect statements to emit, plus any erased rune
    // bindings.
    let script = analyze_script(root.instance.as_ref())?;

    // Collect top-level non-ws nodes.
    let nodes: Vec<&FragmentChild> = root
        .fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if nodes.is_empty() {
        return None;
    }

    let classified: Vec<NodeKind> = nodes.iter().map(|n| classify(n)).collect::<Option<_>>()?;

    let is_multi_root = nodes.len() > 1;
    let mut html = String::with_capacity(64);
    let mut body_stmts: Vec<Statement> = Vec::new();
    let mut effects: Vec<Statement> = Vec::new(); // emitted after navigation

    // Start the function body with the rewritten script body.
    body_stmts.extend(script.body.clone());

    let mut var_counts: HashMap<String, usize> = HashMap::new();
    let mut prev_var: Option<String> = None;

    // Root holder: for multi-root we own a `fragment` variable; for single-root
    // the root element variable IS the holder.
    let root_holder: String;

    if is_multi_root {
        body_stmts.push(t::var("fragment", t::call(t::id("root"), vec![])));
        root_holder = "fragment".to_string();
    } else {
        // Single-root: variable name comes from the only top-level node.
        let first_name = single_root_var_name(&classified[0]);
        let var = unique_var(&first_name, &mut var_counts);
        body_stmts.push(t::var(&var, t::call(t::id("root"), vec![])));
        prev_var = Some(var.clone());
        root_holder = var;
    }

    let last_idx = classified.len() - 1;
    for (i, kind) in classified.iter().enumerate() {
        match kind {
            NodeKind::StaticElement(el) => {
                serialize_element(el, &mut html, /*body*/ true, /*reactive*/ false)?;
                // First-nav only needed in multi-root; single-root already has root_holder.
                if is_multi_root {
                    let var = unique_var(&el.name, &mut var_counts);
                    emit_nav(&mut body_stmts, &var, prev_var.as_deref());
                    prev_var = Some(var);
                }
            }
            NodeKind::InterpElement(el, content) => {
                let needs_reactive_body = matches!(content, ElementContent::Reactive(_));
                serialize_element(el, &mut html, /*body*/ false, needs_reactive_body)?;
                let var = if is_multi_root {
                    let v = unique_var(&el.name, &mut var_counts);
                    emit_nav(&mut body_stmts, &v, prev_var.as_deref());
                    prev_var = Some(v.clone());
                    v
                } else {
                    prev_var.clone().expect("single-root nav established")
                };
                emit_element_content(content, &var, &mut body_stmts, &mut effects);
            }
            NodeKind::Component(c) => {
                html.push_str("<!>");
                let var = if is_multi_root {
                    let v = unique_var("node", &mut var_counts);
                    emit_nav(&mut body_stmts, &v, prev_var.as_deref());
                    prev_var = Some(v.clone());
                    v
                } else {
                    prev_var.clone().expect("single-root nav established")
                };
                body_stmts.push(component_call(c, &var)?);
            }
        }
        if is_multi_root && i < last_idx {
            html.push(' ');
        }
    }

    // Append effects (template_effect calls etc.) after all navigation.
    body_stmts.extend(effects);

    // Final append.
    body_stmts.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(&root_holder)],
    )));

    // Module-level `var root = $.from_html(\`HTML\`[, 1]);`
    let mut from_html_args = vec![t::template_raw(vec![html], vec![])];
    if is_multi_root {
        from_html_args.push(t::lit_number(1.0));
    }
    let root_decl = t::var(
        "root",
        t::call(t::member_id(t::id("$"), "from_html"), from_html_args),
    );

    let export = t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
        body_stmts,
    );

    let mut prog: Vec<Statement> = Vec::with_capacity(5 + script.imports.len());
    prog.push(t::import_side_effect("svelte/internal/disclose-version"));
    if script.emit_legacy_flag {
        prog.push(t::import_side_effect("svelte/internal/flags/legacy"));
    }
    prog.push(t::import_namespace("$", "svelte/internal/client"));
    prog.extend(script.imports.clone());
    prog.push(root_decl);
    prog.push(export);
    Some(t::program(prog))
}

// ---------------------------------------------------------------------------
// Script analysis
// ---------------------------------------------------------------------------

struct ScriptInfo {
    /// Hoisted imports go above the `var root = ...` declaration.
    imports: Vec<Statement>,
    /// Rewritten body statements emitted at the start of the function.
    body: Vec<Statement>,
    /// Whether to emit `import 'svelte/internal/flags/legacy';`
    emit_legacy_flag: bool,
}

fn analyze_script(instance: opt_ref::Ref<svelte_ast::root::Script>) -> Option<ScriptInfo> {
    let Some(script) = instance else {
        return Some(ScriptInfo {
            imports: Vec::new(),
            body: Vec::new(),
            emit_legacy_flag: true,
        });
    };

    let body = &script.content.body;
    let assigned: HashSet<String> = collect_assigned_targets(body);

    let mut imports: Vec<Statement> = Vec::new();
    let mut rest: Vec<Statement> = Vec::new();
    let mut uses_runes = false;
    let mut saw_non_import = false;
    for s in body {
        match s {
            Statement::Import(_) => {
                if saw_non_import {
                    return None;
                }
                imports.push(s.clone());
            }
            _ => {
                saw_non_import = true;
                let rewritten = rewrite_top_stmt(s, &assigned, &mut uses_runes)?;
                rest.push(rewritten);
            }
        }
    }

    Some(ScriptInfo {
        imports,
        body: rest,
        emit_legacy_flag: !uses_runes,
    })
}

/// Rewrite a top-level script statement for the client. Currently:
/// - `let X = $state(LIT)` where X is never assigned anywhere → `let X = LIT;`
/// - `let X = LIT` (no rune) → unchanged
/// - `const X = LIT` → unchanged
/// - Functions → unchanged
/// - Anything else → bail (returns None).
fn rewrite_top_stmt(
    s: &Statement,
    assigned: &HashSet<String>,
    uses_runes: &mut bool,
) -> Option<Statement> {
    match s {
        Statement::Variable(v) => {
            let mut out = (**v).clone();
            for d in &mut out.declarations {
                if let Some(init) = &mut d.init {
                    let stripped = try_strip_state(init, &d.id, assigned, uses_runes);
                    if !stripped {
                        // Identifier init or other simple non-rune init: OK.
                        // Bail if init contains an unsupported rune call.
                        if expr_has_unsupported_rune(init) {
                            return None;
                        }
                    }
                }
            }
            Some(Statement::Variable(Box::new(out)))
        }
        Statement::Function(_) => Some(s.clone()),
        Statement::Expression(_) => Some(s.clone()),
        _ => None,
    }
}

/// If `init` is `$state(LIT)` (or `$state.raw(LIT)`) and `id` is a never-
/// assigned identifier, replace init with the inner literal. Returns true if
/// rewrite happened.
fn try_strip_state(
    init: &mut Expression,
    id: &Pattern,
    assigned: &HashSet<String>,
    uses_runes: &mut bool,
) -> bool {
    let Some(name) = pattern_single_ident(id) else { return false };
    if assigned.contains(&name) {
        // Has assignments — would need full $.state lowering; mark and bail
        // at the caller level.
        if is_state_call(init) {
            *uses_runes = true;
        }
        return false;
    }
    if !is_state_call(init) {
        return false;
    }
    *uses_runes = true;
    let Expression::Call(c) = init else { return false };
    let arg = c.arguments.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e.clone()),
        _ => None,
    });
    *init = arg.unwrap_or_else(|| Expression::Identifier(Identifier {
        name: "undefined".to_string(),
        span: Span::ZERO,
    }));
    true
}

fn pattern_single_ident(p: &Pattern) -> Option<String> {
    match p {
        Pattern::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

fn is_state_call(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    let Some(kp) = global_keypath(&c.callee) else { return false };
    matches!(kp.as_str(), "$state" | "$state.raw" | "$state.eager")
}

fn expr_has_unsupported_rune(e: &Expression) -> bool {
    match e {
        Expression::Call(c) => {
            if let Some(kp) = global_keypath(&c.callee) {
                if kp.starts_with('$') {
                    return true;
                }
            }
            expr_has_unsupported_rune(&c.callee)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_has_unsupported_rune(e),
                    Argument::Spread(s) => expr_has_unsupported_rune(&s.argument),
                })
        }
        Expression::Member(m) => expr_has_unsupported_rune(&m.object),
        _ => false,
    }
}

fn global_keypath(e: &Expression) -> Option<String> {
    match e {
        Expression::Identifier(i) => Some(i.name.clone()),
        Expression::Member(m) => {
            if m.computed || m.optional {
                return None;
            }
            let base = global_keypath(&m.object)?;
            let prop = match &m.property {
                MemberProperty::Identifier(i) => i.name.clone(),
                _ => return None,
            };
            Some(format!("{base}.{prop}"))
        }
        _ => None,
    }
}

/// Walk every statement and collect identifiers that appear as the LHS of an
/// `Assignment` or the operand of an `Update` expression.
fn collect_assigned_targets(body: &[Statement]) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in body {
        scan_stmt_for_assignments(s, &mut out);
    }
    out
}

fn scan_stmt_for_assignments(s: &Statement, out: &mut HashSet<String>) {
    use Statement as S;
    match s {
        S::Variable(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    scan_expr_for_assignments(init, out);
                }
            }
        }
        S::Expression(e) => scan_expr_for_assignments(&e.expression, out),
        S::Block(b) => {
            for s in &b.body {
                scan_stmt_for_assignments(s, out);
            }
        }
        S::Return(r) => {
            if let Some(a) = &r.argument {
                scan_expr_for_assignments(a, out);
            }
        }
        S::If(i) => {
            scan_expr_for_assignments(&i.test, out);
            scan_stmt_for_assignments(&i.consequent, out);
            if let Some(a) = &i.alternate {
                scan_stmt_for_assignments(a, out);
            }
        }
        S::For(f) => {
            if let Some(init) = &f.init {
                if let ForInit::Expression(e) = init {
                    scan_expr_for_assignments(e, out);
                }
            }
            if let Some(t) = &f.test {
                scan_expr_for_assignments(t, out);
            }
            if let Some(u) = &f.update {
                scan_expr_for_assignments(u, out);
            }
            scan_stmt_for_assignments(&f.body, out);
        }
        S::ForIn(f) => {
            scan_expr_for_assignments(&f.right, out);
            scan_stmt_for_assignments(&f.body, out);
        }
        S::ForOf(f) => {
            scan_expr_for_assignments(&f.right, out);
            scan_stmt_for_assignments(&f.body, out);
        }
        S::While(w) => {
            scan_expr_for_assignments(&w.test, out);
            scan_stmt_for_assignments(&w.body, out);
        }
        S::DoWhile(w) => {
            scan_stmt_for_assignments(&w.body, out);
            scan_expr_for_assignments(&w.test, out);
        }
        S::Function(f) => {
            for s in &f.body.body {
                scan_stmt_for_assignments(s, out);
            }
        }
        S::Try(t) => {
            for s in &t.block.body {
                scan_stmt_for_assignments(s, out);
            }
            if let Some(h) = &t.handler {
                for s in &h.body.body {
                    scan_stmt_for_assignments(s, out);
                }
            }
            if let Some(f) = &t.finalizer {
                for s in &f.body {
                    scan_stmt_for_assignments(s, out);
                }
            }
        }
        S::Switch(sw) => {
            scan_expr_for_assignments(&sw.discriminant, out);
            for c in &sw.cases {
                if let Some(t) = &c.test {
                    scan_expr_for_assignments(t, out);
                }
                for s in &c.consequent {
                    scan_stmt_for_assignments(s, out);
                }
            }
        }
        S::Throw(t) => scan_expr_for_assignments(&t.argument, out),
        _ => {}
    }
}

fn scan_expr_for_assignments(e: &Expression, out: &mut HashSet<String>) {
    use Expression as E;
    match e {
        E::Assignment(a) => {
            collect_assignment_target_idents(&a.left, out);
            scan_expr_for_assignments(&a.right, out);
        }
        E::Update(u) => {
            if let E::Identifier(i) = &u.argument {
                out.insert(i.name.clone());
            } else {
                scan_expr_for_assignments(&u.argument, out);
            }
        }
        E::Call(c) => {
            scan_expr_for_assignments(&c.callee, out);
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => scan_expr_for_assignments(e, out),
                    Argument::Spread(s) => scan_expr_for_assignments(&s.argument, out),
                }
            }
        }
        E::New(n) => {
            scan_expr_for_assignments(&n.callee, out);
            for a in &n.arguments {
                match a {
                    Argument::Expression(e) => scan_expr_for_assignments(e, out),
                    Argument::Spread(s) => scan_expr_for_assignments(&s.argument, out),
                }
            }
        }
        E::Member(m) => {
            scan_expr_for_assignments(&m.object, out);
            if let MemberProperty::Expression(e) = &m.property {
                scan_expr_for_assignments(e, out);
            }
        }
        E::Binary(b) => {
            scan_expr_for_assignments(&b.left, out);
            scan_expr_for_assignments(&b.right, out);
        }
        E::Logical(l) => {
            scan_expr_for_assignments(&l.left, out);
            scan_expr_for_assignments(&l.right, out);
        }
        E::Conditional(c) => {
            scan_expr_for_assignments(&c.test, out);
            scan_expr_for_assignments(&c.consequent, out);
            scan_expr_for_assignments(&c.alternate, out);
        }
        E::Unary(u) => scan_expr_for_assignments(&u.argument, out),
        E::Sequence(s) => {
            for e in &s.expressions {
                scan_expr_for_assignments(e, out);
            }
        }
        E::Paren(p) => scan_expr_for_assignments(&p.expression, out),
        E::Template(t) => {
            for ex in &t.expressions {
                scan_expr_for_assignments(ex, out);
            }
        }
        E::Spread(s) => scan_expr_for_assignments(&s.argument, out),
        E::Arrow(a) => match &a.body {
            ArrowBody::Block(b) => {
                for s in &b.body {
                    scan_stmt_for_assignments(s, out);
                }
            }
            ArrowBody::Expression(e) => scan_expr_for_assignments(e, out),
        },
        E::Function(f) => {
            for s in &f.body.body {
                scan_stmt_for_assignments(s, out);
            }
        }
        E::Array(a) => {
            for el in &a.elements {
                if let ArrayElement::Expression(e) = el {
                    scan_expr_for_assignments(e, out);
                }
            }
        }
        E::Object(o) => {
            for m in &o.properties {
                if let ObjectMember::Property(p) = m {
                    scan_expr_for_assignments(&p.value, out);
                }
            }
        }
        E::Await(a) => scan_expr_for_assignments(&a.argument, out),
        _ => {}
    }
}

fn collect_assignment_target_idents(target: &AssignmentTarget, out: &mut HashSet<String>) {
    match target {
        AssignmentTarget::Expression(e) => {
            if let Expression::Identifier(i) = e {
                out.insert(i.name.clone());
            }
        }
        AssignmentTarget::Pattern(p) => collect_pattern_idents(p, out),
    }
}

fn collect_pattern_idents(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Identifier(i) => {
            out.insert(i.name.clone());
        }
        Pattern::Array(a) => {
            for el in &a.elements {
                if let Some(p) = el {
                    collect_pattern_idents(p, out);
                }
            }
        }
        Pattern::Object(o) => {
            for m in &o.properties {
                if let ObjectPatternMember::Property(p) = m {
                    collect_pattern_idents(&p.value, out);
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Template classification
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum NodeKind<'a> {
    StaticElement(&'a RegularElement),
    InterpElement(&'a RegularElement, ElementContent<'a>),
    Component(&'a Component),
}

#[derive(Debug)]
enum ElementContent<'a> {
    /// `<el>{expr}</el>` — single expression child. Lowered to
    /// `el.textContent = EXPR` provided EXPR doesn't reference a state-tracked
    /// binding (we don't yet wrap such reads with `$.get`).
    DirectText(&'a Expression),
    /// `<el>...mix of static text and expressions...</el>` — at least two
    /// fragments combined. Lowered via `$.child(el)` + `$.template_effect`.
    Reactive(Vec<TextPart<'a>>),
}

#[derive(Debug)]
enum TextPart<'a> {
    Static(String),
    Expr(&'a Expression),
}

fn single_root_var_name(kind: &NodeKind) -> String {
    match kind {
        NodeKind::StaticElement(el) => el.name.clone(),
        NodeKind::InterpElement(el, _) => el.name.clone(),
        NodeKind::Component(_) => "fragment".to_string(),
    }
}

fn classify(n: &FragmentChild) -> Option<NodeKind<'_>> {
    match n {
        FragmentChild::RegularElement(el) => {
            if !all_static_attrs(&el.attributes) {
                return None;
            }
            // Walk children: collect text + expression fragments.
            let mut parts: Vec<TextPart> = Vec::new();
            for c in &el.fragment.nodes {
                match c {
                    FragmentChild::Text(t) => {
                        parts.push(TextPart::Static(t.data.clone()));
                    }
                    FragmentChild::ExpressionTag(et) => {
                        parts.push(TextPart::Expr(&et.expression));
                    }
                    FragmentChild::RegularElement(_) => {
                        if !all_static(&[c.clone()]) {
                            return None;
                        }
                        // Nested static element prevents text-content lowering;
                        // treat as full static body.
                        parts.clear();
                        if all_static(&el.fragment.nodes) {
                            return Some(NodeKind::StaticElement(el));
                        }
                        return None;
                    }
                    _ => return None,
                }
            }

            // Strip leading/trailing whitespace-only Static parts.
            while parts
                .first()
                .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                .unwrap_or(false)
            {
                parts.remove(0);
            }
            while parts
                .last()
                .map(|p| matches!(p, TextPart::Static(s) if s.trim().is_empty()))
                .unwrap_or(false)
            {
                parts.pop();
            }

            // No content → static element.
            if parts.is_empty() {
                return Some(NodeKind::StaticElement(el));
            }

            // Only static text → static element.
            if parts.iter().all(|p| matches!(p, TextPart::Static(_))) {
                return Some(NodeKind::StaticElement(el));
            }

            // Single expression, no static parts → direct textContent.
            if parts.len() == 1 {
                if let TextPart::Expr(e) = &parts[0] {
                    if expr_is_safe_for_textcontent(e) {
                        return Some(NodeKind::InterpElement(el, ElementContent::DirectText(*e)));
                    }
                    // Fall through to reactive path.
                }
            }

            // Reactive: one+ expressions, possibly with text.
            Some(NodeKind::InterpElement(el, ElementContent::Reactive(parts)))
        }
        FragmentChild::Component(c) => Some(NodeKind::Component(c)),
        _ => None,
    }
}

fn all_static_attrs(attrs: &[ElementAttribute]) -> bool {
    attrs.iter().all(|a| match a {
        ElementAttribute::Attribute(a) => match &a.value {
            AttributeValue::Empty => true,
            AttributeValue::Many(parts) => parts
                .iter()
                .all(|p| matches!(p, AttributeValuePart::Text(_))),
            AttributeValue::Single(_) => false,
        },
        _ => false,
    })
}

fn write_static_attr(a: &Attribute, out: &mut String) -> Option<()> {
    match &a.value {
        AttributeValue::Empty => {
            out.push(' ');
            out.push_str(&a.name);
            Some(())
        }
        AttributeValue::Many(parts) => {
            out.push(' ');
            out.push_str(&a.name);
            out.push_str("=\"");
            for p in parts {
                if let AttributeValuePart::Text(t) = p {
                    for ch in t.data.chars() {
                        match ch {
                            '"' => out.push_str("&quot;"),
                            '&' => out.push_str("&amp;"),
                            '`' => out.push_str("\\`"),
                            '\\' => out.push_str("\\\\"),
                            _ => out.push(ch),
                        }
                    }
                }
            }
            out.push('"');
            Some(())
        }
        _ => None,
    }
}

/// Conservative check: expression doesn't need `$.get()` wrapping. Literals,
/// `loc.href`-style globals, simple calls into globals are accepted.
fn expr_is_safe_for_textcontent(e: &Expression) -> bool {
    match e {
        Expression::Literal(_) => true,
        Expression::Member(m) => match &m.object {
            Expression::Identifier(_) => !m.computed,
            _ => expr_is_safe_for_textcontent(&m.object) && !m.computed,
        },
        Expression::Call(c) => {
            if !expr_is_safe_for_textcontent(&c.callee) {
                return false;
            }
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => {
                        if !expr_is_safe_for_textcontent(e) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
        Expression::Identifier(_) => true,
        _ => false,
    }
}

fn all_static(nodes: &[FragmentChild]) -> bool {
    nodes.iter().all(|c| match c {
        FragmentChild::Text(_) | FragmentChild::Comment(_) => true,
        FragmentChild::RegularElement(el) => {
            el.attributes.is_empty() && all_static(&el.fragment.nodes)
        }
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// HTML serialization
// ---------------------------------------------------------------------------

fn serialize_element(
    el: &RegularElement,
    out: &mut String,
    include_body: bool,
    needs_text_node: bool,
) -> Option<()> {
    out.push('<');
    out.push_str(&el.name);
    for attr in &el.attributes {
        if let ElementAttribute::Attribute(a) = attr {
            write_static_attr(a, out)?;
        } else {
            return None;
        }
    }
    if is_void(&el.name) {
        out.push_str("/>");
        return Some(());
    }
    out.push('>');
    if include_body {
        for c in &el.fragment.nodes {
            serialize_static_child(c, out)?;
        }
    } else if needs_text_node {
        // Insert a single space so a text node exists for `$.child(el)`.
        out.push(' ');
    }
    out.push_str("</");
    out.push_str(&el.name);
    out.push('>');
    Some(())
}

fn serialize_static_child(c: &FragmentChild, out: &mut String) -> Option<()> {
    match c {
        FragmentChild::Text(t) => {
            for ch in t.data.chars() {
                match ch {
                    '`' => out.push_str("\\`"),
                    '\\' => out.push_str("\\\\"),
                    _ => out.push(ch),
                }
            }
            Some(())
        }
        FragmentChild::RegularElement(el) => serialize_element(el, out, true, false),
        FragmentChild::Comment(_) => Some(()),
        _ => None,
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}

// ---------------------------------------------------------------------------
// Body emission
// ---------------------------------------------------------------------------

fn emit_nav(out: &mut Vec<Statement>, name: &str, prev: Option<&str>) {
    let init = if let Some(p) = prev {
        t::call(
            t::member_id(t::id("$"), "sibling"),
            vec![t::id(p), t::lit_number(2.0)],
        )
    } else {
        t::call(t::member_id(t::id("$"), "first_child"), vec![t::id("fragment")])
    };
    out.push(t::var(name, init));
}

fn unique_var(base: &str, counts: &mut HashMap<String, usize>) -> String {
    let n = counts.entry(base.to_string()).or_insert(0);
    let name = if *n == 0 {
        base.to_string()
    } else {
        format!("{base}_{n}")
    };
    *n += 1;
    name
}

fn emit_element_content(
    content: &ElementContent,
    parent_var: &str,
    body_stmts: &mut Vec<Statement>,
    effects: &mut Vec<Statement>,
) {
    match content {
        ElementContent::DirectText(expr) => {
            let target = Expression::Member(Box::new(MemberExpression {
                object: t::id(parent_var),
                property: MemberProperty::Identifier(Identifier {
                    name: "textContent".to_string(),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            }));
            body_stmts.push(t::stmt(Expression::Assignment(Box::new(
                AssignmentExpression {
                    left: AssignmentTarget::Expression(target),
                    operator: AssignmentOperator::Assign,
                    right: textcontent_value((*expr).clone()),
                    span: Span::ZERO,
                },
            ))));
        }
        ElementContent::Reactive(parts) => {
            // `var text = $.child(parent_var);`
            let text_var = format!("text"); // Could conflict — keep simple for now.
            body_stmts.push(t::var(
                &text_var,
                t::call(t::member_id(t::id("$"), "child"), vec![t::id(parent_var)]),
            ));
            body_stmts.push(t::stmt(t::call(
                t::member_id(t::id("$"), "reset"),
                vec![t::id(parent_var)],
            )));

            // Build template literal + dep functions.
            let (template_expr, dep_fns) = build_template_effect(parts);
            // `$.template_effect((args...) => $.set_text(text, TEMPLATE), [deps])`
            // For 1+ exprs: pass deps as array; for 0 exprs we wouldn't be here.
            let mut params: Vec<Pattern> = Vec::new();
            for i in 0..dep_fns.len() {
                params.push(t::pat_id(&format!("${i}")));
            }
            let fn_body = t::call(
                t::member_id(t::id("$"), "set_text"),
                vec![t::id(&text_var), template_expr],
            );
            let fn_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
                params,
                body: ArrowBody::Expression(fn_body),
                r#async: false,
                span: Span::ZERO,
            }));
            let deps_array = Expression::Array(Box::new(ArrayExpression {
                elements: dep_fns
                    .into_iter()
                    .map(ArrayElement::Expression)
                    .collect(),
                span: Span::ZERO,
            }));
            effects.push(t::stmt(t::call(
                t::member_id(t::id("$"), "template_effect"),
                vec![fn_arrow, deps_array],
            )));
        }
    }
}

/// Given the element's text parts, build:
/// - The template literal expression for `set_text` (e.g.
///   `` `Count is ${$0 ?? ''}` ``).
/// - The deps array entries `() => exprN`.
fn build_template_effect(parts: &[TextPart]) -> (Expression, Vec<Expression>) {
    let mut quasis: Vec<String> = Vec::with_capacity(parts.len() + 1);
    let mut subs: Vec<Expression> = Vec::new();
    let mut dep_fns: Vec<Expression> = Vec::new();

    let mut current = String::new();
    let mut placeholder_idx: usize = 0;
    for p in parts {
        match p {
            TextPart::Static(s) => current.push_str(s),
            TextPart::Expr(e) => {
                quasis.push(std::mem::take(&mut current));
                // `${$N ?? ''}`
                let placeholder = Expression::Logical(Box::new(LogicalExpression {
                    left: t::id(&format!("${placeholder_idx}")),
                    operator: LogicalOperator::Coalesce,
                    right: Expression::Literal(Box::new(Literal::String(StringLiteral {
                        value: String::new(),
                        raw: None,
                        span: Span::ZERO,
                    }))),
                    span: Span::ZERO,
                }));
                subs.push(placeholder);
                // dep: `() => EXPR`
                dep_fns.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression((*e).clone()),
                    r#async: false,
                    span: Span::ZERO,
                })));
                placeholder_idx += 1;
            }
        }
    }
    quasis.push(current);

    let tmpl = t::template_raw(quasis, subs);
    (tmpl, dep_fns)
}

/// Convert a number literal to a string literal when assigned to `.textContent`.
fn textcontent_value(e: Expression) -> Expression {
    if let Expression::Literal(lit) = &e {
        if let Literal::Number(n) = lit.as_ref() {
            return Expression::Literal(Box::new(Literal::String(StringLiteral {
                value: format_num(n.value),
                raw: None,
                span: Span::ZERO,
            })));
        }
    }
    e
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        return format!("{}", n as i64);
    }
    format!("{n}")
}

fn component_call(c: &Component, node_var: &str) -> Option<Statement> {
    let mut props: Vec<ObjectMember> = Vec::new();
    for attr in &c.attributes {
        match attr {
            ElementAttribute::Attribute(a) => props.push(attr_to_prop(a)?),
            ElementAttribute::SpreadAttribute(s) => {
                props.push(ObjectMember::Spread(Box::new(SpreadElement {
                    argument: s.expression.clone(),
                    span: Span::ZERO,
                })));
            }
            _ => return None,
        }
    }
    Some(t::stmt(Expression::Call(Box::new(CallExpression {
        callee: t::id(&c.name),
        arguments: vec![
            Argument::Expression(t::id(node_var)),
            Argument::Expression(Expression::Object(Box::new(ObjectExpression {
                properties: props,
                span: Span::ZERO,
            }))),
        ],
        optional: false,
        span: Span::ZERO,
    }))))
}

fn attr_to_prop(a: &Attribute) -> Option<ObjectMember> {
    let value: Expression = match &a.value {
        AttributeValue::Empty => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: true,
            span: Span::ZERO,
        }))),
        AttributeValue::Single(tag) => tag.expression.clone(),
        AttributeValue::Many(parts) => {
            if parts.len() == 1 {
                match &parts[0] {
                    AttributeValuePart::Text(t) => {
                        Expression::Literal(Box::new(Literal::String(StringLiteral {
                            value: t.data.clone(),
                            raw: None,
                            span: Span::ZERO,
                        })))
                    }
                    AttributeValuePart::ExpressionTag(e) => e.expression.clone(),
                }
            } else {
                return None;
            }
        }
    };
    Some(ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: a.name.clone(),
            span: Span::ZERO,
        }),
        value,
        kind: PropertyKind::Init,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    })))
}

// ---------------------------------------------------------------------------
// Math.X compile-time fold (re-exported for use in the compile pipeline).
// ---------------------------------------------------------------------------

pub fn fold_in_fragment(f: &mut Fragment) {
    for n in &mut f.nodes {
        fold_in_node(n);
    }
}

fn fold_in_node(n: &mut FragmentChild) {
    match n {
        FragmentChild::ExpressionTag(t) => fold_expr(&mut t.expression),
        FragmentChild::HtmlTag(t) => fold_expr(&mut t.expression),
        FragmentChild::RegularElement(el) => {
            for attr in &mut el.attributes {
                fold_in_attr(attr);
            }
            for c in &mut el.fragment.nodes {
                fold_in_node(c);
            }
        }
        FragmentChild::Component(c) => {
            for attr in &mut c.attributes {
                fold_in_attr(attr);
            }
            for c in &mut c.fragment.nodes {
                fold_in_node(c);
            }
        }
        _ => {}
    }
}

fn fold_in_attr(attr: &mut ElementAttribute) {
    match attr {
        ElementAttribute::Attribute(a) => match &mut a.value {
            AttributeValue::Single(tag) => fold_expr(&mut tag.expression),
            AttributeValue::Many(parts) => {
                for p in parts {
                    if let AttributeValuePart::ExpressionTag(t) = p {
                        fold_expr(&mut t.expression);
                    }
                }
            }
            _ => {}
        },
        ElementAttribute::SpreadAttribute(s) => fold_expr(&mut s.expression),
        _ => {}
    }
}

fn fold_expr(e: &mut Expression) {
    match e {
        Expression::Call(c) => {
            fold_expr(&mut c.callee);
            for a in &mut c.arguments {
                if let Argument::Expression(e) = a {
                    fold_expr(e);
                }
            }
            if let Some(folded) = try_fold_math_call(c) {
                *e = folded;
            }
        }
        Expression::Member(m) => fold_expr(&mut m.object),
        Expression::Binary(b) => {
            fold_expr(&mut b.left);
            fold_expr(&mut b.right);
        }
        Expression::Logical(l) => {
            fold_expr(&mut l.left);
            fold_expr(&mut l.right);
        }
        Expression::Conditional(c) => {
            fold_expr(&mut c.test);
            fold_expr(&mut c.consequent);
            fold_expr(&mut c.alternate);
        }
        Expression::Unary(u) => fold_expr(&mut u.argument),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                fold_expr(e);
            }
        }
        Expression::Paren(p) => fold_expr(&mut p.expression),
        _ => {}
    }
}

fn try_fold_math_call(c: &CallExpression) -> Option<Expression> {
    let m = match &c.callee {
        Expression::Member(m) => m,
        _ => return None,
    };
    if m.computed || m.optional {
        return None;
    }
    let obj = match &m.object {
        Expression::Identifier(i) => i.name.as_str(),
        _ => return None,
    };
    let prop = match &m.property {
        MemberProperty::Identifier(i) => i.name.as_str(),
        _ => return None,
    };
    if obj != "Math" {
        return None;
    }
    let mut nums: Vec<f64> = Vec::with_capacity(c.arguments.len());
    for a in &c.arguments {
        let e = match a {
            Argument::Expression(e) => e,
            _ => return None,
        };
        let n = match e {
            Expression::Literal(lit) => match lit.as_ref() {
                Literal::Number(n) => n.value,
                _ => return None,
            },
            _ => return None,
        };
        nums.push(n);
    }
    let result: f64 = match prop {
        "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
        "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "abs" if nums.len() == 1 => nums[0].abs(),
        "floor" if nums.len() == 1 => nums[0].floor(),
        "ceil" if nums.len() == 1 => nums[0].ceil(),
        "round" if nums.len() == 1 => nums[0].round(),
        _ => return None,
    };
    Some(Expression::Literal(Box::new(Literal::Number(NumberLiteral {
        value: result,
        raw: Some(format_num(result)),
        span: Span::ZERO,
    }))))
}

// Helper module to avoid `Option<&Script>` lifetime gymnastics.
mod opt_ref {
    pub type Ref<'a, T> = Option<&'a T>;
}
