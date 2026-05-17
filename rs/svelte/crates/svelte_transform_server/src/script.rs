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

/// Rewrite a Program in-place to erase server-irrelevant rune calls.
/// Returns `(uses_props, rune_bindings)`:
/// - `uses_props`: program references `$props` / `$$props`
/// - `rune_bindings`: names of `let X = $state(...)`-style bindings whose
///   initializer was a rune call. These should NOT be substituted as
///   compile-time constants even though they look like plain literals
///   after erasure (they're reactive bindings semantically).
pub fn rewrite_program_for_server(p: &mut Program) -> (bool, HashSet<String>) {
    // Pre-pass: collect identifiers bound to rune initializers.
    let mut rune_bindings: HashSet<String> = HashSet::new();
    for s in &p.body {
        collect_rune_bindings_stmt(s, &mut rune_bindings);
    }
    // Rune erasure.
    let mut ctx = Ctx { uses_props: false };
    for s in &mut p.body {
        rewrite_statement(s, &mut ctx);
    }
    (ctx.uses_props, rune_bindings)
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
            ExportDefault::Expression(e) => rewrite_expression(e, ctx),
            _ => {}
        },
        _ => {}
    }
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
