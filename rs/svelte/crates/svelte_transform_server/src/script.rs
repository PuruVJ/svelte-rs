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
    /// Legacy `export let NAME[=default]` bindings, in source order. The
    /// caller emits `$.bind_props($$props, { NAME, ... })` at the end of
    /// the function body for these.
    pub legacy_export_props: Vec<String>,
    /// Names of bindings initialized with `$state(...)` (the no-arg form
    /// reads as `void 0`, so the runtime needs to wrap usages as
    /// potentially-nullish — e.g. `let Component = $state()` → wrap
    /// `<Component />` in `if (Component) { ... } else { ... }`).
    pub state_bindings: HashSet<String>,
    /// Names of top-level `let/const/var` declarations and imports — used
    /// to resolve store-subscription references like `$X` (where `X` is one
    /// of these). Includes names that survive the rune-rewrite pass.
    pub top_bindings: HashSet<String>,
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
    /// All `let` / `const` bindings declared in the script (excluding
    /// function declarations). Used to decide whether a top-level block
    /// needs the `$$renderer.async_block([...], ...)` wrap.
    pub script_let_bindings: HashSet<String>,
    /// Per-binding-name, the group index of the async statement that touched
    /// it (writes OR reads inside the async init's call expressions). The
    /// template uses these as `$$promises[idx]` blockers. Mirrors upstream's
    /// `binding.blocker` mechanism in `2-analyze/index.js::calculate_blockers`.
    pub blocker_bindings: HashMap<String, usize>,
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
    if !has_top_level_await(&p) && !has_async_derived_init(&p) {
        return None;
    }

    // Statements before the first async stay as-is in the function body
    // (they execute synchronously before the run-array fires). Statements
    // from the first async onwards go through the hoist + run mechanism.
    let first_async_idx = body.iter().position(|s| {
        if let Statement::Variable(v) = s {
            v.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, |i| {
                    expr_has_top_level_await(i) || rewrite_async_derived(i).is_some()
                })
            })
        } else if let Statement::Expression(e) = s {
            expr_has_top_level_await(&e.expression)
        } else {
            false
        }
    })?;
    let pre_async: Vec<Statement> = body[..first_async_idx].to_vec();
    let body = &body[first_async_idx..];

    // Gather all let/const bindings to hoist + classify each statement.
    let mut hoisted_names: Vec<String> = Vec::new();
    let mut hoisted_spans: Vec<Span> = Vec::new();
    enum Lowered {
        AsyncSet { name: String, init: Expression },
        Sync(Statement),
        AwaitExpr(Expression),
    }
    let mut lowered: Vec<Lowered> = Vec::new();

    for s in body {
        match s {
            Statement::Variable(v) => {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        hoisted_names.push(id.name.clone());
                        hoisted_spans.push(id.span);
                        let init = d.init.clone().unwrap_or_else(undefined_expr);
                        // `$.derived(() => await E)` pattern (post rune-erase
                        // form of `let X = $derived(await E)`) → convert to
                        // `await $.async_derived(() => E)` for the async-set
                        // arrow.
                        if let Some(rewritten) = rewrite_async_derived(&init) {
                            lowered.push(Lowered::AsyncSet {
                                name: id.name.clone(),
                                init: rewritten,
                            });
                            continue;
                        }
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
                // `await EXPR;` (top-level await expression statement) — each
                // gets its own thunk in the run array (not merged with sibling
                // sync stmts). The thunk body unwraps to just `EXPR`.
                if let Expression::Await(a) = &e.expression {
                    lowered.push(Lowered::AwaitExpr(a.argument.clone()));
                    continue;
                }
                lowered.push(Lowered::Sync(s.clone()));
            }
            // Function declarations pass through untouched (they don't
            // participate in the async hoisting).
            Statement::Function(_) => {
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
                // `await EXPR;` (top-level) unwraps to just `EXPR` — the
                // `$$renderer.run` handler awaits each thunk internally,
                // so the thunk body should be the awaited expression.
                let body_expr = match es.expression {
                    Expression::Await(a) => a.argument,
                    other => other,
                };
                groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(body_expr),
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
            Lowered::AwaitExpr(arg) => {
                if !current_sync.is_empty() {
                    flush_sync(&mut groups, &mut current_sync);
                }
                groups.push(Expression::Arrow(Box::new(ArrowFunctionExpression {
                    params: Vec::new(),
                    body: ArrowBody::Expression(arg),
                    r#async: false,
                    span: Span::ZERO,
                })));
                last_was_async = false;
            }
        }
    }
    // Trailing sync group: only when there are actual sync statements after
    // the last async. Empty trailing groups are not emitted.
    let _ = last_was_async;
    if !current_sync.is_empty() {
        flush_sync(&mut groups, &mut current_sync);
    }

    let last_group_idx = if groups.is_empty() { 0 } else { groups.len() - 1 };

    // var X, Y, Z;
    let mut setup_stmts: Vec<Statement> = Vec::new();
    // Pre-async statements (functions + plain sync bindings before the first
    // await) come first, untouched.
    setup_stmts.extend(pre_async.clone());
    if !hoisted_names.is_empty() {
        let decls: Vec<VariableDeclarator> = hoisted_names
            .iter()
            .enumerate()
            .map(|(i, n)| VariableDeclarator {
                id: Pattern::Identifier(Identifier {
                    name: n.clone(),
                    span: hoisted_spans.get(i).copied().unwrap_or(Span::ZERO),
                }),
                init: None,
                span: hoisted_spans.get(i).copied().unwrap_or(Span::ZERO),
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
    // Collect all script let/const bindings (pre-async + hoisted), excluding
    // function declarations.
    let mut script_let_bindings: HashSet<String> = async_bindings.clone();
    for s in pre_async.iter().chain(body.iter()) {
        if let Statement::Variable(v) = s {
            for d in &v.declarations {
                if let Pattern::Identifier(id) = &d.id {
                    script_let_bindings.insert(id.name.clone());
                }
            }
        }
    }
    // Compute blocker_bindings: simulate the group counting and assign
    // `group_idx` per binding. Mirrors upstream's
    // `2-analyze/index.js::calculate_blockers`:
    //   - async declarators flush any pending sync group, then occupy their
    //     own async group index. Touched identifiers (writes via the
    //     CallExpression rule) ALSO get this index.
    //   - sync declarators that come AFTER the first async one accumulate
    //     into the upcoming sync group; their blocker is the index THAT sync
    //     group will land on once flushed.
    let mut blocker_bindings: HashMap<String, usize> = HashMap::new();
    {
        let mut groups_count: usize = 0;
        let mut sync_pending: bool = false;
        let mut awaited_seen: bool = false;
        for s in body.iter() {
            if let Statement::Variable(v) = s {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        let init = d.init.as_ref();
                        let is_async = init.map_or(false, |i| {
                            expr_has_top_level_await(i) || rewrite_async_derived(i).is_some()
                        });
                        if is_async {
                            if sync_pending {
                                groups_count += 1;
                                sync_pending = false;
                            }
                            let idx = groups_count;
                            blocker_bindings
                                .entry(id.name.clone())
                                .or_insert(idx);
                            if let Some(init) = init {
                                let mut touched: HashSet<String> = HashSet::new();
                                collect_touched_in_expr(init, &mut touched);
                                for name in touched {
                                    if script_let_bindings.contains(&name) {
                                        blocker_bindings.entry(name).or_insert(idx);
                                    }
                                }
                            }
                            groups_count += 1;
                            awaited_seen = true;
                        } else {
                            // After any await, sync declarators also get a
                            // blocker — the index of the pending sync group
                            // they'll be flushed into.
                            if awaited_seen {
                                blocker_bindings
                                    .entry(id.name.clone())
                                    .or_insert(groups_count);
                            }
                            sync_pending = true;
                        }
                    }
                }
            } else if matches!(s, Statement::Function(_)) {
                // Function declarations are sync; they go to setup, not groups.
                // Don't count them.
            } else {
                sync_pending = true;
            }
        }
    }
    Some(AsyncInfo {
        setup_stmts,
        async_bindings,
        last_group_idx,
        script_let_bindings,
        blocker_bindings,
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

/// Detect any `$.derived(ARROW)` where ARROW's body contains await — the
/// post rune-erase form of `let X = $derived(await E)`. Used to trigger the
/// async transform even when the script has no other top-level await.
fn has_async_derived_init(p: &Program) -> bool {
    p.body.iter().any(|s| {
        if let Statement::Variable(v) = s {
            v.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, |i| rewrite_async_derived(i).is_some())
            })
        } else {
            false
        }
    })
}

/// If `e` is `$.derived(ARROW)` with `ARROW.body` containing top-level await,
/// return `await $.async_derived(NEW_ARROW)` where NEW_ARROW strips the outer
/// await from the body. Otherwise None.
fn rewrite_async_derived(e: &Expression) -> Option<Expression> {
    let Expression::Call(c) = e else { return None };
    // Must be `$.derived(...)` (rune-erase output).
    if global_keypath(&c.callee).as_deref() != Some("$.derived") {
        return None;
    }
    let arg = c.arguments.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e),
        _ => None,
    })?;
    let arrow = match arg {
        Expression::Arrow(a) => a,
        _ => return None,
    };
    // The wrapping arrow from `$derived(EXPR)` rune-erase is never async; if
    // the user used `$derived.by(async () => …)` (no1's case in
    // async-in-derived) the arrow IS async and we leave it untouched.
    if arrow.r#async {
        return None;
    }
    // Inspect ARROW.body for top-level await. Two shapes:
    //   - body is exactly `await X`   → `await $.async_derived(() => X)`
    //   - body has nested top-level
    //     awaits (e.g. `foo(await 1)`)→ `await $.async_derived(async () => BODY)`
    let body = match &arrow.body {
        ArrowBody::Expression(e) => e,
        _ => return None,
    };
    let (new_body, new_async) = if let Expression::Await(a) = body {
        (ArrowBody::Expression(a.argument.clone()), false)
    } else if expr_has_top_level_await(body) {
        (ArrowBody::Expression(body.clone()), true)
    } else {
        return None;
    };
    let new_arrow = Expression::Arrow(Box::new(ArrowFunctionExpression {
        params: Vec::new(),
        body: new_body,
        r#async: new_async,
        span: Span::ZERO,
    }));
    let async_derived_call = t::call(
        t::member_id(t::id("$"), "async_derived"),
        vec![new_arrow],
    );
    Some(Expression::Await(Box::new(AwaitExpression {
        argument: async_derived_call,
        span: Span::ZERO,
    })))
}

/// Walks `e` and collects every identifier reference into `out`, but does
/// NOT descend into nested function/arrow bodies (mirrors upstream's `touch`
/// which stops at function boundaries). This is the same set of bindings
/// upstream marks as "touched" by an async declarator's init for blocker
/// computation.
fn collect_touched_in_expr(e: &Expression, out: &mut HashSet<String>) {
    match e {
        Expression::Identifier(id) => {
            // Skip `undefined` etc. — these aren't real bindings.
            if id.name != "undefined" {
                out.insert(id.name.clone());
            }
        }
        Expression::Call(c) => {
            collect_touched_in_expr(&c.callee, out);
            for a in &c.arguments {
                match a {
                    Argument::Expression(e) => collect_touched_in_expr(e, out),
                    Argument::Spread(s) => collect_touched_in_expr(&s.argument, out),
                }
            }
        }
        Expression::Member(m) => {
            collect_touched_in_expr(&m.object, out);
            if let MemberProperty::Expression(e) = &m.property {
                if m.computed {
                    collect_touched_in_expr(e, out);
                }
            }
        }
        Expression::Binary(b) => {
            collect_touched_in_expr(&b.left, out);
            collect_touched_in_expr(&b.right, out);
        }
        Expression::Logical(l) => {
            collect_touched_in_expr(&l.left, out);
            collect_touched_in_expr(&l.right, out);
        }
        Expression::Unary(u) => collect_touched_in_expr(&u.argument, out),
        Expression::Update(u) => collect_touched_in_expr(&u.argument, out),
        Expression::Assignment(a) => {
            if let AssignmentTarget::Expression(e) = &a.left {
                collect_touched_in_expr(e, out);
            }
            collect_touched_in_expr(&a.right, out);
        }
        Expression::Conditional(c) => {
            collect_touched_in_expr(&c.test, out);
            collect_touched_in_expr(&c.consequent, out);
            collect_touched_in_expr(&c.alternate, out);
        }
        Expression::Paren(p) => collect_touched_in_expr(&p.expression, out),
        Expression::Sequence(s) => {
            for e in &s.expressions {
                collect_touched_in_expr(e, out);
            }
        }
        Expression::Spread(s) => collect_touched_in_expr(&s.argument, out),
        Expression::Await(a) => collect_touched_in_expr(&a.argument, out),
        Expression::Array(a) => {
            for el in &a.elements {
                if let ArrayElement::Expression(e) = el {
                    collect_touched_in_expr(e, out);
                }
            }
        }
        Expression::Object(o) => {
            for p in &o.properties {
                match p {
                    ObjectMember::Property(prop) => {
                        if prop.computed {
                            if let PropertyKey::Expression(e) = &prop.key {
                                collect_touched_in_expr(e, out);
                            }
                        }
                        collect_touched_in_expr(&prop.value, out);
                    }
                    ObjectMember::Spread(s) => {
                        collect_touched_in_expr(&s.argument, out);
                    }
                }
            }
        }
        Expression::Template(t) => {
            for e in &t.expressions {
                collect_touched_in_expr(e, out);
            }
        }
        Expression::Tagged(t) => {
            collect_touched_in_expr(&t.tag, out);
            for e in &t.quasi.expressions {
                collect_touched_in_expr(e, out);
            }
        }
        Expression::New(n) => {
            collect_touched_in_expr(&n.callee, out);
            for a in &n.arguments {
                match a {
                    Argument::Expression(e) => collect_touched_in_expr(e, out),
                    Argument::Spread(s) => collect_touched_in_expr(&s.argument, out),
                }
            }
        }
        // Descend into function/arrow bodies. Upstream's `touch` does this
        // because the async statement could call the function eagerly, and
        // any binding read inside it counts as touched.
        Expression::Arrow(a) => {
            match &a.body {
                ArrowBody::Expression(e) => collect_touched_in_expr(e, out),
                ArrowBody::Block(b) => {
                    for s in &b.body {
                        collect_touched_in_stmt(s, out);
                    }
                }
            }
        }
        Expression::Function(f) => {
            for s in &f.body.body {
                collect_touched_in_stmt(s, out);
            }
        }
        _ => {}
    }
}

fn collect_touched_in_stmt(s: &Statement, out: &mut HashSet<String>) {
    match s {
        Statement::Expression(e) => collect_touched_in_expr(&e.expression, out),
        Statement::Return(r) => {
            if let Some(a) = &r.argument {
                collect_touched_in_expr(a, out);
            }
        }
        Statement::Variable(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    collect_touched_in_expr(init, out);
                }
            }
        }
        Statement::If(i) => {
            collect_touched_in_expr(&i.test, out);
            collect_touched_in_stmt(&i.consequent, out);
            if let Some(a) = &i.alternate {
                collect_touched_in_stmt(a, out);
            }
        }
        Statement::Block(b) => {
            for s in &b.body {
                collect_touched_in_stmt(s, out);
            }
        }
        Statement::For(f) => {
            if let Some(init) = &f.init {
                match init {
                    ForInit::Declaration(v) => {
                        for d in &v.declarations {
                            if let Some(i) = &d.init {
                                collect_touched_in_expr(i, out);
                            }
                        }
                    }
                    ForInit::Expression(e) => collect_touched_in_expr(e, out),
                }
            }
            if let Some(t) = &f.test {
                collect_touched_in_expr(t, out);
            }
            if let Some(u) = &f.update {
                collect_touched_in_expr(u, out);
            }
            collect_touched_in_stmt(&f.body, out);
        }
        _ => {}
    }
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
    let mut state_bindings: HashSet<String> = HashSet::new();
    let mut single_id_props: Option<String> = None;
    let mut has_class_with_runes = false;
    for s in &p.body {
        collect_rune_bindings_stmt(s, &mut rune_bindings);
        collect_derived_bindings_stmt(s, &mut derived_bindings);
        collect_state_bindings_stmt(s, &mut state_bindings);
        check_single_id_props_stmt(s, &mut single_id_props);
        if stmt_has_class_with_runes(s) {
            has_class_with_runes = true;
        }
    }
    let mut ctx = Ctx { uses_props: false };
    for s in &mut p.body {
        rewrite_statement(s, &mut ctx);
    }

    // Lower legacy `export let X[=default]` into plain `let` declarations
    // that read from `$$props`. Preserves source order so the trailing
    // `$.bind_props($$props, { X, Y, ... })` can be emitted by the caller
    // with bindings in the right order.
    let mut legacy_export_props: Vec<String> = Vec::new();
    let mut new_body: Vec<Statement> = Vec::with_capacity(p.body.len());
    for stmt in std::mem::take(&mut p.body) {
        match stmt {
            Statement::ExportNamed(e) if e.declaration.is_some() => {
                // Only handle `export let NAME[=DEFAULT]` shape. Anything
                // else (export class, export function) — strip the `export`
                // and emit the inner declaration unchanged.
                let mut owned = e;
                let decl = owned.declaration.take();
                match decl {
                    Some(Statement::Variable(v))
                        if matches!(v.kind, VariableKind::Let | VariableKind::Var) =>
                    {
                        let v = *v;
                        for d in &v.declarations {
                            if let Pattern::Identifier(id) = &d.id {
                                legacy_export_props.push(id.name.clone());
                            }
                        }
                        // Emit one `let X = $$props['X'][, $.fallback(...)]`
                        // per declaration.
                        for d in v.declarations {
                            if let Pattern::Identifier(id) = &d.id {
                                let key = id.name.clone();
                                let read = Expression::Member(Box::new(MemberExpression {
                                    object: t::id("$$props"),
                                    property: MemberProperty::Expression(t::literal_str(&key)),
                                    computed: true,
                                    optional: false,
                                    span: Span::ZERO,
                                }));
                                let init = if let Some(def) = d.init {
                                    // Object/array literal defaults need to
                                    // be wrapped in a thunk (so each instance
                                    // gets its own value) and the third
                                    // `true` arg flags it as "shared default".
                                    let is_obj_like = matches!(
                                        &def,
                                        Expression::Object(_) | Expression::Array(_)
                                    );
                                    if is_obj_like {
                                        let arrow = Expression::Arrow(Box::new(
                                            ArrowFunctionExpression {
                                                params: Vec::new(),
                                                body: ArrowBody::Expression(
                                                    Expression::Paren(Box::new(ParenthesizedExpression {
                                                        expression: def,
                                                        span: Span::ZERO,
                                                    })),
                                                ),
                                                r#async: false,
                                                span: Span::ZERO,
                                            },
                                        ));
                                        t::call(
                                            t::member_id(t::id("$"), "fallback"),
                                            vec![
                                                read,
                                                arrow,
                                                Expression::Literal(Box::new(
                                                    Literal::Boolean(BooleanLiteral {
                                                        value: true,
                                                        span: Span::ZERO,
                                                    }),
                                                )),
                                            ],
                                        )
                                    } else {
                                        t::call(
                                            t::member_id(t::id("$"), "fallback"),
                                            vec![read, def],
                                        )
                                    }
                                } else {
                                    read
                                };
                                new_body.push(t::let_decl(&key, Some(init)));
                            } else {
                                // Destructured `export let { ... }` — pass
                                // through unchanged; not lowered yet.
                                new_body.push(Statement::Variable(Box::new(
                                    VariableDeclaration {
                                        kind: v.kind,
                                        declarations: vec![d],
                                        span: Span::ZERO,
                                    },
                                )));
                            }
                        }
                        ctx.uses_props = true;
                    }
                    Some(other) => {
                        new_body.push(other);
                    }
                    None => {
                        new_body.push(Statement::ExportNamed(owned));
                    }
                }
            }
            other => new_body.push(other),
        }
    }
    p.body = new_body;

    if single_id_props.is_some() || has_class_with_runes {
        ctx.uses_props = true;
    }
    let top_bindings = collect_top_level_bindings(p);
    RewriteInfo {
        uses_props: ctx.uses_props,
        rune_bindings,
        derived_bindings,
        single_id_props,
        has_class_with_runes,
        legacy_export_props,
        state_bindings,
        top_bindings,
    }
}

fn collect_state_bindings_stmt(s: &Statement, out: &mut HashSet<String>) {
    match s {
        Statement::Variable(v) => {
            for d in &v.declarations {
                if let (Pattern::Identifier(id), Some(init)) = (&d.id, &d.init) {
                    if is_state_call(init) {
                        out.insert(id.name.clone());
                    }
                }
            }
        }
        Statement::Block(b) => {
            for s in &b.body {
                collect_state_bindings_stmt(s, out);
            }
        }
        Statement::ExportNamed(e) => {
            if let Some(d) = &e.declaration {
                collect_state_bindings_stmt(d, out);
            }
        }
        _ => {}
    }
}

fn is_state_call(e: &Expression) -> bool {
    let Expression::Call(c) = e else { return false };
    let Some(kp) = global_keypath(&c.callee) else { return false };
    matches!(kp.as_str(), "$state" | "$state.raw" | "$state.eager")
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
    // First pass: collect candidates. In runes mode (any rune declaration
    // present in `skip`), unbound `let`s with literal inits may be folded —
    // upstream's `<h1>Hello, {name}</h1>` collapses `name='world'` into the
    // text. In legacy / non-runes mode, `let`s stay LIVE because they're
    // exported as props by default and may be set by the parent.
    let runes_mode = !skip.is_empty();
    let mut candidates: HashMap<String, Expression> = HashMap::new();
    for s in &p.body {
        if let Statement::Variable(v) = s {
            // Only fold `let X = LITERAL` in runes mode. `const` is left
            // alone — upstream doesn't constant-propagate `const` bindings
            // into templates (the binding stays as a Variable reference).
            if !runes_mode {
                continue;
            }
            if !matches!(v.kind, VariableKind::Let) {
                continue;
            }
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
        // $state(x) / $state.raw(x) → x  (no-arg → `void 0`)
        "$state" | "$state.raw" | "$state.eager" => {
            Some(first_arg_or_void(&c.arguments))
        }
        // $derived(EXPR) → $.derived(() => EXPR)
        // $derived.by(fn) → $.derived(fn) (the .by form takes a function directly)
        "$derived" => Some(wrap_derived_arrow(&c.arguments)),
        "$derived.by" => Some(wrap_derived_call(&c.arguments)),
        // $bindable(default) → default  (no-arg → `void 0`)
        "$bindable" => Some(first_arg_or_void(&c.arguments)),
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

/// Like `first_arg_or_undefined` but uses `void 0` for the no-arg case.
/// Mirrors upstream's `b.void0` for `$state()` / `$bindable()`.
fn first_arg_or_void(args: &[Argument]) -> Expression {
    args.iter().find_map(|a| match a {
        Argument::Expression(e) => Some(e.clone()),
        _ => None,
    }).unwrap_or_else(void_zero)
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

/// Walk an Expression and rewrite every Identifier `$X` where `X` is in
/// `top_bindings` into `$.store_get($$store_subs ??= {}, '$X', X)`.
/// Records the full `$X` names that were rewritten in `refs`. Mirrors
/// upstream's `serialize_get_binding` / store_get rewriting for SSR.
pub fn rewrite_store_refs(
    e: &mut Expression,
    top_bindings: &HashSet<String>,
    refs: &mut HashSet<String>,
) {
    if top_bindings.is_empty() {
        return;
    }
    rewrite_store_refs_inner(e, top_bindings, refs);
}

pub fn rewrite_store_refs_in_stmts(
    stmts: &mut [Statement],
    top_bindings: &HashSet<String>,
    refs: &mut HashSet<String>,
) {
    if top_bindings.is_empty() {
        return;
    }
    for s in stmts {
        rewrite_store_refs_stmt(s, top_bindings, refs);
    }
}

fn rewrite_store_refs_stmt(
    s: &mut Statement,
    top_bindings: &HashSet<String>,
    refs: &mut HashSet<String>,
) {
    match s {
        Statement::Variable(v) => {
            for d in &mut v.declarations {
                rewrite_store_refs_in_pattern(&mut d.id, top_bindings, refs);
                if let Some(init) = &mut d.init {
                    rewrite_store_refs_inner(init, top_bindings, refs);
                }
            }
        }
        Statement::Expression(es) => {
            rewrite_store_refs_inner(&mut es.expression, top_bindings, refs);
        }
        Statement::Return(r) => {
            if let Some(a) = &mut r.argument {
                rewrite_store_refs_inner(a, top_bindings, refs);
            }
        }
        Statement::If(i) => {
            rewrite_store_refs_inner(&mut i.test, top_bindings, refs);
            rewrite_store_refs_stmt(&mut i.consequent, top_bindings, refs);
            if let Some(a) = &mut i.alternate {
                rewrite_store_refs_stmt(a, top_bindings, refs);
            }
        }
        Statement::Block(b) => {
            for s in &mut b.body {
                rewrite_store_refs_stmt(s, top_bindings, refs);
            }
        }
        Statement::For(f) => {
            if let Some(init) = &mut f.init {
                match init {
                    ForInit::Declaration(v) => {
                        for d in &mut v.declarations {
                            if let Some(i) = &mut d.init {
                                rewrite_store_refs_inner(i, top_bindings, refs);
                            }
                        }
                    }
                    ForInit::Expression(e) => rewrite_store_refs_inner(e, top_bindings, refs),
                }
            }
            if let Some(t) = &mut f.test {
                rewrite_store_refs_inner(t, top_bindings, refs);
            }
            if let Some(u) = &mut f.update {
                rewrite_store_refs_inner(u, top_bindings, refs);
            }
            rewrite_store_refs_stmt(&mut f.body, top_bindings, refs);
        }
        _ => {}
    }
}

fn rewrite_store_refs_in_pattern(
    p: &mut Pattern,
    top_bindings: &HashSet<String>,
    refs: &mut HashSet<String>,
) {
    match p {
        Pattern::Object(o) => {
            for prop in &mut o.properties {
                match prop {
                    ObjectPatternMember::Property(pr) => {
                        rewrite_store_refs_in_pattern(&mut pr.value, top_bindings, refs);
                    }
                    ObjectPatternMember::Rest(r) => {
                        rewrite_store_refs_in_pattern(&mut r.argument, top_bindings, refs);
                    }
                }
            }
        }
        Pattern::Array(a) => {
            for el in &mut a.elements {
                if let Some(p) = el {
                    rewrite_store_refs_in_pattern(p, top_bindings, refs);
                }
            }
        }
        Pattern::Assignment(a) => {
            rewrite_store_refs_in_pattern(&mut a.left, top_bindings, refs);
            rewrite_store_refs_inner(&mut a.right, top_bindings, refs);
        }
        Pattern::Rest(r) => {
            rewrite_store_refs_in_pattern(&mut r.argument, top_bindings, refs);
        }
        _ => {}
    }
}

fn rewrite_store_refs_inner(
    e: &mut Expression,
    top_bindings: &HashSet<String>,
    refs: &mut HashSet<String>,
) {
    // Self check: if this expression IS a $X identifier where X is a top
    // binding, rewrite it.
    if let Expression::Identifier(id) = e {
        if id.name.len() >= 2 && id.name.starts_with('$') && !id.name.starts_with("$$") {
            let base = id.name[1..].to_string();
            if top_bindings.contains(&base) {
                let store_name = id.name.clone();
                refs.insert(store_name.clone());
                let coalesce = Expression::Assignment(Box::new(AssignmentExpression {
                    left: AssignmentTarget::Expression(t::id("$$store_subs")),
                    operator: AssignmentOperator::CoalesceAssign,
                    right: Expression::Object(Box::new(ObjectExpression {
                        properties: vec![],
                        span: Span::ZERO,
                    })),
                    span: Span::ZERO,
                }));
                *e = t::call(
                    t::member_id(t::id("$"), "store_get"),
                    vec![coalesce, t::literal_str(&store_name), t::id(&base)],
                );
                return;
            }
        }
    }
    match e {
        Expression::Member(m) => {
            rewrite_store_refs_inner(&mut m.object, top_bindings, refs);
            if let MemberProperty::Expression(e) = &mut m.property {
                rewrite_store_refs_inner(e, top_bindings, refs);
            }
        }
        Expression::Call(c) => {
            rewrite_store_refs_inner(&mut c.callee, top_bindings, refs);
            for a in &mut c.arguments {
                match a {
                    Argument::Expression(e) => {
                        rewrite_store_refs_inner(e, top_bindings, refs);
                    }
                    Argument::Spread(s) => {
                        rewrite_store_refs_inner(&mut s.argument, top_bindings, refs);
                    }
                }
            }
        }
        Expression::Binary(b) => {
            rewrite_store_refs_inner(&mut b.left, top_bindings, refs);
            rewrite_store_refs_inner(&mut b.right, top_bindings, refs);
        }
        Expression::Logical(l) => {
            rewrite_store_refs_inner(&mut l.left, top_bindings, refs);
            rewrite_store_refs_inner(&mut l.right, top_bindings, refs);
        }
        Expression::Conditional(c) => {
            rewrite_store_refs_inner(&mut c.test, top_bindings, refs);
            rewrite_store_refs_inner(&mut c.consequent, top_bindings, refs);
            rewrite_store_refs_inner(&mut c.alternate, top_bindings, refs);
        }
        Expression::Unary(u) => rewrite_store_refs_inner(&mut u.argument, top_bindings, refs),
        Expression::Sequence(s) => {
            for e in &mut s.expressions {
                rewrite_store_refs_inner(e, top_bindings, refs);
            }
        }
        Expression::Template(t) => {
            for ex in &mut t.expressions {
                rewrite_store_refs_inner(ex, top_bindings, refs);
            }
        }
        Expression::Paren(p) => rewrite_store_refs_inner(&mut p.expression, top_bindings, refs),
        Expression::Assignment(a) => {
            rewrite_store_refs_inner(&mut a.right, top_bindings, refs);
        }
        Expression::Array(a) => {
            for el in &mut a.elements {
                match el {
                    ArrayElement::Expression(e) => rewrite_store_refs_inner(e, top_bindings, refs),
                    ArrayElement::Spread(s) => rewrite_store_refs_inner(&mut s.argument, top_bindings, refs),
                    ArrayElement::Elision => {}
                }
            }
        }
        Expression::Object(o) => {
            for p in &mut o.properties {
                match p {
                    ObjectMember::Property(pr) => {
                        if pr.computed {
                            if let PropertyKey::Expression(k) = &mut pr.key {
                                rewrite_store_refs_inner(k, top_bindings, refs);
                            }
                        }
                        rewrite_store_refs_inner(&mut pr.value, top_bindings, refs);
                    }
                    ObjectMember::Spread(s) => {
                        rewrite_store_refs_inner(&mut s.argument, top_bindings, refs);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Collect names of top-level `let/const/var` bindings (only direct Identifier
/// patterns; destructure patterns are skipped — store-detection only needs
/// the names users would reference via the `$X` form).
pub fn collect_top_level_bindings(p: &Program) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in &p.body {
        match s {
            Statement::Variable(v) => {
                for d in &v.declarations {
                    if let Pattern::Identifier(id) = &d.id {
                        out.insert(id.name.clone());
                    }
                }
            }
            Statement::Import(im) => {
                for sp in &im.specifiers {
                    match sp {
                        ImportSpecifierKind::Default(d) => {
                            out.insert(d.local.name.clone());
                        }
                        ImportSpecifierKind::Named(n) => {
                            out.insert(n.local.name.clone());
                        }
                        ImportSpecifierKind::Namespace(n) => {
                            out.insert(n.local.name.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}
