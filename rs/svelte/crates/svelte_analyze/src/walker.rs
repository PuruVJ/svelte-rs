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

use svelte_ast::Root;

use crate::scope::{BindingKind, DeclarationKind, Scope, ScopePtr};

/// Walk a Program node (the `content` field of a `Script`) and populate
/// `root_scope` and its descendants.
pub fn build_program_scope(program: &svelte_js_ast::Program, root_scope: &ScopePtr) -> bool {
    for stmt in &program.body {
        hoist_typed(stmt, root_scope);
    }
    let uses_runes = program.body.iter().any(stmt_uses_runes);
    for stmt in &program.body {
        visit_stmt(stmt, root_scope);
    }
    uses_runes
}

fn hoist_typed(s: &svelte_js_ast::Statement, scope: &ScopePtr) {
    use svelte_js_ast::Statement as S;
    match s {
        S::Variable(v) if matches!(v.kind, svelte_js_ast::VariableKind::Var) => {
            declare_var_declaration(v, scope);
        }
        S::Function(f) => {
            if let Some(id) = &f.id {
                scope.borrow_mut().declare(
                    id.name.to_string(),
                    BindingKind::Normal,
                    DeclarationKind::Function,
                    id.clone(),
                );
            }
        }
        S::ExportNamed(e) => {
            if let Some(d) = &e.declaration {
                hoist_typed(d, scope);
            }
        }
        S::ExportDefault(e) => {
            if let svelte_js_ast::ExportDefault::Function(f) = &e.declaration {
                if let Some(id) = &f.id {
                    scope.borrow_mut().declare(
                    id.name.to_string(),
                        BindingKind::Normal,
                        DeclarationKind::Function,
                        id.clone(),
                    );
                }
            }
        }
        _ => {}
    }
}

fn visit_stmt(s: &svelte_js_ast::Statement, scope: &ScopePtr) {
    use svelte_js_ast::Statement as S;
    match s {
        S::Variable(v) => {
            if !matches!(v.kind, svelte_js_ast::VariableKind::Var) {
                declare_var_declaration(v, scope);
            }
        }
        S::Function(f) => visit_function_body(f, scope),
        S::Class(c) => visit_class(c, scope),
        S::Import(i) => {
            for spec in &i.specifiers {
                let local = match spec {
                    svelte_js_ast::ImportSpecifierKind::Named(s) => &s.local,
                    svelte_js_ast::ImportSpecifierKind::Default(s) => &s.local,
                    svelte_js_ast::ImportSpecifierKind::Namespace(s) => &s.local,
                };
                scope.borrow_mut().declare(
                        local.name.to_string(),
                    BindingKind::Normal,
                    DeclarationKind::Import,
                    local.clone(),
                );
            }
        }
        S::ExportNamed(e) => {
            if let Some(d) = &e.declaration {
                visit_stmt(d, scope);
            }
        }
        S::ExportDefault(e) => match &e.declaration {
            svelte_js_ast::ExportDefault::Function(f) => visit_function_body(f, scope),
            svelte_js_ast::ExportDefault::Class(c) => visit_class(c, scope),
            svelte_js_ast::ExportDefault::Expression(_) => {}
        },
        S::Block(b) => {
            let child = Scope::child(scope, true);
            for s in &b.body {
                hoist_typed(s, &child);
            }
            for s in &b.body {
                visit_stmt(s, &child);
            }
        }
        S::If(i) => {
            visit_stmt(&i.consequent, scope);
            if let Some(a) = &i.alternate {
                visit_stmt(a, scope);
            }
        }
        S::For(f) => {
            let child = Scope::child(scope, true);
            if let Some(svelte_js_ast::ForInit::Declaration(d)) = &f.init {
                if matches!(d.kind, svelte_js_ast::VariableKind::Var) {
                    declare_var_declaration(d, scope);
                } else {
                    declare_var_declaration(d, &child);
                }
            }
            visit_stmt(&f.body, &child);
        }
        S::ForIn(f) => {
            let child = Scope::child(scope, true);
            if let svelte_js_ast::ForInit::Declaration(d) = &f.left {
                if matches!(d.kind, svelte_js_ast::VariableKind::Var) {
                    declare_var_declaration(d, scope);
                } else {
                    declare_var_declaration(d, &child);
                }
            }
            visit_stmt(&f.body, &child);
        }
        S::ForOf(f) => {
            let child = Scope::child(scope, true);
            if let svelte_js_ast::ForInit::Declaration(d) = &f.left {
                if matches!(d.kind, svelte_js_ast::VariableKind::Var) {
                    declare_var_declaration(d, scope);
                } else {
                    declare_var_declaration(d, &child);
                }
            }
            visit_stmt(&f.body, &child);
        }
        S::While(w) => visit_stmt(&w.body, scope),
        S::DoWhile(w) => visit_stmt(&w.body, scope),
        S::Try(t) => {
            for s in &t.block.body {
                visit_stmt(s, scope);
            }
            if let Some(h) = &t.handler {
                let catch_scope = Scope::child(scope, true);
                if let Some(p) = &h.param {
                    declare_pattern(p, &catch_scope, BindingKind::Normal, DeclarationKind::Let);
                }
                for s in &h.body.body {
                    visit_stmt(s, &catch_scope);
                }
            }
            if let Some(f) = &t.finalizer {
                for s in &f.body {
                    visit_stmt(s, scope);
                }
            }
        }
        S::Switch(sw) => {
            let switch_scope = Scope::child(scope, true);
            for c in &sw.cases {
                for s in &c.consequent {
                    visit_stmt(s, &switch_scope);
                }
            }
        }
        S::Labeled(l) => visit_stmt(&l.body, scope),
        S::With(w) => visit_stmt(&w.body, scope),
        _ => {}
    }
}

fn declare_var_declaration(v: &svelte_js_ast::VariableDeclaration, scope: &ScopePtr) {
    let decl_kind = match v.kind {
        svelte_js_ast::VariableKind::Var => DeclarationKind::Var,
        svelte_js_ast::VariableKind::Let => DeclarationKind::Let,
        svelte_js_ast::VariableKind::Const => DeclarationKind::Const,
    };
    for d in &v.declarations {
        // Classify by the initializer's rune call, if any.
        let kind = d
            .init
            .as_ref()
            .and_then(get_rune_keypath_typed)
            .as_deref()
            .map(rune_to_binding_kind)
            .filter(|k| !matches!(k, BindingKind::Normal))
            .unwrap_or(BindingKind::Normal);
        declare_pattern(&d.id, scope, kind, decl_kind);
    }
}

fn declare_pattern(
    p: &svelte_js_ast::Pattern,
    scope: &ScopePtr,
    kind: BindingKind,
    decl_kind: DeclarationKind,
) {
    use svelte_js_ast::Pattern as P;
    match p {
        P::Identifier(id) => {
            scope
                .borrow_mut()
                .declare(id.name.to_string(), kind, decl_kind, id.clone());
        }
        P::Array(a) => {
            for el in a.elements.iter().flatten() {
                declare_pattern(el, scope, kind, decl_kind);
            }
        }
        P::Object(o) => {
            for m in &o.properties {
                match m {
                    svelte_js_ast::ObjectPatternMember::Property(p) => {
                        declare_pattern(&p.value, scope, kind, decl_kind);
                    }
                    svelte_js_ast::ObjectPatternMember::Rest(r) => {
                        // `let { ...rest } = $props()` — rest of props.
                        let rest_kind = if matches!(kind, BindingKind::Prop) {
                            BindingKind::RestProp
                        } else {
                            kind
                        };
                        declare_pattern(&r.argument, scope, rest_kind, decl_kind);
                    }
                }
            }
        }
        P::Rest(r) => {
            declare_pattern(&r.argument, scope, kind, DeclarationKind::RestParam);
        }
        P::Assignment(a) => {
            declare_pattern(&a.left, scope, kind, decl_kind);
        }
        P::Member(_) => {}
    }
}

fn visit_function_body(f: &svelte_js_ast::FunctionDeclaration, parent: &ScopePtr) {
    let fn_scope = Scope::child(parent, false);
    for param in &f.params {
        declare_pattern(param, &fn_scope, BindingKind::Normal, DeclarationKind::Param);
    }
    for s in &f.body.body {
        hoist_typed(s, &fn_scope);
    }
    for s in &f.body.body {
        visit_stmt(s, &fn_scope);
    }
}

fn visit_class(c: &svelte_js_ast::ClassDeclaration, parent: &ScopePtr) {
    if let Some(id) = &c.id {
        parent.borrow_mut().declare(
            id.name.to_string(),
            BindingKind::Normal,
            DeclarationKind::Let,
            id.clone(),
        );
    }
    // Method bodies use their own scope but we don't descend for now —
    // transforms get them via a fresh scope when needed.
}

/// `$state(...)` / `$props()` / etc. → the rune keypath when initializer is
/// a CallExpression with a rune-shaped callee.
fn get_rune_keypath_typed(init: &svelte_js_ast::Expression) -> Option<String> {
    let svelte_js_ast::Expression::Call(c) = init else {
        return None;
    };
    let path = global_keypath_typed(&c.callee)?;
    if is_rune(&path) {
        Some(path)
    } else {
        None
    }
}

#[allow(dead_code)]
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

/// Returns true if `<svelte:options runes />` (or equivalent) opts the
/// component into runes mode without walking script bodies.
pub fn runes_enabled_by_options(root: &Root) -> bool {
    // <svelte:options runes /> or <svelte:options runes={true} /> → explicit opt-in.
    if let Some(options) = &root.options {
        if options.runes == Some(true) {
            return true;
        }
    }
    // Also walk the fragment for a SvelteOptions node — the parser doesn't
    // always populate `root.options` and instead leaves the element in the
    // fragment children. Check for `runes` attribute presence.
    for n in &root.fragment.nodes {
        if let svelte_ast::fragment::FragmentChild::SvelteOptions(opts) = n {
            for a in &opts.attributes {
                if let svelte_ast::attributes::ElementAttribute::Attribute(attr) = a {
                    if attr.name == "runes" {
                        // Empty / boolean / `runes={true}` all opt in.
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn stmt_uses_runes(s: &svelte_js_ast::Statement) -> bool {
    use svelte_js_ast::Statement as S;
    match s {
        S::Block(b) => b.body.iter().any(stmt_uses_runes),
        S::Expression(e) => expr_uses_runes(&e.expression),
        S::Return(r) => r.argument.as_ref().is_some_and(expr_uses_runes),
        S::If(i) => {
            expr_uses_runes(&i.test)
                || stmt_uses_runes(&i.consequent)
                || i.alternate.as_ref().is_some_and(stmt_uses_runes)
        }
        S::DoWhile(w) => stmt_uses_runes(&w.body) || expr_uses_runes(&w.test),
        S::While(w) => expr_uses_runes(&w.test) || stmt_uses_runes(&w.body),
        S::For(f) => {
            f.init.as_ref().is_some_and(for_init_uses_runes)
                || f.test.as_ref().is_some_and(expr_uses_runes)
                || f.update.as_ref().is_some_and(expr_uses_runes)
                || stmt_uses_runes(&f.body)
        }
        S::ForIn(f) => {
            for_init_uses_runes(&f.left) || expr_uses_runes(&f.right) || stmt_uses_runes(&f.body)
        }
        S::ForOf(f) => {
            for_init_uses_runes(&f.left) || expr_uses_runes(&f.right) || stmt_uses_runes(&f.body)
        }
        S::Throw(t) => expr_uses_runes(&t.argument),
        S::Try(t) => {
            t.block.body.iter().any(stmt_uses_runes)
                || t.handler.as_ref().is_some_and(|h| h.body.body.iter().any(stmt_uses_runes))
                || t.finalizer.as_ref().is_some_and(|f| f.body.iter().any(stmt_uses_runes))
        }
        S::Switch(sw) => {
            expr_uses_runes(&sw.discriminant)
                || sw.cases.iter().any(|c| {
                    c.test.as_ref().is_some_and(expr_uses_runes)
                        || c.consequent.iter().any(stmt_uses_runes)
                })
        }
        S::With(w) => expr_uses_runes(&w.object) || stmt_uses_runes(&w.body),
        S::Labeled(l) => stmt_uses_runes(&l.body),
        S::Variable(v) => v
            .declarations
            .iter()
            .any(|d| d.init.as_ref().is_some_and(expr_uses_runes)),
        S::Function(f) => f.body.body.iter().any(stmt_uses_runes),
        S::Class(c) => c.body.body.iter().any(class_member_uses_runes),
        S::ExportNamed(e) => e.declaration.as_ref().is_some_and(|d| stmt_uses_runes(d)),
        S::ExportDefault(e) => match &e.declaration {
            svelte_js_ast::ExportDefault::Function(f) => {
                f.body.body.iter().any(stmt_uses_runes)
            }
            svelte_js_ast::ExportDefault::Class(c) => {
                c.body.body.iter().any(class_member_uses_runes)
            }
            svelte_js_ast::ExportDefault::Expression(e) => expr_uses_runes(e),
        },
        _ => false,
    }
}

fn for_init_uses_runes(init: &svelte_js_ast::ForInit) -> bool {
    match init {
        svelte_js_ast::ForInit::Declaration(d) => d
            .declarations
            .iter()
            .any(|d| d.init.as_ref().is_some_and(expr_uses_runes)),
        svelte_js_ast::ForInit::Expression(e) => expr_uses_runes(e),
    }
}

fn class_member_uses_runes(m: &svelte_js_ast::ClassMember) -> bool {
    match m {
        svelte_js_ast::ClassMember::Method(md) => md.value.body.body.iter().any(stmt_uses_runes),
        svelte_js_ast::ClassMember::Property(p) => {
            p.value.as_ref().is_some_and(expr_uses_runes)
        }
        svelte_js_ast::ClassMember::StaticBlock(s) => s.body.iter().any(stmt_uses_runes),
    }
}

fn expr_uses_runes(e: &svelte_js_ast::Expression) -> bool {
    use svelte_js_ast::Expression as E;
    match e {
        E::Call(c) => {
            if let Some(keypath) = global_keypath_typed(&c.callee) {
                if is_rune(&keypath) {
                    return true;
                }
            }
            expr_uses_runes(&c.callee) || c.arguments.iter().any(argument_uses_runes)
        }
        E::New(n) => expr_uses_runes(&n.callee) || n.arguments.iter().any(argument_uses_runes),
        E::Member(m) => {
            expr_uses_runes(&m.object)
                || match &m.property {
                    svelte_js_ast::MemberProperty::Expression(e) => expr_uses_runes(e),
                    _ => false,
                }
        }
        E::Binary(b) => expr_uses_runes(&b.left) || expr_uses_runes(&b.right),
        E::Logical(l) => expr_uses_runes(&l.left) || expr_uses_runes(&l.right),
        E::Assignment(a) => expr_uses_runes(&a.right),
        E::Update(u) => expr_uses_runes(&u.argument),
        E::Unary(u) => expr_uses_runes(&u.argument),
        E::Conditional(c) => {
            expr_uses_runes(&c.test) || expr_uses_runes(&c.consequent) || expr_uses_runes(&c.alternate)
        }
        E::Sequence(s) => s.expressions.iter().any(expr_uses_runes),
        E::Spread(s) => expr_uses_runes(&s.argument),
        E::Yield(y) => y.argument.as_ref().is_some_and(|a| expr_uses_runes(a)),
        E::Await(a) => expr_uses_runes(&a.argument),
        E::Tagged(t) => {
            expr_uses_runes(&t.tag) || t.quasi.expressions.iter().any(expr_uses_runes)
        }
        E::Template(t) => t.expressions.iter().any(expr_uses_runes),
        E::Paren(p) => expr_uses_runes(&p.expression),
        E::Array(a) => a.elements.iter().any(|el| match el {
            svelte_js_ast::ArrayElement::Expression(e) => expr_uses_runes(e),
            svelte_js_ast::ArrayElement::Spread(s) => expr_uses_runes(&s.argument),
            svelte_js_ast::ArrayElement::Elision => false,
        }),
        E::Object(o) => o.properties.iter().any(|p| match p {
            svelte_js_ast::ObjectMember::Property(prop) => expr_uses_runes(&prop.value),
            svelte_js_ast::ObjectMember::Spread(s) => expr_uses_runes(&s.argument),
        }),
        E::Arrow(a) => match &a.body {
            svelte_js_ast::ArrowBody::Block(b) => b.body.iter().any(stmt_uses_runes),
            svelte_js_ast::ArrowBody::Expression(e) => expr_uses_runes(e),
        },
        E::Function(f) => f.body.body.iter().any(stmt_uses_runes),
        E::Class(c) => c.body.body.iter().any(class_member_uses_runes),
        E::Identifier(id) => {
            // Bare rune identifier (e.g. `let { a } = $props;`) — upstream
            // treats this as a runes-mode signal too, allowing the validator
            // to emit `rune_missing_parentheses`.
            matches!(
                id.name.as_ref(),
                "$state" | "$derived" | "$props" | "$effect" | "$host"
                    | "$bindable" | "$inspect"
            )
        }
        _ => false,
    }
}

fn argument_uses_runes(a: &svelte_js_ast::Argument) -> bool {
    match a {
        svelte_js_ast::Argument::Expression(e) => expr_uses_runes(e),
        svelte_js_ast::Argument::Spread(s) => expr_uses_runes(&s.argument),
    }
}

/// Walk a possibly-chained `MemberExpression` whose root is an identifier,
/// joining the parts as `a.b.c`. Returns `None` for computed access or
/// non-identifier roots.
fn global_keypath_typed(e: &svelte_js_ast::Expression) -> Option<String> {
    let mut joined = String::new();
    let mut cur = e;
    while let svelte_js_ast::Expression::Member(m) = cur {
        if m.computed {
            return None;
        }
        let name = match &m.property {
            svelte_js_ast::MemberProperty::Identifier(i) => &i.name,
            _ => return None,
        };
        joined = format!(".{name}{joined}");
        cur = &m.object;
    }
    if let svelte_js_ast::Expression::Identifier(i) = cur {
        Some(format!("{}{joined}", i.name))
    } else {
        None
    }
}
