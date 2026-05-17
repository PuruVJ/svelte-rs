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

use svelte_js_ast::*;

/// Rewrite a Program in-place to erase server-irrelevant rune calls.
/// Returns `true` if the program references `$props` / `$$props` (caller
/// needs to thread `$$props` as a 2nd function parameter).
pub fn rewrite_program_for_server(p: &mut Program) -> bool {
    let mut ctx = Ctx { uses_props: false };
    for s in &mut p.body {
        rewrite_statement(s, &mut ctx);
    }
    ctx.uses_props
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
