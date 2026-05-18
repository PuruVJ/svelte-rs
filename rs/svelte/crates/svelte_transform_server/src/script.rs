//! Server-side script rewriting.
//!
//! Mirrors `phases/3-transform/server/visitors/CallExpression.js` etc.
//! On the server, all reactive runes erase to their inert values:
//!
//! - `$state(x)` / `$state.raw(x)` → `x`
//! - `$derived(x)` / `$derived.by(x)` → `x`
//! - `$bindable(default)` → `default`
//! - `$effect(...)` / `$effect.pre(...)` / `$effect.root(...)` / `$inspect(...)` / `$host()` → `undefined`
//! - `$props()` → `$$props`
//! - `$props.id` → `$$props_id`
//!
//! Walks every Expression/Statement and rewrites in place.

use std::collections::{HashMap, HashSet};

use svelte_js_ast::*;

/// Result of `rewrite_program_for_server`.
pub struct RewriteInfo {
    pub uses_props: bool,
    pub rune_bindings: HashSet<String>,
    /// Names of bindings whose initializer was `$derived(...)` or
    /// `$derived.by(...)`. Template references to these names need to be
    /// CALLED (`promise` → `promise()`).
    pub derived_bindings: HashSet<String>,
    /// `Some(name)` when the script has `let name = $props()` — a single
    /// identifier destructure.
    pub single_id_props: Option<String>,
    /// Set when the script contains a class with rune-initialized fields.
    pub has_class_with_runes: bool,
}

/// Result of `transform_async_script_server` — non-None when the script
/// contains top-level `await`. Carries the rewritten statements (hoisted
/// `var` decls + `var $$promises = $$renderer.run([...])`) plus a set of
/// bindings whose template references should be wrapped with
/// `$$renderer.async([$$promises[idx]], ...)`.
pub struct AsyncInfo {
    pub setup_stmts: Vec<Statement>,
    pub async_bindings: HashSet<String>,
    pub last_group_idx: usize,
}

/// Returns true if the program contains top-level `await` (an Await
/// expression not inside an async function / async arrow).
pub fn has_top_level_await(p: &Program) -> bool {
    p.body.iter().any(stmt_has_top_level_await)
}

fn stmt_has_top_level_await(s: &Statement) -> bool {
    match s {
        Statement::Variable(v) => v
            .declarations
            .iter()
            .any(|d| d.init.as_ref().map_or(false, expr_has_top_level_await)),
        Statement::Expression(e) => expr_has_top_level_await(&e.expression),
        _ => false,
    }
}

fn expr_has_top_level_await(e: &Expression) -> bool {
    match e {
        Expression::Await(_) => true,
        // Async function/arrow boundaries stop the search.
        Expression::Function(f) if f.r#async => false,
        Expression::Arrow(a) if a.r#async => false,
        Expression::Call(c) => {
            expr_has_top_level_await(&c.callee)
                || c.arguments.iter().any(|a| match a {
                    Argument::Expression(e) => expr_has_top_level_await(e),
                    Argument::Spread(s) => expr_has_top_level_await(&s.argument),
                })
        }
        Expression::Binary(b) => {
            expr_has_top_level_await(&b.left) || expr_has_top_level_await(&b.right)
        }
        Expression::Logical(l) => {
            expr_has_top_level_await(&l.left) || expr_has_top_level_await(&l.right)
        }
        Expression::Unary(u) => expr_has_top_level_await(&u.argument),
        Expression::Member(m) => expr_has_top_level_await(&m.object),
        Expression::Conditional(c) => {
            expr_has_top_level_await(&c.test)
                || expr_has_top_level_await(&c.consequent)
                || expr_has_top_level_await(&c.alternate)
        }
        Expression::Paren(p) => expr_has_top_level_await(&p.expression),
        Expression::Sequence(s) => s.expressions.iter().any(expr_has_top_level_await),
        Expression::Spread(s) => expr_has_top_level_await(&s.argument),
        _ => false,
    }
}

/// Transform a server-side script with top-level await into:
/// 1. Hoisted `var X, Y, Z;` declaration of every top-level `let`/`const`.
/// 2. `var $$promises = $$renderer.run([async () => ..., () => { ... }]);`
///    where each await statement starts a new async arrow, and contiguous
///    runs of sync statements collapse into a single sync arrow.
///
/// Returns None if no top-level await is present.
pub fn transform_async_script_server(body: &[Statement]) -> Option<AsyncInfo> {
    let p = Program {
        source_type: SourceType::Module,
        body: body.to_vec(),
        span: Span::ZERO,
    };
    if !has_top_level_await(&p) {
        return None;
    }

    // Gather all let/const bindings to hoist + classify each statement.
    let mut hoisted_names: Vec<String> = Vec::new();
    enum Lowered {
        AsyncSet { name: String, init: Expression },
        Sync(Statement),
    }
    let mut lowered: Vec<Lowered> = Vec::new();

    for s in body {
        match s {
            Statement::Variable(v) => {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        hoisted_names.push(id.name.clone());
                        let init = d.init.clone().unwrap_or_else(undefined_expr);
                        if expr_has_top_level_await(&init) {
                            lowered.push(Lowered::AsyncSet {
                                name: id.name.clone(),
                                init,
                            });
                        } else {
                            lowered.push(Lowered::Sync(assignment_stmt(&id.name, init)));
                        }
                    } else {
                        return None;
                    }
                }
            }
            Statement::Expression(e) => {
                // Erase server-side $inspect/$inspect.trace calls.
                if let Expression::Call(c) = &e.expression {
                    if let Some(kp) = global_keypath(&c.callee) {
                        if matches!(kp.as_str(), "$inspect" | "$inspect.trace") {
                            lowered.push(Lowered::Sync(t::stmt(void_zero())));
                            continue;
                        }
                    }
                }
                // Plain `undefined` identifier statement (post-rune-erasure
                // form of `$inspect(...)` etc.) → emit as `void 0`.
                if let Expression::Identifier(id) = &e.expression {
                    if id.name == "undefined" {
                        lowered.push(Lowered::Sync(t::stmt(void_zero())));
                        continue;
                    }
                }
                lowered.push(Lowered::Sync(s.clone()));
            }
            _ => return None,
        }
    }

    // Build arrow groups: async statements get their own async arrow; sync
    // statements collapse into the immediately-following sync arrow.
    let mut groups: Vec<Expression> = Vec::new();
    let mut current_sync: Vec<Statement> = Vec::new();
    let mut last_was_async = false;

    let flush_sync = |groups: &mut Vec<Expression>, current_sync: &mut Vec<Statement>| {
        if current_sync.len() == 1 {
            // Single-statement sync arrow: emit `() => EXPR` if the stmt is
            // a single expression. Otherwise block.
            let s = current_sync.remove(0);
            if let Statement::Expression(es) = s {
                groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(es.expression),
                    r#async: false,
                    span: Span::ZERO,
                })));
                return;
            }
            current_sync.push(s);
        }
        groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
            params: Vec::new(),
            body: ArrowBody::Block(Box::new(BlockStatement {
                body: std::mem::take(current_sync),
                span: Span::ZERO,
            })),
            r#async: false,
            span: Span::ZERO,
        })));
    };

    for l in lowered {
        match l {
            Lowered::AsyncSet { name, init } => {
                // Flush any pending sync into its own group.
                if !current_sync.is_empty() {
                    flush_sync(&mut groups, &mut current_sync);
                }
                // `async () => X = INIT`
                let assign = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(t::id(&name)),
                    operator: AssignmentOperator::Assign,
                    right: init,
                    span: Span::ZERO,
                }));
                groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(assign),
                    r#async: true,
                    span: Span::ZERO,
                })));
                last_was_async = true;
            }
            Lowered::Sync(stmt) => {
                current_sync.push(stmt);
                last_was_async = false;
            }
        }
    }
    // Ensure there's a trailing sync group so $$promises[1] etc. has a slot.
    if !current_sync.is_empty() || last_was_async {
        if current_sync.is_empty() {
            current_sync.push(t::stmt(void_zero()));
        }
        flush_sync(&mut groups, &mut current_sync);
    }

    let last_group_idx = if groups.is_empty() { 0 } else { groups.len() - 1 };

    // var X, Y, Z;
    let mut setup_stmts: Vec<Statement> = Vec::new();
    if !hoisted_names.is_empty() {
        let decls: Vec<VariableDeclarator> = hoisted_names
            .iter()
            .map(|n| VariableDeclarator {
                id: t::pat_id(n),
                init: None,
                span: Span::ZERO,
            })
            .collect();
        setup_stmts.push(Statement::Variable(Box::new(VariableDeclaration {
            kind: VariableKind::Var,
            declarations: decls,
            span: Span::ZERO,
        })));
    }
    // var $$promises = $$renderer.run([...groups...]);
    setup_stmts.push(t::var(
        "$$promises",
        t::call(
            t::member_id(t::id("$$renderer"), "run"),
            vec![Expression::Array(Box::new(ArrayExpression {
                elements: groups.into_iter().map(ArrayElement::Expression).collect(),
                span: Span::ZERO,
            }))],
        ),
    ));

    let async_bindings: HashSet<String> = hoisted_names.into_iter().collect();
    Some(AsyncInfo {
        setup_stmts,
        async_bindings,
        last_group_idx,
    })
}

/// `void 0` — a UnaryExpression that evaluates to `undefined` and is the
/// idiomatic JS-output form for "no value".
fn void_zero() -> Expression {
    Expression::Unary(Box::new(UnaryExpression {
        operator: UnaryOperator::Void,
        argument: Expression::Literal(Box::new(Literal::Number(NumberLiteral {
            value: 0.0,
            raw: Some("0".to_string()),
            span: Span::ZERO,
        }))),
        prefix: true,
        span: Span::ZERO,
    }))
}

fn assignment_stmt(name: &str, value: Expression) -> Statement {
    t::stmt(Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(t::id(name)),
        operator: AssignmentOperator::Assign,
        right: value,
        span: Span::ZERO,
    })))
}

use svelte_transform_shared::builders_typed as t;

impl RewriteInfo {
    pub fn needs_component_wrap(&self) -> bool {
        self.single_id_props.is_some() || self.has_class_with_runes
    }
}

/// Rewrite a Program in-place to erase server-irrelevant rune calls.
pub fn rewrite_program_for_server(p: &mut Program) -> RewriteInfo {
    let mut rune_bindings: HashSet<String> = HashSet::new();
    let mut derived_bindings: HashSet<String> = HashSet::new();
    let mut single_id_props: Option<String> = None;
    let mut has_class_with_runes = false;
    for s in &p.body {
        collect_rune_bindings_stmt(s, &mut rune_bindings);
        collect_derived_bindings_stmt(s, &mut derived_bindings);
        check_single_id_props_stmt(s, &mut single_id_props);
        if stmt_has_class_with_runes(s) {
            has_class_with_runes = true;
        }
    }
    let mut ctx = Ctx { uses_props: false };
    for s in &mut p.body {
        rewrite_statement(s, &mut ctx);
    }
    if single_id_props.is_some() || has_class_with_runes {
        ctx.uses_props = true;
    }
    RewriteInfo {
        uses_props: ctx.uses_props,
        rune_bindings,
        derived_bindings,
        single_id_props,
        has_class_with_runes,
    }
}

fn collect_derived_bindings_stmt(s: &Statement, out: &mut HashSet<String>) {
    match s {
        Statement::Variable(v) => {
            for d in &v.declarations {
                if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                    if is_derived_call(init) {
                        out.insert(id.name.clone());
                    }
                }
            }
        }
        Statement::Block(b) => {
            for s in &b.body {
                collect_derived_bindings_stmt(s, out);
            }
        }
        Statement::ExportNamed(e) => {
            if let Some(d) = &e.declaration {
                collect_derived_bindings_stmt(d, out);
            }
        }
        _ => {}
    }
}

fn is_derived_call(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    let Some(kp) = global_keypath(&c.callee) else { return false };
    matches!(kp.as_str(), "$derived" | "$derived.by")
}

/// Walk an Expression and wrap every Identifier reference that names a
/// derived binding with `IDENT()` — making the template-position access
/// actually call the derived getter.
pub fn call_derived_refs(e: &mut Expression, derived: &HashSet<String>) {
    if derived.is_empty() {
        return;
    }
    call_derived_refs_inner(e, derived);
}

fn call_derived_refs_inner(e: &mut Expression, derived: &HashSet<String>) {
    match e {
        Expression::Identifier(i) => {
            if derived.contains(&i.name) {
                let id = std::mem::replace(
                    i,
                    Identifier { name: String::new(), span: Span::ZERO },
                );
                *e = Expression::Call(Box::new(CallExpression {
                    callee: Expression::Identifier(id),
                    arguments: Vec::new(),
                    optional: false,
                    span: Span::ZERO,
                }));
            }
        }
        Expression::Member(m) => {
            call_derived_refs_inner(&mut m.object, derived);
            if let MemberProperty::Expression(e) = &mut m.property {
                call_derived_refs_inner(e, derived);
            }
        }
        Expression::Call(c) => {
            call_derived_refs_inner(&mut c.callee, derived);
            for a in &mut c.arguments {
                match a {
                    Argument::Expression(e) => call_derived_refs_inner(e, derived),
                    Argument::Spread(s) => call_derived_refs_inner(&mut s.argument, derived),
                }
            }
        }
        Expression::Binary(b) => {
            call_derived_refs_inner(&mut b.left, derived);
            call_derived_refs_inner(&mut b.right, derived);
        }
        Expression::Logical(l) => {
            call_derived_refs_inner(&mut l.left, derived);
            call_derived_refs_inner(&mut l.right, derived);
        }
        Expression::Conditional(c) => {
            call_derived_refs_inner(&mut c.test, derived);
            call_derived_refs_inner(&mut c.consequent, derived);
            call_derived_refs_inner(&mut c.alternate, derived);
        }
        Expression::Unary(u) => call_derived_refs_inner(&mut u.argument, derived),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                call_derived_refs_inner(e, derived);
            }
        }
        Expression::Template(t) => {
            for ex in &mut t.expressions {
                call_derived_refs_inner(ex, derived);
            }
        }
        Expression::Paren(p) => call_derived_refs_inner(&mut p.expression, derived),
        _ => {}
    }
}

fn check_single_id_props_stmt(s: &Statement, out: &mut Option<String>) {
    if let Statement::Variable(v) = s {
        for d in &v.declarations {
            if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                if is_props_call(init) {
                    *out = Some(id.name.clone());
                }
            }
        }
    }
}

fn is_props_call(e: &Expression) -> bool {
    if let Expression::Call(c) = e {
        if let Some(kp) = global_keypath(&c.callee) {
            return kp == "$props";
        }
    }
    false
}

fn stmt_has_class_with_runes(s: &Statement) -> bool {
    match s {
        Statement::Class(c) => class_has_rune_fields(c),
        Statement::ExportDefault(e) => match &e.declaration {
            ExportDefault::Class(c) => class_has_rune_fields(c),
            _ => false,
        },
        _ => false,
    }
}

fn class_has_rune_fields(c: &ClassDeclaration) -> bool {
    c.body.body.iter().any(|m| {
        if let ClassMember::Property(p) = m {
            if let Some(value) = &p.value {
                return is_rune_call(value);
            }
        }
        false
    })
}

/// Rewrite a single `let X = $$props;` (post rune-erasure) into
/// `let { $$slots, $$events, ...X } = $$props;`. Used by the component-wrap path.
pub fn rewrite_props_destructure(p: &mut Program, identifier: &str) {
    for s in &mut p.body {
        if let Statement::Variable(v) = s {
            for d in &mut v.declarations {
                if let Pattern::Identifier(id) = &d.id {
                    if id.name == identifier {
                        // Replace with `{ $$slots, $$events, ...identifier }`.
                        d.id = Pattern::Object(Box::new(ObjectPattern {
                            properties: vec![
                                ObjectPatternMember::Property(Box::new(ObjectPatternProperty {
                                    key: PropertyKey::Identifier(Identifier {
                                        name: "$$slots".to_string(),
                                        span: Span::ZERO,
                                    }),
                                    value: Pattern::Identifier(Identifier {
                                        name: "$$slots".to_string(),
                                        span: Span::ZERO,
                                    }),
                                    computed: false,
                                    shorthand: true,
                                    span: Span::ZERO,
                                })),
                                ObjectPatternMember::Property(Box::new(ObjectPatternProperty {
                                    key: PropertyKey::Identifier(Identifier {
                                        name: "$$events".to_string(),
                                        span: Span::ZERO,
                                    }),
                                    value: Pattern::Identifier(Identifier {
                                        name: "$$events".to_string(),
                                        span: Span::ZERO,
                                    }),
                                    computed: false,
                                    shorthand: true,
                                    span: Span::ZERO,
                                })),
                                ObjectPatternMember::Rest(Box::new(RestElement {
                                    argument: Pattern::Identifier(Identifier {
                                        name: identifier.to_string(),
                                        span: Span::ZERO,
                                    }),
                                    span: Span::ZERO,
                                })),
                            ],
                            span: Span::ZERO,
                        }));
                    }
                }
            }
        }
    }
}

fn collect_rune_bindings_stmt(s: &Statement, out: &mut HashSet<String>) {
    match s {
        Statement::Variable(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    if is_rune_call(init) {
                        // Collect every identifier in the LHS pattern.
                        collect_pattern_names(&d.id, out);
                    }
                }
            }
        }
        Statement::Block(b) => {
            for s in &b.body {
                collect_rune_bindings_stmt(s, out);
            }
        }
        Statement::ExportNamed(e) => {
            if let Some(d) = &e.declaration {
                collect_rune_bindings_stmt(d, out);
            }
        }
        _ => {}
    }
}

fn is_rune_call(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    let Some(kp) = global_keypath(&c.callee) else { return false };
    matches!(
        kp.as_str(),
        "$state"
            | "$state.raw"
            | "$state.eager"
            | "$derived"
            | "$derived.by"
            | "$bindable"
            | "$props"
            | "$props.id"
            | "$effect"
            | "$inspect"
    )
}

fn collect_pattern_names(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Identifier(i) => {
            out.insert(i.name.clone());
        }
        Pattern::Array(a) => {
            for el in a.elements.iter().flatten() {
                collect_pattern_names(el, out);
            }
        }
        Pattern::Object(o) => {
            for m in &o.properties {
                match m {
                    ObjectPatternMember::Property(p) => collect_pattern_names(&p.value, out),
                    ObjectPatternMember::Rest(r) => collect_pattern_names(&r.argument, out),
                }
            }
        }
        Pattern::Rest(r) => collect_pattern_names(&r.argument, out),
        Pattern::Assignment(a) => collect_pattern_names(&a.left, out),
        Pattern::Member(_) => {}
    }
}

/// Find script bindings that are non-rune, non-mutated, initialized to a
/// primitive literal — these get inlined at template expression positions.
/// Returns a map `name -> literal Expression`.
///
/// Conservative: requires `let X = LITERAL` (no destructuring, no reassign).
pub fn collect_script_constants(
    p: &Program,
    skip: &HashSet<String>,
) -> HashMap<String, Expression> {
    // First pass: collect candidates.
    let mut candidates: HashMap<String, Expression> = HashMap::new();
    for s in &p.body {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let Pattern::Identifier(id) = &d.id {
                    if skip.contains(&id.name) {
                        continue;
                    }
                    if let Some(init) = &d.init {
                        if is_inlineable_literal(init) {
                            candidates.insert(id.name.clone(), init.clone());
                        }
                    }
                }
            }
        }
    }
    if candidates.is_empty() {
        return candidates;
    }
    // Second pass: drop any candidate that's ever reassigned or updated.
    let mut mutated: HashSet<String> = HashSet::new();
    for s in &p.body {
        collect_mutations_stmt(s, &mut mutated);
    }
    candidates.retain(|name, _| !mutated.contains(name));
    candidates
}

fn is_inlineable_literal(e: &Expression) -> bool {
    match e {
        Expression::Literal(_) => true,
        Expression::Identifier(i) => i.name == "undefined",
        _ => false,
    }
}

fn collect_mutations_stmt(s: &Statement, out: &mut HashSet<String>) {
    match s {
        Statement::Variable(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    collect_mutations_expr(init, out);
                }
            }
        }
        Statement::Expression(e) => collect_mutations_expr(&e.expression, out),
        Statement::Block(b) => {
            for s in &b.body {
                collect_mutations_stmt(s, out);
            }
        }
        Statement::Return(r) => {
            if let Some(a) = &r.argument {
                collect_mutations_expr(a, out);
            }
        }
        Statement::If(i) => {
            collect_mutations_expr(&i.test, out);
            collect_mutations_stmt(&i.consequent, out);
            if let Some(a) = &i.alternate {
                collect_mutations_stmt(a, out);
            }
        }
        Statement::For(f) => {
            if let Some(t) = &f.test {
                collect_mutations_expr(t, out);
            }
            if let Some(u) = &f.update {
                collect_mutations_expr(u, out);
            }
            collect_mutations_stmt(&f.body, out);
        }
        Statement::While(w) => {
            collect_mutations_expr(&w.test, out);
            collect_mutations_stmt(&w.body, out);
        }
        Statement::DoWhile(w) => {
            collect_mutations_stmt(&w.body, out);
            collect_mutations_expr(&w.test, out);
        }
        Statement::Function(f) => {
            for s in &f.body.body {
                collect_mutations_stmt(s, out);
            }
        }
        Statement::ExportNamed(e) => {
            if let Some(d) = &e.declaration {
                collect_mutations_stmt(d, out);
            }
        }
        _ => {}
    }
}

fn collect_mutations_expr(e: &Expression, out: &mut HashSet<String>) {
    match e {
        Expression::Assignment(a) => {
            if let AssignmentTarget::Pattern(Pattern::Identifier(i)) = &a.left {
                out.insert(i.name.clone());
            }
            collect_mutations_expr(&a.right, out);
        }
        Expression::Update(u) => {
            if let Expression::Identifier(i) = &u.argument {
                out.insert(i.name.clone());
            }
        }
        Expression::Call(c) => {
            collect_mutations_expr(&c.callee, out);
            for a in &c.arguments {
                if let Argument::Expression(e) = a {
                    collect_mutations_expr(e, out);
                }
            }
        }
        Expression::Binary(b) => {
            collect_mutations_expr(&b.left, out);
            collect_mutations_expr(&b.right, out);
        }
        Expression::Logical(l) => {
            collect_mutations_expr(&l.left, out);
            collect_mutations_expr(&l.right, out);
        }
        Expression::Conditional(c) => {
            collect_mutations_expr(&c.test, out);
            collect_mutations_expr(&c.consequent, out);
            collect_mutations_expr(&c.alternate, out);
        }
        Expression::Arrow(a) => match &a.body {
            ArrowBody::Block(b) => {
                for s in &b.body {
                    collect_mutations_stmt(s, out);
                }
            }
            ArrowBody::Expression(e) => collect_mutations_expr(e, out),
        },
        Expression::Function(f) => {
            for s in &f.body.body {
                collect_mutations_stmt(s, out);
            }
        }
        Expression::Member(m) => collect_mutations_expr(&m.object, out),
        Expression::Sequence(s) => {
            for e in &s.expressions {
                collect_mutations_expr(e, out);
            }
        }
        _ => {}
    }
}

/// Substitute identifier references in `e` with literals from `consts`,
/// then apply simple constant-fold rules (nullish-coalesce of literals,
/// member access of literals).
pub fn substitute_and_fold(e: &mut Expression, consts: &HashMap<String, Expression>) {
    // First substitute identifiers.
    substitute(e, consts);
    // Then fold.
    fold(e);
}

fn substitute(e: &mut Expression, consts: &HashMap<String, Expression>) {
    match e {
        Expression::Identifier(i) => {
            if let Some(lit) = consts.get(&i.name) {
                *e = lit.clone();
            }
        }
        Expression::Logical(l) => {
            substitute(&mut l.left, consts);
            substitute(&mut l.right, consts);
        }
        Expression::Binary(b) => {
            substitute(&mut b.left, consts);
            substitute(&mut b.right, consts);
        }
        Expression::Conditional(c) => {
            substitute(&mut c.test, consts);
            substitute(&mut c.consequent, consts);
            substitute(&mut c.alternate, consts);
        }
        Expression::Call(c) => {
            substitute(&mut c.callee, consts);
            for a in &mut c.arguments {
                if let Argument::Expression(e) = a {
                    substitute(e, consts);
                }
            }
        }
        Expression::Member(m) => substitute(&mut m.object, consts),
        Expression::Unary(u) => substitute(&mut u.argument, consts),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                substitute(e, consts);
            }
        }
        Expression::Template(t) => {
            for ex in &mut t.expressions {
                substitute(ex, consts);
            }
        }
        Expression::Paren(p) => substitute(&mut p.expression, consts),
        _ => {}
    }
}

fn fold(e: &mut Expression) {
    match e {
        Expression::Logical(l) => {
            fold(&mut l.left);
            fold(&mut l.right);
            // `LIT ?? X` → LIT when LIT is non-nullish.
            if matches!(l.operator, LogicalOperator::Coalesce) {
                if let Some(true) = is_non_nullish_literal(&l.left) {
                    let left = std::mem::replace(
                        &mut l.left,
                        Expression::Identifier(Identifier {
                            name: String::new(),
                            span: Span::ZERO,
                        }),
                    );
                    *e = left;
                }
            }
        }
        Expression::Binary(b) => {
            fold(&mut b.left);
            fold(&mut b.right);
        }
        Expression::Conditional(c) => {
            fold(&mut c.test);
            fold(&mut c.consequent);
            fold(&mut c.alternate);
        }
        Expression::Call(c) => {
            fold(&mut c.callee);
            for a in &mut c.arguments {
                if let Argument::Expression(e) = a {
                    fold(e);
                }
            }
            // Math.X(literal-numbers...) → literal number.
            if let Some(folded) = try_fold_math_call(c) {
                *e = folded;
            }
        }
        Expression::Member(m) => fold(&mut m.object),
        Expression::Unary(u) => fold(&mut u.argument),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                fold(e);
            }
        }
        Expression::Paren(p) => {
            fold(&mut p.expression);
            // Drop the paren wrapper if its inner is already a literal.
            if matches!(p.expression, Expression::Literal(_)) {
                let inner = std::mem::replace(
                    &mut p.expression,
                    Expression::Identifier(Identifier {
                        name: String::new(),
                        span: Span::ZERO,
                    }),
                );
                *e = inner;
            }
        }
        _ => {}
    }
}

/// `Math.X(literal_numbers...)` → number literal, when `X` is a known pure
/// math function. Mirrors the `globals` whitelist in
/// `packages/svelte/src/compiler/phases/scope.js:26-74`.
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
        "trunc" if nums.len() == 1 => nums[0].trunc(),
        "sign" if nums.len() == 1 => nums[0].signum(),
        "sqrt" if nums.len() == 1 => nums[0].sqrt(),
        "cbrt" if nums.len() == 1 => nums[0].cbrt(),
        "pow" if nums.len() == 2 => nums[0].powf(nums[1]),
        "atan2" if nums.len() == 2 => nums[0].atan2(nums[1]),
        "log" if nums.len() == 1 => nums[0].ln(),
        "log10" if nums.len() == 1 => nums[0].log10(),
        "log2" if nums.len() == 1 => nums[0].log2(),
        "log1p" if nums.len() == 1 => nums[0].ln_1p(),
        "exp" if nums.len() == 1 => nums[0].exp(),
        "expm1" if nums.len() == 1 => nums[0].exp_m1(),
        "sin" if nums.len() == 1 => nums[0].sin(),
        "cos" if nums.len() == 1 => nums[0].cos(),
        "tan" if nums.len() == 1 => nums[0].tan(),
        "asin" if nums.len() == 1 => nums[0].asin(),
        "acos" if nums.len() == 1 => nums[0].acos(),
        "atan" if nums.len() == 1 => nums[0].atan(),
        "sinh" if nums.len() == 1 => nums[0].sinh(),
        "cosh" if nums.len() == 1 => nums[0].cosh(),
        "tanh" if nums.len() == 1 => nums[0].tanh(),
        "asinh" if nums.len() == 1 => nums[0].asinh(),
        "acosh" if nums.len() == 1 => nums[0].acosh(),
        "atanh" if nums.len() == 1 => nums[0].atanh(),
        "fround" if nums.len() == 1 => nums[0] as f32 as f64,
        "imul" if nums.len() == 2 => ((nums[0] as i32).wrapping_mul(nums[1] as i32)) as f64,
        "clz32" if nums.len() == 1 => (nums[0] as u32).leading_zeros() as f64,
        _ => return None,
    };
    Some(Expression::Literal(Box::new(Literal::Number(NumberLiteral {
        value: result,
        raw: Some(format_num(result)),
        span: Span::ZERO,
    }))))
}

fn format_num(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() };
    }
    if n.fract() == 0.0 && n.abs() < 1e21 {
        return format!("{}", n as i64);
    }
    format!("{n}")
}

/// `Some(true)` if `e` is a non-null/non-undefined literal. `Some(false)`
/// if it's explicitly null/undefined. `None` for non-literal.
fn is_non_nullish_literal(e: &Expression) -> Option<bool> {
    match e {
        Expression::Literal(lit) => match lit.as_ref() {
            Literal::Null(_) => Some(false),
            _ => Some(true),
        },
        Expression::Identifier(i) if i.name == "undefined" => Some(false),
        _ => None,
    }
}

struct Ctx {
    uses_props: bool,
}

fn rewrite_statement(s: &mut Statement, ctx: &mut Ctx) {
    use Statement as S;
    match s {
        S::Variable(v) => {
            for d in &mut v.declarations {
                if let Some(init) = &mut d.init {
                    rewrite_expression(init, ctx);
                }
            }
        }
        S::Expression(e) => rewrite_expression(&mut e.expression, ctx),
        S::Block(b) => {
            for s in &mut b.body {
                rewrite_statement(s, ctx);
            }
        }
        S::Return(r) => {
            if let Some(a) = &mut r.argument {
                rewrite_expression(a, ctx);
            }
        }
        S::If(i) => {
            rewrite_expression(&mut i.test, ctx);
            rewrite_statement(&mut i.consequent, ctx);
            if let Some(a) = &mut i.alternate {
                rewrite_statement(a, ctx);
            }
        }
        S::For(f) => {
            if let Some(init) = &mut f.init {
                rewrite_for_init(init, ctx);
            }
            if let Some(t) = &mut f.test {
                rewrite_expression(t, ctx);
            }
            if let Some(u) = &mut f.update {
                rewrite_expression(u, ctx);
            }
            rewrite_statement(&mut f.body, ctx);
        }
        S::ForIn(f) => {
            rewrite_for_init(&mut f.left, ctx);
            rewrite_expression(&mut f.right, ctx);
            rewrite_statement(&mut f.body, ctx);
        }
        S::ForOf(f) => {
            rewrite_for_init(&mut f.left, ctx);
            rewrite_expression(&mut f.right, ctx);
            rewrite_statement(&mut f.body, ctx);
        }
        S::While(w) => {
            rewrite_expression(&mut w.test, ctx);
            rewrite_statement(&mut w.body, ctx);
        }
        S::DoWhile(w) => {
            rewrite_statement(&mut w.body, ctx);
            rewrite_expression(&mut w.test, ctx);
        }
        S::Switch(sw) => {
            rewrite_expression(&mut sw.discriminant, ctx);
            for c in &mut sw.cases {
                if let Some(t) = &mut c.test {
                    rewrite_expression(t, ctx);
                }
                for s in &mut c.consequent {
                    rewrite_statement(s, ctx);
                }
            }
        }
        S::Try(t) => {
            for s in &mut t.block.body {
                rewrite_statement(s, ctx);
            }
            if let Some(h) = &mut t.handler {
                for s in &mut h.body.body {
                    rewrite_statement(s, ctx);
                }
            }
            if let Some(f) = &mut t.finalizer {
                for s in &mut f.body {
                    rewrite_statement(s, ctx);
                }
            }
        }
        S::Throw(t) => rewrite_expression(&mut t.argument, ctx),
        S::Function(f) => {
            for s in &mut f.body.body {
                rewrite_statement(s, ctx);
            }
        }
        S::Labeled(l) => rewrite_statement(&mut l.body, ctx),
        S::With(w) => {
            rewrite_expression(&mut w.object, ctx);
            rewrite_statement(&mut w.body, ctx);
        }
        S::ExportNamed(e) => {
            if let Some(d) = &mut e.declaration {
                rewrite_statement(d, ctx);
            }
        }
        S::ExportDefault(e) => match &mut e.declaration {
            ExportDefault::Function(f) => {
                for s in &mut f.body.body {
                    rewrite_statement(s, ctx);
                }
            }
            ExportDefault::Class(c) => rewrite_class_body(c, ctx),
            ExportDefault::Expression(e) => rewrite_expression(e, ctx),
        },
        S::Class(c) => rewrite_class_body(c, ctx),
        _ => {}
    }
}

/// Rewrite a class body in-place: erase `$state(x)` field initializers and
/// transform `$derived(x)` fields into private fields with getter/setter
/// accessor pairs. Ports
/// `packages/svelte/src/compiler/phases/3-transform/server/visitors/ClassBody.js`.
fn rewrite_class_body(c: &mut ClassDeclaration, ctx: &mut Ctx) {
    let mut new_members: Vec<ClassMember> = Vec::with_capacity(c.body.body.len());
    for member in std::mem::take(&mut c.body.body) {
        match member {
            ClassMember::Property(mut p) => {
                let kind = property_rune_kind(&p.value);
                match kind {
                    Some(ClassFieldRune::State) => {
                        // `name = $state(x)` → `name = x`; `name = $state()` → `name;`
                        let inner = property_rune_inner(p.value.as_ref().unwrap());
                        p.value = inner;
                        new_members.push(ClassMember::Property(p));
                    }
                    Some(ClassFieldRune::Derived(by)) => {
                        // Rename key to private and wrap value as $.derived(...).
                        let public_key = match &p.key {
                            PropertyKey::Identifier(id) => id.name.clone(),
                            PropertyKey::Private(pi) => pi.name.clone(),
                            _ => {
                                // Unsupported key shape — leave as-is.
                                new_members.push(ClassMember::Property(p));
                                continue;
                            }
                        };
                        let was_private = matches!(p.key, PropertyKey::Private(_));
                        let private_key = if was_private {
                            public_key.clone()
                        } else {
                            public_key.clone()
                        };
                        let arg = property_rune_inner(p.value.as_ref().unwrap())
                            .unwrap_or_else(undefined_expr);
                        let derived_expr = if by {
                            derived_call(arg)
                        } else {
                            derived_call(Expression::Arrow(Box::new(ArrowFunctionExpression {
                                params: Vec::new(),
                                body: ArrowBody::Expression(arg),
                                r#async: false,
                                span: Span::ZERO,
                            })))
                        };
                        p.key = PropertyKey::Private(PrivateIdentifier {
                            name: private_key.clone(),
                            span: Span::ZERO,
                        });
                        p.value = Some(derived_expr);
                        new_members.push(ClassMember::Property(p));
                        if !was_private {
                            // Add getter and setter accessor pair.
                            new_members.push(make_derived_getter(&public_key, &private_key));
                            new_members.push(make_derived_setter(&public_key, &private_key));
                        }
                    }
                    None => {
                        // Plain field — recurse into its initializer for any nested runes.
                        if let Some(v) = &mut p.value {
                            rewrite_expression(v, ctx);
                        }
                        new_members.push(ClassMember::Property(p));
                    }
                }
            }
            ClassMember::Method(mut m) => {
                for s in &mut m.value.body.body {
                    rewrite_statement(s, ctx);
                }
                new_members.push(ClassMember::Method(m));
            }
            ClassMember::StaticBlock(mut sb) => {
                for s in &mut sb.body {
                    rewrite_statement(s, ctx);
                }
                new_members.push(ClassMember::StaticBlock(sb));
            }
        }
    }
    c.body.body = new_members;
}

enum ClassFieldRune {
    State,
    /// `$derived(...)` (false) vs `$derived.by(...)` (true).
    Derived(bool),
}

fn property_rune_kind(value: &Option<Expression>) -> Option<ClassFieldRune> {
    let e = value.as_ref()?;
    let Expression::Call(c) = e else { return None };
    let kp = global_keypath(&c.callee)?;
    match kp.as_str() {
        "$state" | "$state.raw" | "$state.eager" => Some(ClassFieldRune::State),
        "$derived" => Some(ClassFieldRune::Derived(false)),
        "$derived.by" => Some(ClassFieldRune::Derived(true)),
        _ => None,
    }
}

fn property_rune_inner(e: &Expression) -> Option<Expression> {
    let Expression::Call(c) = e else { return None };
    c.arguments.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e.clone()),
        _ => None,
    })
}

/// `get NAME() { return this.#PRIVATE(); }`
fn make_derived_getter(public_name: &str, private_name: &str) -> ClassMember {
    let body = vec![Statement::Return(Box::new(ReturnStatement {
        argument: Some(Expression::Call(Box::new(CallExpression {
            callee: Expression::Member(Box::new(MemberExpression {
                object: Expression::This(Span::ZERO),
                property: MemberProperty::Private(PrivateIdentifier {
                    name: private_name.to_string(),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            })),
            arguments: Vec::new(),
            optional: false,
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: public_name.to_string(),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: Vec::new(),
            body: BlockStatement { body, span: Span::ZERO },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        },
        kind: MethodKind::Get,
        computed: false,
        r#static: false,
        span: Span::ZERO,
    }))
}

/// `set NAME($$value) { return this.#PRIVATE($$value); }`
fn make_derived_setter(public_name: &str, private_name: &str) -> ClassMember {
    let body = vec![Statement::Return(Box::new(ReturnStatement {
        argument: Some(Expression::Call(Box::new(CallExpression {
            callee: Expression::Member(Box::new(MemberExpression {
                object: Expression::This(Span::ZERO),
                property: MemberProperty::Private(PrivateIdentifier {
                    name: private_name.to_string(),
                    span: Span::ZERO,
                }),
                computed: false,
                optional: false,
                span: Span::ZERO,
            })),
            arguments: vec![Argument::Expression(Expression::Identifier(Identifier {
                name: "$$value".to_string(),
                span: Span::ZERO,
            }))],
            optional: false,
            span: Span::ZERO,
        }))),
        span: Span::ZERO,
    }))];
    ClassMember::Method(Box::new(MethodDefinition {
        key: PropertyKey::Identifier(Identifier {
            name: public_name.to_string(),
            span: Span::ZERO,
        }),
        value: FunctionExpression {
            id: None,
            params: vec![Pattern::Identifier(Identifier {
                name: "$$value".to_string(),
                span: Span::ZERO,
            })],
            body: BlockStatement { body, span: Span::ZERO },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        },
        kind: MethodKind::Set,
        computed: false,
        r#static: false,
        span: Span::ZERO,
    }))
}

fn rewrite_for_init(init: &mut ForInit, ctx: &mut Ctx) {
    match init {
        ForInit::Declaration(d) => {
            for d in &mut d.declarations {
                if let Some(init) = &mut d.init {
                    rewrite_expression(init, ctx);
                }
            }
        }
        ForInit::Expression(e) => rewrite_expression(e, ctx),
    }
}

fn rewrite_expression(e: &mut Expression, ctx: &mut Ctx) {
    // Try the rune-rewrite first; if it fires, the expression is replaced.
    if let Some(replacement) = try_rewrite_rune_call(e, ctx) {
        *e = replacement;
        return;
    }

    // Otherwise recurse into children.
    use Expression as E;
    match e {
        E::Identifier(_) | E::Literal(_) | E::This(_) | E::Super(_) | E::Raw(_) => {}
        E::Template(t) => {
            for ex in &mut t.expressions {
                rewrite_expression(ex, ctx);
            }
        }
        E::Array(a) => {
            for el in &mut a.elements {
                match el {
                    ArrayElement::Expression(e) => rewrite_expression(e, ctx),
                    ArrayElement::Spread(s) => rewrite_expression(&mut s.argument, ctx),
                    ArrayElement::Elision => {}
                }
            }
        }
        E::Object(o) => {
            for m in &mut o.properties {
                match m {
                    ObjectMember::Property(p) => {
                        rewrite_expression(&mut p.value, ctx);
                    }
                    ObjectMember::Spread(s) => rewrite_expression(&mut s.argument, ctx),
                }
            }
        }
        E::Arrow(a) => match &mut a.body {
            ArrowBody::Block(b) => {
                for s in &mut b.body {
                    rewrite_statement(s, ctx);
                }
            }
            ArrowBody::Expression(e) => rewrite_expression(e, ctx),
        },
        E::Function(f) => {
            for s in &mut f.body.body {
                rewrite_statement(s, ctx);
            }
        }
        E::Class(_) => {}
        E::Member(m) => {
            rewrite_expression(&mut m.object, ctx);
            if let MemberProperty::Expression(e) = &mut m.property {
                rewrite_expression(e, ctx);
            }
        }
        E::Call(c) => {
            rewrite_expression(&mut c.callee, ctx);
            for a in &mut c.arguments {
                if let Argument::Expression(e) = a {
                    rewrite_expression(e, ctx);
                } else if let Argument::Spread(s) = a {
                    rewrite_expression(&mut s.argument, ctx);
                }
            }
        }
        E::New(n) => {
            rewrite_expression(&mut n.callee, ctx);
            for a in &mut n.arguments {
                if let Argument::Expression(e) = a {
                    rewrite_expression(e, ctx);
                } else if let Argument::Spread(s) = a {
                    rewrite_expression(&mut s.argument, ctx);
                }
            }
        }
        E::Binary(b) => {
            rewrite_expression(&mut b.left, ctx);
            rewrite_expression(&mut b.right, ctx);
        }
        E::Logical(l) => {
            rewrite_expression(&mut l.left, ctx);
            rewrite_expression(&mut l.right, ctx);
        }
        E::Assignment(a) => rewrite_expression(&mut a.right, ctx),
        E::Update(u) => rewrite_expression(&mut u.argument, ctx),
        E::Unary(u) => rewrite_expression(&mut u.argument, ctx),
        E::Conditional(c) => {
            rewrite_expression(&mut c.test, ctx);
            rewrite_expression(&mut c.consequent, ctx);
            rewrite_expression(&mut c.alternate, ctx);
        }
        E::Sequence(s) => {
            for e in &mut s.expressions {
                rewrite_expression(e, ctx);
            }
        }
        E::Spread(s) => rewrite_expression(&mut s.argument, ctx),
        E::Yield(y) => {
            if let Some(a) = &mut y.argument {
                rewrite_expression(a, ctx);
            }
        }
        E::Await(a) => rewrite_expression(&mut a.argument, ctx),
        E::Tagged(t) => {
            rewrite_expression(&mut t.tag, ctx);
            for ex in &mut t.quasi.expressions {
                rewrite_expression(ex, ctx);
            }
        }
        E::Paren(p) => rewrite_expression(&mut p.expression, ctx),
        E::Meta(_) => {}
    }
}

fn try_rewrite_rune_call(e: &Expression, ctx: &mut Ctx) -> Option<Expression> {
    let Expression::Call(c) = e else { return None };
    let keypath = global_keypath(&c.callee)?;
    match keypath.as_str() {
        // $state(x) / $state.raw(x) → x  (no-arg → undefined)
        "$state" | "$state.raw" | "$state.eager" => {
            Some(first_arg_or_undefined(&c.arguments))
        }
        // $derived(EXPR) → $.derived(() => EXPR)
        // $derived.by(fn) → $.derived(fn) (the .by form takes a function directly)
        "$derived" => Some(wrap_derived_arrow(&c.arguments)),
        "$derived.by" => Some(wrap_derived_call(&c.arguments)),
        // $bindable(default) → default  (no-arg → undefined)
        "$bindable" => Some(first_arg_or_undefined(&c.arguments)),
        // $effect(...) / $inspect(...) / $host() → undefined
        "$effect" | "$effect.pre" | "$effect.root" | "$effect.tracking" | "$effect.pending"
        | "$inspect" | "$inspect.trace" | "$host" => Some(undefined_expr()),
        // $props() → $$props
        "$props" => {
            ctx.uses_props = true;
            Some(Expression::Identifier(Identifier {
                name: "$$props".to_string(),
                span: Span::ZERO,
            }))
        }
        _ => None,
    }
}

/// `$derived(EXPR)` → `$.derived(() => EXPR)`. Wraps the user expression
/// in a zero-arg arrow so the derived computation is lazy.
fn wrap_derived_arrow(args: &[Argument]) -> Expression {
    let inner = first_arg_or_undefined(args);
    let arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: ArrowBody::Expression(inner),
        r#async: false,
        span: Span::ZERO,
    }));
    derived_call(arrow)
}

/// `$derived.by(fn)` → `$.derived(fn)`. The `.by` form takes a function directly.
fn wrap_derived_call(args: &[Argument]) -> Expression {
    let inner = first_arg_or_undefined(args);
    derived_call(inner)
}

fn derived_call(arg: Expression) -> Expression {
    Expression::Call(Box::new(CallExpression {
        callee: Expression::Member(Box::new(MemberExpression {
            object: Expression::Identifier(Identifier {
                name: "$".to_string(),
                span: Span::ZERO,
            }),
            property: MemberProperty::Identifier(Identifier {
                name: "derived".to_string(),
                span: Span::ZERO,
            }),
            computed: false,
            optional: false,
            span: Span::ZERO,
        })),
        arguments: vec![Argument::Expression(arg)],
        optional: false,
        span: Span::ZERO,
    }))
}

fn first_arg_or_undefined(args: &[Argument]) -> Expression {
    args.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e.clone()),
        _ => None,
    }).unwrap_or_else(undefined_expr)
}

fn undefined_expr() -> Expression {
    Expression::Identifier(Identifier {
        name: "undefined".to_string(),
        span: Span::ZERO,
    })
}

fn global_keypath(e: &Expression) -> Option<String> {
    let mut joined = String::new();
    let mut cur = e;
    while let Expression::Member(m) = cur {
        if m.computed {
            return None;
        }
        let name = match &m.property {
            MemberProperty::Identifier(i) => &i.name,
            _ => return None,
        };
        joined = format!(".{name}{joined}");
        cur = &m.object;
    }
    if let Expression::Identifier(i) = cur {
        Some(format!("{}{joined}", i.name))
    } else {
        None
    }
}
