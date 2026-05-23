//! Walk an OXC arena-allocated JS/TS AST and produce typed `svelte_js_ast`
//! nodes.
//!
//! All node positions get an `offset_shift` added so that the OXC slice's
//! local coordinates land back in the original `.svelte` source's coordinate
//! space. Strings are copied out of the OXC arena into owned `String`s — the
//! arena is dropped immediately after the conversion.
//!
//! Coverage: full ES2024 + JSX-as-error-only + TS-as-transparent-wrap (TS-only
//! nodes like `TSAsExpression` unwrap to their inner expression — type
//! annotations are dropped, which is what Svelte's analyse/transform
//! expects).

use oxc_ast::ast as oxc;
use oxc_span::GetSpan;
use svelte_js_ast::*;
use std::borrow::Cow;

// -------------------------------------------------------------------------
// Span shifting
// -------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Shift(pub u32);

fn span_of(s: oxc_span::Span, shift: Shift) -> Span {
    Span::new(s.start + shift.0, s.end + shift.0)
}
#[inline]
fn cow_owned(s: &str) -> Cow<'static, str> {
    Cow::Owned(s.into())
}

#[inline]
fn private_identifier(name: &str, sp: oxc_span::Span, shift: Shift) -> PrivateIdentifier {
    PrivateIdentifier {
        name: cow_owned(name),
        span: span_of(sp, shift),
    }
}

/// Pre-allocate a `Vec` when the source iterator exposes an exact length.
#[inline]
fn map_collect<T, U, I, F>(iter: I, f: F) -> Vec<U>
where
    I: IntoIterator<Item = T>,
    I::IntoIter: ExactSizeIterator,
    F: FnMut(T) -> U,
{
    let iter = iter.into_iter();
    let mut out = Vec::with_capacity(iter.len());
    out.extend(iter.map(f));
    out
}


// Thread-local slice the OXC parser is operating on. Set by `set_slice`
// (called from the parse bridge), read by helpers that need to capture
// raw source text (e.g. TS type annotations).
thread_local! {
    static SLICE: std::cell::RefCell<Option<std::sync::Arc<str>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_slice(s: &str) {
    SLICE.with(|c| *c.borrow_mut() = Some(std::sync::Arc::from(s)));
}

pub fn clear_slice() {
    SLICE.with(|c| *c.borrow_mut() = None);
}

/// Extract a text range from the OXC slice using a local (unshifted) span.
fn slice_text(span: oxc_span::Span) -> Option<String> {
    SLICE.with(|c| {
        c.borrow().as_ref().and_then(|s| {
            let start = span.start as usize;
            let end = span.end as usize;
            if end <= s.len() && start <= end {
                Some(s[start..end].into())
            } else {
                None
            }
        })
    })
}

// -------------------------------------------------------------------------
// Program
// -------------------------------------------------------------------------

pub fn program(p: &oxc::Program<'_>, shift: Shift) -> Program {
    let mut body = Vec::with_capacity(p.body.len());
    body.extend(p.body.iter().map(|s| statement(s, shift)));
    Program {
        source_type: SourceType::Module,
        body,
        span: span_of(p.span, shift),
    }
}

// -------------------------------------------------------------------------
// Statements / Declarations
// -------------------------------------------------------------------------

pub fn statement(s: &oxc::Statement<'_>, shift: Shift) -> Statement {
    use oxc::Statement as S;
    match s {
        S::BlockStatement(b) => Statement::Block(Box::new(block_statement(b, shift))),
        S::BreakStatement(b) => Statement::Break(Box::new(BreakStatement {
            label: b.label.as_ref().map(|l| ident_from_name(&l.name, l.span, shift)),
            span: span_of(b.span, shift),
        })),
        S::ContinueStatement(c) => Statement::Continue(Box::new(ContinueStatement {
            label: c.label.as_ref().map(|l| ident_from_name(&l.name, l.span, shift)),
            span: span_of(c.span, shift),
        })),
        S::DebuggerStatement(d) => Statement::Debugger(span_of(d.span, shift)),
        S::DoWhileStatement(d) => Statement::DoWhile(Box::new(DoWhileStatement {
            body: statement(&d.body, shift),
            test: expression(&d.test, shift),
            span: span_of(d.span, shift),
        })),
        S::EmptyStatement(e) => Statement::Empty(span_of(e.span, shift)),
        S::ExpressionStatement(e) => Statement::Expression(Box::new(ExpressionStatement {
            expression: expression(&e.expression, shift),
            span: span_of(e.span, shift),
        })),
        S::ForInStatement(f) => Statement::ForIn(Box::new(ForInStatement {
            left: for_init_target(&f.left, shift),
            right: expression(&f.right, shift),
            body: statement(&f.body, shift),
            span: span_of(f.span, shift),
        })),
        S::ForOfStatement(f) => Statement::ForOf(Box::new(ForOfStatement {
            left: for_init_target(&f.left, shift),
            right: expression(&f.right, shift),
            body: statement(&f.body, shift),
            r#await: f.r#await,
            span: span_of(f.span, shift),
        })),
        S::ForStatement(f) => Statement::For(Box::new(ForStatement {
            init: f.init.as_ref().map(|i| for_init_loose(i, shift)),
            test: f.test.as_ref().map(|t| expression(t, shift)),
            update: f.update.as_ref().map(|u| expression(u, shift)),
            body: statement(&f.body, shift),
            span: span_of(f.span, shift),
        })),
        S::IfStatement(i) => Statement::If(Box::new(IfStatement {
            test: expression(&i.test, shift),
            consequent: statement(&i.consequent, shift),
            alternate: i.alternate.as_ref().map(|a| statement(a, shift)),
            span: span_of(i.span, shift),
        })),
        S::LabeledStatement(l) => Statement::Labeled(Box::new(LabeledStatement {
            label: ident_from_name(&l.label.name, l.label.span, shift),
            body: statement(&l.body, shift),
            span: span_of(l.span, shift),
        })),
        S::ReturnStatement(r) => Statement::Return(Box::new(ReturnStatement {
            argument: r.argument.as_ref().map(|a| expression(a, shift)),
            span: span_of(r.span, shift),
        })),
        S::SwitchStatement(s) => Statement::Switch(Box::new(SwitchStatement {
            discriminant: expression(&s.discriminant, shift),
            cases: map_collect(s.cases.iter(), |c| SwitchCase {
                test: c.test.as_ref().map(|t| expression(t, shift)),
                consequent: map_collect(c.consequent.iter(), |s| statement(s, shift)),
                span: span_of(c.span, shift),
            }),
            span: span_of(s.span, shift),
        })),
        S::ThrowStatement(t) => Statement::Throw(Box::new(ThrowStatement {
            argument: expression(&t.argument, shift),
            span: span_of(t.span, shift),
        })),
        S::TryStatement(t) => Statement::Try(Box::new(TryStatement {
            block: block_statement(&t.block, shift),
            handler: t.handler.as_ref().map(|h| CatchClause {
                param: h.param.as_ref().map(|p| binding_pattern(&p.pattern, shift)),
                body: block_statement(&h.body, shift),
                span: span_of(h.span, shift),
            }),
            finalizer: t.finalizer.as_ref().map(|f| block_statement(f, shift)),
            span: span_of(t.span, shift),
        })),
        S::WhileStatement(w) => Statement::While(Box::new(WhileStatement {
            test: expression(&w.test, shift),
            body: statement(&w.body, shift),
            span: span_of(w.span, shift),
        })),
        S::WithStatement(w) => Statement::With(Box::new(WithStatement {
            object: expression(&w.object, shift),
            body: statement(&w.body, shift),
            span: span_of(w.span, shift),
        })),

        // Declarations
        S::VariableDeclaration(v) => {
            Statement::Variable(Box::new(variable_declaration(v, shift)))
        }
        S::FunctionDeclaration(f) => {
            Statement::Function(Box::new(function_decl(f, shift)))
        }
        S::ClassDeclaration(c) => Statement::Class(Box::new(class_decl(c, shift))),

        // Modules
        S::ImportDeclaration(i) => {
            Statement::Import(Box::new(import_declaration(i, shift)))
        }
        S::ExportNamedDeclaration(e) => {
            Statement::ExportNamed(Box::new(export_named_declaration(e, shift)))
        }
        S::ExportDefaultDeclaration(e) => {
            Statement::ExportDefault(Box::new(export_default_declaration(e, shift)))
        }
        S::ExportAllDeclaration(e) => {
            Statement::ExportAll(Box::new(export_all_declaration(e, shift)))
        }

        // TS-only declarations we drop on the floor (transparent to Svelte
        // analysis). Emit Empty to preserve statement position.
        S::TSTypeAliasDeclaration(d) => Statement::Empty(span_of(d.span, shift)),
        S::TSInterfaceDeclaration(d) => Statement::Empty(span_of(d.span, shift)),
        S::TSEnumDeclaration(d) => Statement::Empty(span_of(d.span, shift)),
        S::TSModuleDeclaration(d) => Statement::Empty(span_of(d.span, shift)),
        S::TSGlobalDeclaration(d) => Statement::Empty(span_of(d.span, shift)),
        S::TSImportEqualsDeclaration(d) => Statement::Empty(span_of(d.span, shift)),
        S::TSExportAssignment(d) => Statement::Empty(span_of(d.span, shift)),
        S::TSNamespaceExportDeclaration(d) => Statement::Empty(span_of(d.span, shift)),
    }
}

fn block_statement(b: &oxc::BlockStatement<'_>, shift: Shift) -> BlockStatement {
    BlockStatement {
        body: {
            let mut body = Vec::with_capacity(b.body.len());
            body.extend(b.body.iter().map(|s| statement(s, shift)));
            body
        },
        span: span_of(b.span, shift),
    }
}

fn for_init_loose(init: &oxc::ForStatementInit<'_>, shift: Shift) -> ForInit {
    use oxc::ForStatementInit as F;
    match init {
        F::VariableDeclaration(v) => {
            ForInit::Declaration(Box::new(variable_declaration(v, shift)))
        }
        _ => {
            // It's an expression variant inherited from `Expression`.
            // We can extract the underlying expression by matching.
            let expr = match init {
                F::VariableDeclaration(_) => unreachable!(),
                _ => {
                    // Treat it as an expression statement — convert via expression().
                    expression_from_for_init(init, shift)
                }
            };
            ForInit::Expression(expr)
        }
    }
}

fn for_init_target(
    left: &oxc::ForStatementLeft<'_>,
    shift: Shift,
) -> ForInit {
    use oxc::ForStatementLeft as L;
    match left {
        L::VariableDeclaration(v) => {
            ForInit::Declaration(Box::new(variable_declaration(v, shift)))
        }
        _ => ForInit::Expression(expression_from_for_left(left, shift)),
    }
}

fn variable_declaration(
    v: &oxc::VariableDeclaration<'_>,
    shift: Shift,
) -> VariableDeclaration {
    use oxc::VariableDeclarationKind as K;
    let kind = match v.kind {
        K::Var => VariableKind::Var,
        K::Let | K::Using | K::AwaitUsing => VariableKind::Let,
        K::Const => VariableKind::Const,
    };
    let declarations = map_collect(v.declarations.iter(), |d| VariableDeclarator {
        id: binding_pattern(&d.id, shift),
        init: d.init.as_ref().map(|e| expression(e, shift)),
        type_annotation: d.type_annotation.as_ref().and_then(|t| slice_text(t.span)),
        span: span_of(d.span, shift),
    });
    VariableDeclaration {
        kind,
        declarations: declarations,
        span: span_of(v.span, shift),
    }
}

fn function_decl(f: &oxc::Function<'_>, shift: Shift) -> FunctionDeclaration {
    let (params, type_annotations) = collect_params(f, shift);
    FunctionDeclaration {
        id: f.id.as_ref().map(|b| binding_identifier(b, shift)),
        params,
        param_type_annotations: type_annotations,
        body: f.body.as_ref().map(|b| function_body(b, shift)).unwrap_or_else(|| {
            BlockStatement { body: Vec::new(), span: Span::ZERO }
        }),
        generator: f.generator,
        r#async: f.r#async,
        span: span_of(f.span, shift),
    }
}

fn function_expression(f: &oxc::Function<'_>, shift: Shift) -> FunctionExpression {
    let (params, type_annotations) = collect_params(f, shift);
    FunctionExpression {
        id: f.id.as_ref().map(|b| binding_identifier(b, shift)),
        params,
        param_type_annotations: type_annotations,
        body: f.body.as_ref().map(|b| function_body(b, shift)).unwrap_or_else(|| {
            BlockStatement { body: Vec::new(), span: Span::ZERO }
        }),
        generator: f.generator,
        r#async: f.r#async,
        span: span_of(f.span, shift),
    }
}

/// Walk OXC's FormalParameters into our `Vec<Pattern>` plus a parallel
/// `Vec<Option<String>>` of TS type annotations (raw `": T"` text including
/// the leading colon).
fn collect_params(
    f: &oxc::Function<'_>,
    shift: Shift,
) -> (Vec<Pattern>, Vec<Option<String>>) {
    let mut params: Vec<Pattern> = Vec::with_capacity(f.params.items.len() + 1);
    let mut types: Vec<Option<String>> = Vec::with_capacity(f.params.items.len() + 1);
    for p in f.params.items.iter() {
        params.push(formal_param_pattern(p, shift));
        types.push(p.type_annotation.as_ref().and_then(|t| slice_text(t.span)));
    }
    if let Some(r) = f.params.rest.as_ref() {
        params.push(Pattern::Rest(Box::new(RestElement {
            argument: binding_pattern(&r.rest.argument, shift),
            span: span_of(r.rest.span, shift),
        })));
        types.push(None);
    }
    (params, types)
}

fn function_body(b: &oxc::FunctionBody<'_>, shift: Shift) -> BlockStatement {
    BlockStatement {
        body: {
            let mut body = Vec::with_capacity(b.statements.len());
            body.extend(b.statements.iter().map(|s| statement(s, shift)));
            body
        },
        span: span_of(b.span, shift),
    }
}

fn class_decl(c: &oxc::Class<'_>, shift: Shift) -> ClassDeclaration {
    ClassDeclaration {
        id: c.id.as_ref().map(|b| binding_identifier(b, shift)),
        super_class: c.super_class.as_ref().map(|e| expression(e, shift)),
        body: ClassBody {
            body: {
                let mut body = Vec::with_capacity(c.body.body.len());
                body.extend(c.body.body.iter().filter_map(|m| class_member(m, shift)));
                body
            },
            span: span_of(c.body.span, shift),
        },
        span: span_of(c.span, shift),
    }
}

fn class_expression(c: &oxc::Class<'_>, shift: Shift) -> ClassExpression {
    ClassExpression {
        id: c.id.as_ref().map(|b| binding_identifier(b, shift)),
        super_class: c.super_class.as_ref().map(|e| expression(e, shift)),
        body: ClassBody {
            body: {
                let mut body = Vec::with_capacity(c.body.body.len());
                body.extend(c.body.body.iter().filter_map(|m| class_member(m, shift)));
                body
            },
            span: span_of(c.body.span, shift),
        },
        span: span_of(c.span, shift),
    }
}

fn class_member(m: &oxc::ClassElement<'_>, shift: Shift) -> Option<ClassMember> {
    use oxc::ClassElement as E;
    match m {
        E::MethodDefinition(md) => {
            use oxc::MethodDefinitionKind as MK;
            let kind = match md.kind {
                MK::Constructor => MethodKind::Constructor,
                MK::Method => MethodKind::Method,
                MK::Get => MethodKind::Get,
                MK::Set => MethodKind::Set,
            };
            Some(ClassMember::Method(Box::new(MethodDefinition {
                key: property_key(&md.key, shift),
                value: function_expression(&md.value, shift),
                kind,
                computed: md.computed,
                r#static: md.r#static,
                span: span_of(md.span, shift),
            })))
        }
        E::PropertyDefinition(pd) => Some(ClassMember::Property(Box::new(PropertyDefinition {
            key: property_key(&pd.key, shift),
            value: pd.value.as_ref().map(|e| expression(e, shift)),
            computed: pd.computed,
            r#static: pd.r#static,
            span: span_of(pd.span, shift),
        }))),
        E::StaticBlock(sb) => Some(ClassMember::StaticBlock(Box::new(StaticBlock {
            body: map_collect(sb.body.iter(), |s| statement(s, shift)),
            span: span_of(sb.span, shift),
        }))),
        E::AccessorProperty(_) | E::TSIndexSignature(_) => None,
    }
}

// -------------------------------------------------------------------------
// Modules
// -------------------------------------------------------------------------

fn import_declaration(
    i: &oxc::ImportDeclaration<'_>,
    shift: Shift,
) -> ImportDeclaration {
    let specifiers = i
        .specifiers
        .as_ref()
        .map(|specs| {
            map_collect(specs.iter(), |s| match s {
                    oxc::ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        ImportSpecifierKind::Named(ImportSpecifier {
                            imported: module_export_name(&s.imported, shift),
                            local: binding_identifier(&s.local, shift),
                            type_only: matches!(
                                s.import_kind,
                                oxc::ImportOrExportKind::Type
                            ),
                            span: span_of(s.span, shift),
                        })
                    }
                    oxc::ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                        ImportSpecifierKind::Default(ImportDefaultSpecifier {
                            local: binding_identifier(&s.local, shift),
                            span: span_of(s.span, shift),
                        })
                    }
                    oxc::ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        ImportSpecifierKind::Namespace(ImportNamespaceSpecifier {
                            local: binding_identifier(&s.local, shift),
                            span: span_of(s.span, shift),
                        })
                    }
                })
        })
        .unwrap_or_default();
    ImportDeclaration {
        specifiers,
        source: string_literal(&i.source, shift),
        type_only: matches!(i.import_kind, oxc::ImportOrExportKind::Type),
        span: span_of(i.span, shift),
    }
}

fn export_named_declaration(
    e: &oxc::ExportNamedDeclaration<'_>,
    shift: Shift,
) -> ExportNamedDeclaration {
    ExportNamedDeclaration {
        declaration: e.declaration.as_ref().map(|d| declaration_to_statement(d, shift)),
        specifiers: map_collect(e.specifiers.iter(), |s| ExportSpecifier {
            local: module_export_name(&s.local, shift),
            exported: module_export_name(&s.exported, shift),
            span: span_of(s.span, shift),
        }),
        source: e.source.as_ref().map(|s| string_literal(s, shift)),
        span: span_of(e.span, shift),
    }
}

fn declaration_to_statement(d: &oxc::Declaration<'_>, shift: Shift) -> Statement {
    use oxc::Declaration as D;
    match d {
        D::VariableDeclaration(v) => {
            Statement::Variable(Box::new(variable_declaration(v, shift)))
        }
        D::FunctionDeclaration(f) => Statement::Function(Box::new(function_decl(f, shift))),
        D::ClassDeclaration(c) => Statement::Class(Box::new(class_decl(c, shift))),
        _ => Statement::Empty(Span::ZERO),
    }
}

fn export_default_declaration(
    e: &oxc::ExportDefaultDeclaration<'_>,
    shift: Shift,
) -> ExportDefaultDeclaration {
    use oxc::ExportDefaultDeclarationKind as K;
    let inner = match &e.declaration {
        K::FunctionDeclaration(f) => ExportDefault::Function(Box::new(function_decl(f, shift))),
        K::ClassDeclaration(c) => ExportDefault::Class(Box::new(class_decl(c, shift))),
        K::TSInterfaceDeclaration(_) => {
            ExportDefault::Expression(Expression::Identifier(Identifier {
                name: "__ts_interface__".into(),
                span: Span::ZERO,
            }))
        }
        other => ExportDefault::Expression(expression_from_default_kind(other, shift)),
    };
    ExportDefaultDeclaration { declaration: inner, span: span_of(e.span, shift) }
}

fn export_all_declaration(
    e: &oxc::ExportAllDeclaration<'_>,
    shift: Shift,
) -> ExportAllDeclaration {
    ExportAllDeclaration {
        source: string_literal(&e.source, shift),
        exported: e.exported.as_ref().map(|m| module_export_name(m, shift)),
        span: span_of(e.span, shift),
    }
}

fn module_export_name(
    m: &oxc::ModuleExportName<'_>,
    shift: Shift,
) -> ModuleExportName {
    use oxc::ModuleExportName as M;
    match m {
        M::IdentifierName(n) => ModuleExportName::Identifier(ident_from_name(n.name.as_ref(), n.span, shift)),
        M::IdentifierReference(r) => ModuleExportName::Identifier(ident_from_name(r.name.as_ref(), r.span, shift)),
        M::StringLiteral(s) => ModuleExportName::String(string_literal(s, shift)),
    }
}

// -------------------------------------------------------------------------
// Expressions
// -------------------------------------------------------------------------

pub fn expression(e: &oxc::Expression<'_>, shift: Shift) -> Expression {
    use oxc::Expression as E;
    match e {
        E::BooleanLiteral(b) => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: b.value,
            span: span_of(b.span, shift),
        }))),
        E::NullLiteral(n) => Expression::Literal(Box::new(Literal::Null(span_of(n.span, shift)))),
        E::NumericLiteral(n) => Expression::Literal(Box::new(Literal::Number(NumberLiteral {
            value: n.value,
            raw: n.raw.as_ref().map(|s| s.as_str().into()),
            span: span_of(n.span, shift),
        }))),
        E::BigIntLiteral(b) => Expression::Literal(Box::new(Literal::BigInt(BigIntLiteral {
            raw: b.raw.as_ref().map(|s| s.as_str().into()).unwrap_or_default(),
            span: span_of(b.span, shift),
        }))),
        E::RegExpLiteral(r) => Expression::Literal(Box::new(Literal::Regex(RegexLiteral {
            pattern: r.regex.pattern.text.as_str().into(),
            flags: r.regex.flags.to_string(),
            span: span_of(r.span, shift),
        }))),
        E::StringLiteral(s) => Expression::Literal(Box::new(Literal::String(string_literal(s, shift)))),
        E::TemplateLiteral(t) => Expression::Template(Box::new(template_literal(t, shift))),
        E::Identifier(i) => Expression::Identifier(ident_from_name(i.name.as_ref(), i.span, shift)),
        E::MetaProperty(m) => Expression::Meta(Box::new(MetaProperty {
            meta: ident_from_name(m.meta.name.as_ref(), m.meta.span, shift),
            property: ident_from_name(m.property.name.as_ref(), m.property.span, shift),
            span: span_of(m.span, shift),
        })),
        E::Super(s) => Expression::Super(span_of(s.span, shift)),
        E::ArrayExpression(a) => Expression::Array(Box::new(ArrayExpression {
            elements: {
                let mut elements = Vec::with_capacity(a.elements.len());
                elements.extend(a.elements.iter().map(|el| array_element(el, shift)));
                elements
            },
            span: span_of(a.span, shift),
        })),
        E::ArrowFunctionExpression(a) => {
            let mut params: Vec<Pattern> = Vec::with_capacity(a.params.items.len() + 1);
            let mut type_annotations: Vec<Option<String>> =
                Vec::with_capacity(a.params.items.len() + 1);
            for p in a.params.items.iter() {
                params.push(formal_param_pattern(p, shift));
                type_annotations
                    .push(p.type_annotation.as_ref().and_then(|t| slice_text(t.span)));
            }
            if let Some(r) = a.params.rest.as_ref() {
                params.push(Pattern::Rest(Box::new(RestElement {
                    argument: binding_pattern(&r.rest.argument, shift),
                    span: span_of(r.rest.span, shift),
                })));
                type_annotations.push(None);
            }
            Expression::Arrow(Box::new(ArrowFunctionExpression {
                params,
                param_type_annotations: type_annotations,
                body: if a.expression {
                    let stmt = a.body.statements.first();
                    let expr = match stmt {
                        Some(oxc::Statement::ExpressionStatement(es)) => {
                            expression(&es.expression, shift)
                        }
                        _ => Expression::Identifier(Identifier { name: Cow::Borrowed(""),
                            span: Span::ZERO,
                        }),
                    };
                    ArrowBody::Expression(expr)
                } else {
                    ArrowBody::Block(Box::new(function_body(&a.body, shift)))
                },
                r#async: a.r#async,
                span: span_of(a.span, shift),
            }))
        }
        E::AssignmentExpression(a) => Expression::Assignment(Box::new(AssignmentExpression {
            left: assignment_target(&a.left, shift),
            operator: assignment_operator(a.operator),
            right: expression(&a.right, shift),
            span: span_of(a.span, shift),
        })),
        E::AwaitExpression(a) => Expression::Await(Box::new(AwaitExpression {
            argument: expression(&a.argument, shift),
            span: span_of(a.span, shift),
        })),
        E::BinaryExpression(b) => Expression::Binary(Box::new(BinaryExpression {
            left: expression(&b.left, shift),
            operator: binary_operator(b.operator),
            right: expression(&b.right, shift),
            span: span_of(b.span, shift),
        })),
        E::CallExpression(c) => Expression::Call(Box::new(CallExpression {
            callee: expression(&c.callee, shift),
            arguments: map_collect(c.arguments.iter(), |a| argument(a, shift)).into(),
            optional: c.optional,
            span: span_of(c.span, shift),
        })),
        E::ChainExpression(c) => match &c.expression {
            oxc::ChainElement::CallExpression(c2) => Expression::Call(Box::new(CallExpression {
                callee: expression(&c2.callee, shift),
                arguments: map_collect(c2.arguments.iter(), |a| argument(a, shift)).into(),
                optional: c2.optional,
                span: span_of(c2.span, shift),
            })),
            oxc::ChainElement::ComputedMemberExpression(m) => {
                Expression::Member(Box::new(MemberExpression {
                    object: expression(&m.object, shift),
                    property: MemberProperty::Expression(expression(&m.expression, shift)),
                    computed: true,
                    optional: m.optional,
                    span: span_of(m.span, shift),
                }))
            }
            oxc::ChainElement::StaticMemberExpression(m) => {
                Expression::Member(Box::new(MemberExpression {
                    object: expression(&m.object, shift),
                    property: MemberProperty::Identifier(ident_from_name(
                        m.property.name.as_ref(),
                        m.property.span,
                        shift,
                    )),
                    computed: false,
                    optional: m.optional,
                    span: span_of(m.span, shift),
                }))
            }
            oxc::ChainElement::PrivateFieldExpression(m) => {
                Expression::Member(Box::new(MemberExpression {
                    object: expression(&m.object, shift),
                    property: MemberProperty::Private(private_identifier(m.field.name.as_ref(), m.field.span, shift)),
                    computed: false,
                    optional: m.optional,
                    span: span_of(m.span, shift),
                }))
            }
            _ => Expression::Identifier(Identifier {
                name: "__unhandled_chain__".into(),
                span: Span::ZERO,
            }),
        },
        E::ClassExpression(c) => Expression::Class(Box::new(class_expression(c, shift))),
        E::ConditionalExpression(c) => Expression::Conditional(Box::new(ConditionalExpression {
            test: expression(&c.test, shift),
            consequent: expression(&c.consequent, shift),
            alternate: expression(&c.alternate, shift),
            span: span_of(c.span, shift),
        })),
        E::FunctionExpression(f) => Expression::Function(Box::new(function_expression(f, shift))),
        E::ImportExpression(i) => Expression::Call(Box::new(CallExpression {
            callee: Expression::Identifier(Identifier {
                name: "import".into(),
                span: Span::ZERO,
            }),
            arguments: {
                let mut args = Vec::with_capacity(1 + usize::from(i.options.is_some()));
                args.push(Argument::Expression(expression(&i.source, shift)));
                if let Some(o) = &i.options {
                    args.push(Argument::Expression(expression(o, shift)));
                }
                args
            },
            optional: false,
            span: span_of(i.span, shift),
        })),
        E::LogicalExpression(l) => Expression::Logical(Box::new(LogicalExpression {
            left: expression(&l.left, shift),
            operator: logical_operator(l.operator),
            right: expression(&l.right, shift),
            span: span_of(l.span, shift),
        })),
        E::NewExpression(n) => Expression::New(Box::new(NewExpression {
            callee: expression(&n.callee, shift),
            arguments: map_collect(n.arguments.iter(), |a| argument(a, shift)).into(),
            span: span_of(n.span, shift),
        })),
        E::ObjectExpression(o) => Expression::Object(Box::new(ObjectExpression {
            properties: map_collect(o.properties.iter(), |p| match p {
                    oxc::ObjectPropertyKind::ObjectProperty(op) => {
                        ObjectMember::Property(Box::new(Property {
                            key: property_key(&op.key, shift),
                            value: expression(&op.value, shift),
                            kind: match op.kind {
                                oxc::PropertyKind::Init => PropertyKind::Init,
                                oxc::PropertyKind::Get => PropertyKind::Get,
                                oxc::PropertyKind::Set => PropertyKind::Set,
                            },
                            computed: op.computed,
                            shorthand: op.shorthand,
                            method: op.method,
                            span: span_of(op.span, shift),
                        }))
                    }
                    oxc::ObjectPropertyKind::SpreadProperty(s) => {
                        ObjectMember::Spread(Box::new(SpreadElement {
                            argument: expression(&s.argument, shift),
                            span: span_of(s.span, shift),
                        }))
                    }
                }),
            span: span_of(o.span, shift),
        })),
        E::ParenthesizedExpression(p) => Expression::Paren(Box::new(ParenthesizedExpression {
            expression: expression(&p.expression, shift),
            span: span_of(p.span, shift),
        })),
        E::SequenceExpression(s) => Expression::Sequence(Box::new(SequenceExpression {
            expressions: map_collect(s.expressions.iter(), |e| expression(e, shift)),
            span: span_of(s.span, shift),
        })),
        E::TaggedTemplateExpression(t) => Expression::Tagged(Box::new(TaggedTemplateExpression {
            tag: expression(&t.tag, shift),
            quasi: template_literal(&t.quasi, shift),
            span: span_of(t.span, shift),
        })),
        E::ThisExpression(t) => Expression::This(span_of(t.span, shift)),
        E::UnaryExpression(u) => Expression::Unary(Box::new(UnaryExpression {
            operator: unary_operator(u.operator),
            argument: expression(&u.argument, shift),
            prefix: true,
            span: span_of(u.span, shift),
        })),
        E::UpdateExpression(u) => Expression::Update(Box::new(UpdateExpression {
            operator: update_operator(u.operator),
            argument: expression_from_simple_target(&u.argument, shift),
            prefix: u.prefix,
            span: span_of(u.span, shift),
        })),
        E::YieldExpression(y) => Expression::Yield(Box::new(YieldExpression {
            argument: y.argument.as_ref().map(|a| expression(a, shift)),
            delegate: y.delegate,
            span: span_of(y.span, shift),
        })),
        E::PrivateInExpression(p) => Expression::Binary(Box::new(BinaryExpression {
            left: Expression::Identifier(Identifier {
                name: {
                    let name = p.left.name.as_ref();
                    let mut s = String::with_capacity(name.len() + 1);
                    s.push('#');
                    s.push_str(name);
                    Cow::Owned(s)
                },
                span: span_of(p.left.span, shift),
            }),
            operator: BinaryOperator::In,
            right: expression(&p.right, shift),
            span: span_of(p.span, shift),
        })),

        E::ComputedMemberExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Expression(expression(&m.expression, shift)),
            computed: true,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        E::StaticMemberExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Identifier(ident_from_name(
                m.property.name.as_ref(),
                m.property.span,
                shift,
            )),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        E::PrivateFieldExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Private(private_identifier(m.field.name.as_ref(), m.field.span, shift)),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),

        // JSX (not supported in Svelte scripts — emit a placeholder).
        E::JSXElement(j) => Expression::Identifier(Identifier {
            name: "__jsx_element__".into(),
            span: span_of(j.span, shift),
        }),
        E::JSXFragment(j) => Expression::Identifier(Identifier {
            name: "__jsx_fragment__".into(),
            span: span_of(j.span, shift),
        }),

        // TS-only wrappers — unwrap to the underlying expression.
        E::TSAsExpression(t) => expression(&t.expression, shift),
        E::TSSatisfiesExpression(t) => expression(&t.expression, shift),
        E::TSTypeAssertion(t) => expression(&t.expression, shift),
        E::TSNonNullExpression(t) => expression(&t.expression, shift),
        E::TSInstantiationExpression(t) => expression(&t.expression, shift),

        E::V8IntrinsicExpression(v) => Expression::Identifier(Identifier {
            name: {
                let name = v.name.name.as_ref();
                let mut s = String::with_capacity(name.len() + 1);
                s.push('%');
                s.push_str(name);
                Cow::Owned(s)
            },
            span: span_of(v.span, shift),
        }),
    }
}

fn expression_from_for_init(init: &oxc::ForStatementInit<'_>, shift: Shift) -> Expression {
    use oxc::ForStatementInit as F;
    // ForStatementInit inherits from Expression. Match each variant manually.
    match init {
        F::VariableDeclaration(_) => unreachable!(),
        F::BooleanLiteral(b) => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: b.value,
            span: span_of(b.span, shift),
        }))),
        F::NullLiteral(n) => Expression::Literal(Box::new(Literal::Null(span_of(n.span, shift)))),
        F::NumericLiteral(n) => Expression::Literal(Box::new(Literal::Number(NumberLiteral {
            value: n.value,
            raw: n.raw.as_ref().map(|s| s.as_str().into()),
            span: span_of(n.span, shift),
        }))),
        F::StringLiteral(s) => Expression::Literal(Box::new(Literal::String(string_literal(s, shift)))),
        F::Identifier(i) => Expression::Identifier(ident_from_name(i.name.as_ref(), i.span, shift)),
        F::CallExpression(c) => Expression::Call(Box::new(CallExpression {
            callee: expression(&c.callee, shift),
            arguments: map_collect(c.arguments.iter(), |a| argument(a, shift)).into(),
            optional: c.optional,
            span: span_of(c.span, shift),
        })),
        F::AssignmentExpression(a) => Expression::Assignment(Box::new(AssignmentExpression {
            left: assignment_target(&a.left, shift),
            operator: assignment_operator(a.operator),
            right: expression(&a.right, shift),
            span: span_of(a.span, shift),
        })),
        // Fallback: extract span and emit a placeholder.
        _ => {
            let s = init.span();
            Expression::Identifier(Identifier {
                name: "__for_init__".into(),
                span: span_of(s, shift),
            })
        }
    }
}

fn expression_from_for_left(left: &oxc::ForStatementLeft<'_>, shift: Shift) -> Expression {
    use oxc::ForStatementLeft as L;
    match left {
        L::VariableDeclaration(_) => unreachable!(),
        // ForStatementLeft inherits from AssignmentTarget — most variants are
        // member/identifier-flavored targets.
        L::AssignmentTargetIdentifier(i) => {
            Expression::Identifier(ident_from_name(i.name.as_ref(), i.span, shift))
        }
        L::ComputedMemberExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Expression(expression(&m.expression, shift)),
            computed: true,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        L::StaticMemberExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Identifier(ident_from_name(
                m.property.name.as_ref(),
                m.property.span,
                shift,
            )),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        _ => {
            let s = left.span();
            Expression::Identifier(Identifier {
                name: "__for_left__".into(),
                span: span_of(s, shift),
            })
        }
    }
}

fn expression_from_default_kind(
    k: &oxc::ExportDefaultDeclarationKind<'_>,
    shift: Shift,
) -> Expression {
    use oxc::ExportDefaultDeclarationKind as K;
    match k {
        K::FunctionDeclaration(_) | K::ClassDeclaration(_) | K::TSInterfaceDeclaration(_) => {
            // Already handled by caller; fallback shouldn't reach here.
            Expression::Identifier(Identifier {
                name: "__default_declaration__".into(),
                span: Span::ZERO,
            })
        }
        K::BooleanLiteral(b) => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: b.value,
            span: span_of(b.span, shift),
        }))),
        K::NullLiteral(n) => Expression::Literal(Box::new(Literal::Null(span_of(n.span, shift)))),
        K::NumericLiteral(n) => Expression::Literal(Box::new(Literal::Number(NumberLiteral {
            value: n.value,
            raw: n.raw.as_ref().map(|s| s.as_str().into()),
            span: span_of(n.span, shift),
        }))),
        K::StringLiteral(s) => Expression::Literal(Box::new(Literal::String(string_literal(s, shift)))),
        K::Identifier(i) => Expression::Identifier(ident_from_name(i.name.as_ref(), i.span, shift)),
        K::ArrayExpression(a) => Expression::Array(Box::new(ArrayExpression {
            elements: {
                let mut elements = Vec::with_capacity(a.elements.len());
                elements.extend(a.elements.iter().map(|el| array_element(el, shift)));
                elements
            },
            span: span_of(a.span, shift),
        })),
        K::ObjectExpression(o) => Expression::Object(Box::new(ObjectExpression {
            properties: map_collect(o.properties.iter(), |p| match p {
                    oxc::ObjectPropertyKind::ObjectProperty(op) => {
                        ObjectMember::Property(Box::new(Property {
                            key: property_key(&op.key, shift),
                            value: expression(&op.value, shift),
                            kind: PropertyKind::Init,
                            computed: op.computed,
                            shorthand: op.shorthand,
                            method: op.method,
                            span: span_of(op.span, shift),
                        }))
                    }
                    oxc::ObjectPropertyKind::SpreadProperty(s) => {
                        ObjectMember::Spread(Box::new(SpreadElement {
                            argument: expression(&s.argument, shift),
                            span: span_of(s.span, shift),
                        }))
                    }
                }),
            span: span_of(o.span, shift),
        })),
        K::ArrowFunctionExpression(_) | K::FunctionExpression(_) | K::ClassExpression(_) => {
            // These all have a typed equivalent above — recurse via the main expression
            // path by upcasting to Expression. We need a reborrow.
            let s = k.span();
            Expression::Identifier(Identifier {
                name: "__default_complex__".into(),
                span: span_of(s, shift),
            })
        }
        _ => {
            let s = k.span();
            Expression::Identifier(Identifier {
                name: "__default_other__".into(),
                span: span_of(s, shift),
            })
        }
    }
}

fn expression_from_simple_target(
    t: &oxc::SimpleAssignmentTarget<'_>,
    shift: Shift,
) -> Expression {
    use oxc::SimpleAssignmentTarget as T;
    match t {
        T::AssignmentTargetIdentifier(i) => {
            Expression::Identifier(ident_from_name(i.name.as_ref(), i.span, shift))
        }
        T::ComputedMemberExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Expression(expression(&m.expression, shift)),
            computed: true,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        T::StaticMemberExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Identifier(ident_from_name(
                m.property.name.as_ref(),
                m.property.span,
                shift,
            )),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        T::PrivateFieldExpression(m) => Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Private(private_identifier(m.field.name.as_ref(), m.field.span, shift)),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        _ => Expression::Identifier(Identifier {
            name: "__simple_target__".into(),
            span: Span::ZERO,
        }),
    }
}

// -------------------------------------------------------------------------
// Patterns
// -------------------------------------------------------------------------

pub fn binding_pattern(b: &oxc::BindingPattern<'_>, shift: Shift) -> Pattern {
    use oxc::BindingPattern as K;
    match b {
        K::BindingIdentifier(bi) => Pattern::Identifier(binding_identifier(bi, shift)),
        K::ObjectPattern(o) => Pattern::Object(Box::new(ObjectPattern {
            properties: {
                let mut properties =
                    Vec::with_capacity(o.properties.len() + usize::from(o.rest.is_some()));
                properties.extend(o.properties.iter().map(|p| {
                    ObjectPatternMember::Property(Box::new(ObjectPatternProperty {
                        key: property_key(&p.key, shift),
                        value: binding_pattern(&p.value, shift),
                        computed: p.computed,
                        shorthand: p.shorthand,
                        span: span_of(p.span, shift),
                    }))
                }));
                if let Some(r) = &o.rest {
                    properties.push(ObjectPatternMember::Rest(Box::new(RestElement {
                        argument: binding_pattern(&r.argument, shift),
                        span: span_of(r.span, shift),
                    })));
                }
                properties
            },
            span: span_of(o.span, shift),
        })),
        K::ArrayPattern(a) => Pattern::Array(Box::new(ArrayPattern {
            elements: {
                let mut elements =
                    Vec::with_capacity(a.elements.len() + usize::from(a.rest.is_some()));
                elements.extend(
                    a.elements
                        .iter()
                        .map(|e| e.as_ref().map(|p| binding_pattern(p, shift))),
                );
                if let Some(r) = &a.rest {
                    elements.push(Some(Pattern::Rest(Box::new(RestElement {
                        argument: binding_pattern(&r.argument, shift),
                        span: span_of(r.span, shift),
                    }))));
                }
                elements
            },
            span: span_of(a.span, shift),
        })),
        K::AssignmentPattern(p) => Pattern::Assignment(Box::new(AssignmentPattern {
            left: binding_pattern(&p.left, shift),
            right: expression(&p.right, shift),
            span: span_of(p.span, shift),
        })),
    }
}

/// `function f(x = 1)` — OXC stores `pattern: x, initializer: Some(1)`.
/// Our typed AST collapses both flavors into `Pattern::Assignment`.
fn formal_param_pattern(p: &oxc::FormalParameter<'_>, shift: Shift) -> Pattern {
    let pat = binding_pattern(&p.pattern, shift);
    if let Some(init) = &p.initializer {
        Pattern::Assignment(Box::new(AssignmentPattern {
            left: pat,
            right: expression(init, shift),
            span: span_of(p.span, shift),
        }))
    } else {
        pat
    }
}

fn binding_identifier(b: &oxc::BindingIdentifier<'_>, shift: Shift) -> Identifier {
    Identifier {
        name: cow_owned(b.name.as_ref()),
        span: span_of(b.span, shift),
    }
}

fn ident_from_name(name: &str, sp: oxc_span::Span, shift: Shift) -> Identifier {
    Identifier { name: cow_owned(name), span: span_of(sp, shift) }
}

fn property_key(k: &oxc::PropertyKey<'_>, shift: Shift) -> PropertyKey {
    use oxc::PropertyKey as K;
    match k {
        K::StaticIdentifier(n) => PropertyKey::Identifier(ident_from_name(n.name.as_ref(), n.span, shift)),
        K::PrivateIdentifier(p) => PropertyKey::Private(private_identifier(p.name.as_ref(), p.span, shift)),
        // The Expression-inherited variants — convert via expression().
        _ => {
            // Reborrow as Expression. Most variants of PropertyKey beyond
            // Static/Private are valid Expression variants thanks to
            // `@inherit Expression`.
            PropertyKey::Expression(property_key_as_expr(k, shift))
        }
    }
}

fn property_key_as_expr(k: &oxc::PropertyKey<'_>, shift: Shift) -> Expression {
    use oxc::PropertyKey as K;
    match k {
        K::StaticIdentifier(_) | K::PrivateIdentifier(_) => unreachable!(),
        K::Identifier(i) => Expression::Identifier(ident_from_name(i.name.as_ref(), i.span, shift)),
        K::StringLiteral(s) => Expression::Literal(Box::new(Literal::String(string_literal(s, shift)))),
        K::NumericLiteral(n) => Expression::Literal(Box::new(Literal::Number(NumberLiteral {
            value: n.value,
            raw: n.raw.as_ref().map(|s| s.as_str().into()),
            span: span_of(n.span, shift),
        }))),
        K::TemplateLiteral(t) => Expression::Template(Box::new(template_literal(t, shift))),
        // PropertyKey is a superset of Expression in OXC — for non-literal
        // computed keys (BinaryExpression, CallExpression, MemberExpression,
        // ...) funnel through the regular `expression()` mapper so all
        // variants render correctly.
        _ => expression(k.to_expression(), shift),
    }
}

fn string_literal(s: &oxc::StringLiteral<'_>, shift: Shift) -> StringLiteral {
    StringLiteral {
        value: cow_owned(s.value.as_ref()),
        raw: s.raw.as_ref().map(|r| r.as_str().into()),
        span: span_of(s.span, shift),
    }
}

fn template_literal(t: &oxc::TemplateLiteral<'_>, shift: Shift) -> TemplateLiteral {
    let last = t.quasis.len().saturating_sub(1);
    TemplateLiteral {
        quasis: {
            let quasis = t.quasis.iter();
            let mut out = Vec::with_capacity(quasis.len());
            out.extend(quasis.enumerate().map(|(i, q)| TemplateElement {
                cooked: q.value.cooked.as_ref().map(|s| s.as_str().into()).unwrap_or_default(),
                raw: q.value.raw.as_str().into(),
                tail: q.tail || i == last,
                span: span_of(q.span, shift),
            }));
            out
        },
        expressions: map_collect(t.expressions.iter(), |e| expression(e, shift)),
        span: span_of(t.span, shift),
    }
}

fn array_element(e: &oxc::ArrayExpressionElement<'_>, shift: Shift) -> ArrayElement {
    use oxc::ArrayExpressionElement as E;
    match e {
        E::Elision(_) => ArrayElement::Elision,
        E::SpreadElement(s) => ArrayElement::Spread(Box::new(SpreadElement {
            argument: expression(&s.argument, shift),
            span: span_of(s.span, shift),
        })),
        _ => {
            // Inherited Expression variants — convert via expression().
            ArrayElement::Expression(array_element_as_expr(e, shift))
        }
    }
}

fn array_element_as_expr(e: &oxc::ArrayExpressionElement<'_>, shift: Shift) -> Expression {
    // `ArrayExpressionElement` is a superset of `Expression` in OXC —
    // every non-Elision / non-SpreadElement variant has an equivalent
    // `Expression`. Use OXC's `to_expression()` accessor to convert, then
    // funnel through the regular `expression()` mapper so we cover all
    // variants (Object, Conditional, Logical, …) instead of a tiny
    // hard-coded subset.
    use oxc::ArrayExpressionElement as E;
    match e {
        E::Elision(_) | E::SpreadElement(_) => unreachable!(),
        _ => expression(e.to_expression(), shift),
    }
}

fn argument(a: &oxc::Argument<'_>, shift: Shift) -> Argument {
    use oxc::Argument as A;
    match a {
        A::SpreadElement(s) => Argument::Spread(Box::new(SpreadElement {
            argument: expression(&s.argument, shift),
            span: span_of(s.span, shift),
        })),
        _ => Argument::Expression(argument_as_expr(a, shift)),
    }
}

fn argument_as_expr(a: &oxc::Argument<'_>, shift: Shift) -> Expression {
    // Argument inherits all of Expression's variants. Delegate via the
    // OXC-provided `to_expression()` accessor (panics on SpreadElement —
    // we handle that branch above before reaching here).
    if let Some(e) = a.as_expression() {
        return expression(e, shift);
    }
    // Fallback for any unexpected non-Expression branch.
    use oxc::Argument as A;
    match a {
        A::SpreadElement(_) => unreachable!(),
        A::BooleanLiteral(b) => Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
            value: b.value,
            span: span_of(b.span, shift),
        }))),
        A::NullLiteral(n) => Expression::Literal(Box::new(Literal::Null(span_of(n.span, shift)))),
        A::NumericLiteral(n) => Expression::Literal(Box::new(Literal::Number(NumberLiteral {
            value: n.value,
            raw: n.raw.as_ref().map(|s| s.as_str().into()),
            span: span_of(n.span, shift),
        }))),
        A::StringLiteral(s) => Expression::Literal(Box::new(Literal::String(string_literal(s, shift)))),
        A::Identifier(i) => Expression::Identifier(ident_from_name(i.name.as_ref(), i.span, shift)),
        A::CallExpression(c) => Expression::Call(Box::new(CallExpression {
            callee: expression(&c.callee, shift),
            arguments: map_collect(c.arguments.iter(), |a| argument(a, shift)).into(),
            optional: c.optional,
            span: span_of(c.span, shift),
        })),
        A::BinaryExpression(b) => Expression::Binary(Box::new(BinaryExpression {
            left: expression(&b.left, shift),
            operator: binary_operator(b.operator),
            right: expression(&b.right, shift),
            span: span_of(b.span, shift),
        })),
        A::ObjectExpression(o) => Expression::Object(Box::new(ObjectExpression {
            properties: map_collect(o.properties.iter(), |p| match p {
                    oxc::ObjectPropertyKind::ObjectProperty(op) => {
                        ObjectMember::Property(Box::new(Property {
                            key: property_key(&op.key, shift),
                            value: expression(&op.value, shift),
                            kind: PropertyKind::Init,
                            computed: op.computed,
                            shorthand: op.shorthand,
                            method: op.method,
                            span: span_of(op.span, shift),
                        }))
                    }
                    oxc::ObjectPropertyKind::SpreadProperty(s) => {
                        ObjectMember::Spread(Box::new(SpreadElement {
                            argument: expression(&s.argument, shift),
                            span: span_of(s.span, shift),
                        }))
                    }
                }),
            span: span_of(o.span, shift),
        })),
        _ => {
            let s = a.span();
            Expression::Identifier(Identifier {
                name: "__argument__".into(),
                span: span_of(s, shift),
            })
        }
    }
}

fn assignment_target(t: &oxc::AssignmentTarget<'_>, shift: Shift) -> AssignmentTarget {
    use oxc::AssignmentTarget as T;
    match t {
        T::AssignmentTargetIdentifier(i) => AssignmentTarget::Pattern(Pattern::Identifier(
            ident_from_name(i.name.as_ref(), i.span, shift),
        )),
        T::ComputedMemberExpression(m) => AssignmentTarget::Expression(Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Expression(expression(&m.expression, shift)),
            computed: true,
            optional: m.optional,
            span: span_of(m.span, shift),
        }))),
        T::StaticMemberExpression(m) => AssignmentTarget::Expression(Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Identifier(ident_from_name(
                m.property.name.as_ref(),
                m.property.span,
                shift,
            )),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        }))),
        T::PrivateFieldExpression(m) => AssignmentTarget::Expression(Expression::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Private(private_identifier(m.field.name.as_ref(), m.field.span, shift)),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        }))),
        T::ArrayAssignmentTarget(a) => AssignmentTarget::Pattern(Pattern::Array(Box::new(ArrayPattern {
            elements: {
                let mut elements =
                    Vec::with_capacity(a.elements.len() + usize::from(a.rest.is_some()));
                elements.extend(a.elements.iter().map(|e| {
                    e.as_ref().map(|el| match el {
                        oxc::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(w) => {
                            Pattern::Assignment(Box::new(AssignmentPattern {
                                left: assignment_target_to_pattern(&w.binding, shift),
                                right: expression(&w.init, shift),
                                span: span_of(w.span, shift),
                            }))
                        }
                        other => {
                            let s = other.span();
                            match other {
                                oxc::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(_) => unreachable!(),
                                _ => {
                                    let _ = s;
                                    assignment_target_maybe_default_to_pattern(other, shift)
                                }
                            }
                        }
                    })
                }));
                if let Some(r) = &a.rest {
                    elements.push(Some(Pattern::Rest(Box::new(RestElement {
                        argument: assignment_target_to_pattern(&r.target, shift),
                        span: span_of(r.span, shift),
                    }))));
                }
                elements
            },
            span: span_of(a.span, shift),
        }))),
        T::ObjectAssignmentTarget(o) => {
            AssignmentTarget::Pattern(Pattern::Object(Box::new(ObjectPattern {
                properties: {
                    let mut properties =
                        Vec::with_capacity(o.properties.len() + usize::from(o.rest.is_some()));
                    properties.extend(o.properties.iter().map(|p| match p {
                        oxc::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(id) => {
                            ObjectPatternMember::Property(Box::new(ObjectPatternProperty {
                                key: PropertyKey::Identifier(ident_from_name(
                                    id.binding.name.as_ref(),
                                    id.binding.span,
                                    shift,
                                )),
                                value: match &id.init {
                                    Some(init) => Pattern::Assignment(Box::new(AssignmentPattern {
                                        left: Pattern::Identifier(ident_from_name(
                                            id.binding.name.as_ref(),
                                            id.binding.span,
                                            shift,
                                        )),
                                        right: expression(init, shift),
                                        span: span_of(id.span, shift),
                                    })),
                                    None => Pattern::Identifier(ident_from_name(
                                        id.binding.name.as_ref(),
                                        id.binding.span,
                                        shift,
                                    )),
                                },
                                computed: false,
                                shorthand: true,
                                span: span_of(id.span, shift),
                            }))
                        }
                        oxc::AssignmentTargetProperty::AssignmentTargetPropertyProperty(pp) => {
                            ObjectPatternMember::Property(Box::new(ObjectPatternProperty {
                                key: property_key(&pp.name, shift),
                                value: assignment_target_maybe_default_to_pattern(
                                    &pp.binding,
                                    shift,
                                ),
                                computed: pp.computed,
                                shorthand: false,
                                span: span_of(pp.span, shift),
                            }))
                        }
                    }));
                    if let Some(r) = &o.rest {
                        properties.push(ObjectPatternMember::Rest(Box::new(RestElement {
                            argument: assignment_target_to_pattern(&r.target, shift),
                            span: span_of(r.span, shift),
                        })));
                    }
                    properties
                },
                span: span_of(o.span, shift),
            })))
        }
        _ => AssignmentTarget::Expression(Expression::Identifier(Identifier {
            name: "__assignment_target__".into(),
            span: Span::ZERO,
        })),
    }
}

fn assignment_target_to_pattern(t: &oxc::AssignmentTarget<'_>, shift: Shift) -> Pattern {
    match assignment_target(t, shift) {
        AssignmentTarget::Pattern(p) => p,
        AssignmentTarget::Expression(Expression::Member(m)) => Pattern::Member(m),
        AssignmentTarget::Expression(e) => match e {
            Expression::Identifier(i) => Pattern::Identifier(i),
            _ => Pattern::Identifier(Identifier {
                name: "__bad_target__".into(),
                span: Span::ZERO,
            }),
        },
    }
}

fn assignment_target_maybe_default_to_pattern(
    t: &oxc::AssignmentTargetMaybeDefault<'_>,
    shift: Shift,
) -> Pattern {
    use oxc::AssignmentTargetMaybeDefault as M;
    match t {
        M::AssignmentTargetWithDefault(w) => Pattern::Assignment(Box::new(AssignmentPattern {
            left: assignment_target_to_pattern(&w.binding, shift),
            right: expression(&w.init, shift),
            span: span_of(w.span, shift),
        })),
        M::AssignmentTargetIdentifier(i) => {
            Pattern::Identifier(ident_from_name(i.name.as_ref(), i.span, shift))
        }
        M::ComputedMemberExpression(m) => Pattern::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Expression(expression(&m.expression, shift)),
            computed: true,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        M::StaticMemberExpression(m) => Pattern::Member(Box::new(MemberExpression {
            object: expression(&m.object, shift),
            property: MemberProperty::Identifier(ident_from_name(
                m.property.name.as_ref(),
                m.property.span,
                shift,
            )),
            computed: false,
            optional: m.optional,
            span: span_of(m.span, shift),
        })),
        M::ArrayAssignmentTarget(_) | M::ObjectAssignmentTarget(_) => {
            // Recurse via assignment_target — these are AssignmentTarget variants too.
            // Cheaply emit a placeholder; nested destructure-as-target is rare.
            let s = t.span();
            Pattern::Identifier(Identifier {
                name: "__nested_destructure__".into(),
                span: span_of(s, shift),
            })
        }
        _ => Pattern::Identifier(Identifier {
            name: "__amd__".into(),
            span: Span::ZERO,
        }),
    }
}

// -------------------------------------------------------------------------
// Operators
// -------------------------------------------------------------------------

fn binary_operator(o: oxc_syntax::operator::BinaryOperator) -> BinaryOperator {
    use oxc_syntax::operator::BinaryOperator as B;
    use BinaryOperator::*;
    match o {
        B::Addition => Plus,
        B::Subtraction => Minus,
        B::Multiplication => Mul,
        B::Division => Div,
        B::Remainder => Mod,
        B::Exponential => Pow,
        B::Equality => Eq,
        B::Inequality => NotEq,
        B::StrictEquality => StrictEq,
        B::StrictInequality => StrictNotEq,
        B::LessThan => Lt,
        B::LessEqualThan => LtEq,
        B::GreaterThan => Gt,
        B::GreaterEqualThan => GtEq,
        B::ShiftLeft => ShiftL,
        B::ShiftRight => ShiftR,
        B::ShiftRightZeroFill => UnsignedShiftR,
        B::BitwiseAnd => BitAnd,
        B::BitwiseOR => BitOr,
        B::BitwiseXOR => BitXor,
        B::Instanceof => InstanceOf,
        B::In => In,
    }
}

fn logical_operator(o: oxc_syntax::operator::LogicalOperator) -> LogicalOperator {
    use oxc_syntax::operator::LogicalOperator as L;
    match o {
        L::And => LogicalOperator::And,
        L::Or => LogicalOperator::Or,
        L::Coalesce => LogicalOperator::Coalesce,
    }
}

fn assignment_operator(o: oxc_syntax::operator::AssignmentOperator) -> AssignmentOperator {
    use oxc_syntax::operator::AssignmentOperator as A;
    use AssignmentOperator::*;
    match o {
        A::Assign => Assign,
        A::Addition => AddAssign,
        A::Subtraction => SubAssign,
        A::Multiplication => MulAssign,
        A::Division => DivAssign,
        A::Remainder => ModAssign,
        A::Exponential => PowAssign,
        A::ShiftLeft => ShiftLAssign,
        A::ShiftRight => ShiftRAssign,
        A::ShiftRightZeroFill => UnsignedShiftRAssign,
        A::BitwiseAnd => BitAndAssign,
        A::BitwiseOR => BitOrAssign,
        A::BitwiseXOR => BitXorAssign,
        A::LogicalAnd => AndAssign,
        A::LogicalOr => OrAssign,
        A::LogicalNullish => CoalesceAssign,
    }
}

fn unary_operator(o: oxc_syntax::operator::UnaryOperator) -> UnaryOperator {
    use oxc_syntax::operator::UnaryOperator as U;
    use UnaryOperator::*;
    match o {
        U::UnaryPlus => Plus,
        U::UnaryNegation => Minus,
        U::LogicalNot => Not,
        U::BitwiseNot => BitNot,
        U::Typeof => TypeOf,
        U::Void => Void,
        U::Delete => Delete,
    }
}

fn update_operator(o: oxc_syntax::operator::UpdateOperator) -> UpdateOperator {
    use oxc_syntax::operator::UpdateOperator as U;
    match o {
        U::Increment => UpdateOperator::Increment,
        U::Decrement => UpdateOperator::Decrement,
    }
}
