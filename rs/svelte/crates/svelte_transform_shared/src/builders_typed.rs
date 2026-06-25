//! Typed-AST builders.
//!
//! Direct typed counterpart to `builders.rs` (which produces
//! `serde_json::Value` acorn-shaped JSON). The signatures are 1:1 with the
//! Value builders so visitors can swap call sites mechanically.
//!
//! Visitors that mix typed and Value during migration use `to_value` (TBD)
//! or `from_value` adapters at the boundary.

use std::borrow::Cow;

use svelte_js_ast::*;

pub fn id(name: &'static str) -> Expression {
    Expression::Identifier(Identifier {
        name: Cow::Borrowed(name),
        span: Span::ZERO,
    })
}

pub fn id_owned(name: String) -> Expression {
    Expression::Identifier(Identifier {
        name: Cow::Owned(name),
        span: Span::ZERO,
    })
}

pub fn id_with_span(name: &'static str, span: Span) -> Expression {
    Expression::Identifier(Identifier {
        name: Cow::Borrowed(name),
        span,
    })
}

pub fn pat_id(name: &'static str) -> Pattern {
    Pattern::Identifier(Identifier {
        name: Cow::Borrowed(name),
        span: Span::ZERO,
    })
}

pub fn pat_id_owned(name: String) -> Pattern {
    Pattern::Identifier(Identifier {
        name: Cow::Owned(name),
        span: Span::ZERO,
    })
}

// Zero-copy common identifier builders — centralize hot-path identifiers so
// call sites avoid repeating string literals and can be optimized in one place.
pub fn id_dollar() -> Expression {
    id("$")
}
pub fn id_anchor() -> Expression {
    id("$$anchor")
}
pub fn id_fragment() -> Expression {
    id("fragment")
}
pub fn id_renderer() -> Expression {
    id("$$renderer")
}
pub fn id_render() -> Expression {
    id("$$render")
}
pub fn pat_id_anchor() -> Pattern {
    pat_id("$$anchor")
}
pub fn pat_id_renderer() -> Pattern {
    pat_id("$$renderer")
}

pub fn literal_str(value: &'static str) -> Expression {
    Expression::Literal(Box::new(Literal::String(StringLiteral {
        value: Cow::Borrowed(value),
        raw: None,
        span: Span::ZERO,
    })))
}

pub fn literal_str_owned(value: String) -> Expression {
    Expression::Literal(Box::new(Literal::String(StringLiteral {
        value: Cow::Owned(value),
        raw: None,
        span: Span::ZERO,
    })))
}

pub fn literal_num(value: f64) -> Expression {
    Expression::Literal(Box::new(Literal::Number(NumberLiteral {
        value,
        raw: None,
        span: Span::ZERO,
    })))
}

pub fn literal_bool(value: bool) -> Expression {
    Expression::Literal(Box::new(Literal::Boolean(BooleanLiteral {
        value,
        span: Span::ZERO,
    })))
}

pub fn literal_null() -> Expression {
    Expression::Literal(Box::new(Literal::Null(Span::ZERO)))
}

pub fn lit_number(n: f64) -> Expression {
    Expression::Literal(Box::new(Literal::Number(NumberLiteral {
        value: n,
        raw: Some(format!("{n}")),
        span: Span::ZERO,
    })))
}

pub fn template_raw(parts: Vec<String>, exprs: Vec<Expression>) -> Expression {
    // parts has N entries; exprs has N-1. Build alternating quasi/expr.
    // Each quasi's raw form needs JS template-literal escaping:
    // - Backslash → `\\`
    // - Backtick → `` \` ``
    // - `${` interpolation prefix → `\${` (so it stays literal)
    let len = parts.len();
    let mut quasis = Vec::with_capacity(len);
    for (i, cooked) in parts.into_iter().enumerate() {
        let raw = if template_quasi_needs_escape(&cooked) {
            escape_template_quasi(&cooked)
        } else {
            cooked.clone()
        };
        quasis.push(TemplateElement {
            cooked,
            raw,
            tail: i == len - 1,
            span: Span::ZERO,
        });
    }
    Expression::Template(Box::new(TemplateLiteral {
        quasis,
        expressions: exprs,
        span: Span::ZERO,
    }))
}

fn template_quasi_needs_escape(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'\\' | b'`' => return true,
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => return true,
            _ => {}
        }
    }
    false
}

fn escape_template_quasi(s: &str) -> String {
    debug_assert!(template_quasi_needs_escape(s));
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\\' => {
                out.push_str("\\\\");
            }
            b'`' => {
                out.push_str("\\`");
            }
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                out.push_str("\\${");
                i += 2;
                continue;
            }
            _ => {
                out.push(c as char);
            }
        }
        i += 1;
    }
    out
}

pub fn array(elements: Vec<Expression>) -> Expression {
    Expression::Array(Box::new(ArrayExpression {
        elements: elements.into_iter().map(ArrayElement::Expression).collect(),
        span: Span::ZERO,
    }))
}

pub fn object(properties: Vec<ObjectMember>) -> Expression {
    Expression::Object(Box::new(ObjectExpression {
        properties: properties,
        span: Span::ZERO,
    }))
}

pub fn init(name: &str, value: Expression) -> ObjectMember {
    ObjectMember::Property(Box::new(Property {
        key: PropertyKey::Identifier(Identifier {
            name: Cow::Owned(name.to_string()),
            span: Span::ZERO,
        }),
        value,
        kind: PropertyKind::Init,
        computed: false,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    }))
}

pub fn prop(key: PropertyKey, value: Expression, kind: PropertyKind, computed: bool) -> ObjectMember {
    ObjectMember::Property(Box::new(Property {
        key,
        value,
        kind,
        computed,
        shorthand: false,
        method: false,
        span: Span::ZERO,
    }))
}

pub fn block(body: Vec<Statement>) -> BlockStatement {
    BlockStatement {
        body,
        span: Span::ZERO,
    }
}

pub fn stmt(expression: Expression) -> Statement {
    Statement::Expression(Box::new(ExpressionStatement {
        expression,
        span: Span::ZERO,
    }))
}

pub fn call(callee: Expression, args: Vec<Expression>) -> Expression {
    Expression::Call(Box::new(CallExpression {
        callee,
        arguments: args.into_iter().map(Argument::Expression).collect(),
        optional: false,
        span: Span::ZERO,
    }))
}

pub fn call_id(name: &str, args: Vec<Expression>) -> Expression {
    call(id_owned(name.to_string()), args)
}

pub fn member_id(object: Expression, property: &str) -> Expression {
    Expression::Member(Box::new(MemberExpression {
        object,
        property: MemberProperty::Identifier(Identifier {
            name: Cow::Owned(property.to_string()),
            span: Span::ZERO,
        }),
        computed: false,
        optional: false,
        span: Span::ZERO,
    }))
}

pub fn member_computed(object: Expression, property: Expression) -> Expression {
    Expression::Member(Box::new(MemberExpression {
        object,
        property: MemberProperty::Expression(property),
        computed: true,
        optional: false,
        span: Span::ZERO,
    }))
}

pub fn var(name: &str, init: Expression) -> Statement {
    Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Var,
        declarations: vec![VariableDeclarator {
            id: pat_id_owned(name.to_string()),
            init: Some(init),
            type_annotation: None,
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    }))
}

pub fn let_decl(name: &str, init: Option<Expression>) -> Statement {
    Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Let,
        declarations: vec![VariableDeclarator {
            id: pat_id_owned(name.to_string()),
            init,
            type_annotation: None,
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    }))
}

pub fn const_decl(name: &str, init: Expression) -> Statement {
    Statement::Variable(Box::new(VariableDeclaration {
        kind: VariableKind::Const,
        declarations: vec![VariableDeclarator {
            id: pat_id_owned(name.to_string()),
            init: Some(init),
            type_annotation: None,
            span: Span::ZERO,
        }],
        span: Span::ZERO,
    }))
}

pub fn function_decl(
    name: &str,
    params: Vec<Pattern>,
    body: Vec<Statement>,
) -> Statement {
    Statement::Function(Box::new(FunctionDeclaration {
        id: Some(Identifier {
            name: Cow::Owned(name.to_string()),
            span: Span::ZERO,
        }),
        params,
        param_type_annotations: Vec::new(),
        body: BlockStatement {
            body,
            span: Span::ZERO,
        },
        generator: false,
        r#async: false,
        span: Span::ZERO,
    }))
}

pub fn export_default_function(
    name: &str,
    params: Vec<Pattern>,
    body: Vec<Statement>,
) -> Statement {
    Statement::ExportDefault(Box::new(ExportDefaultDeclaration {
        declaration: ExportDefault::Function(Box::new(FunctionDeclaration {
            id: Some(Identifier {
                name: Cow::Owned(name.to_string()),
                span: Span::ZERO,
            }),
            params,
            param_type_annotations: Vec::new(),
            body: BlockStatement {
                body,
                span: Span::ZERO,
            },
            generator: false,
            r#async: false,
            span: Span::ZERO,
        })),
        span: Span::ZERO,
    }))
}

/// `import 'source';` — side-effect only.
pub fn import_side_effect(source: &str) -> Statement {
    Statement::Import(Box::new(ImportDeclaration {
        specifiers: Vec::new(),
        source: StringLiteral {
            value: Cow::Owned(source.to_string()),
            raw: None,
            span: Span::ZERO,
        },
        type_only: false,
        span: Span::ZERO,
    }))
}

/// `import * as local from 'source';`
pub fn import_namespace(local: &str, source: &str) -> Statement {
    Statement::Import(Box::new(ImportDeclaration {
        specifiers: vec![ImportSpecifierKind::Namespace(ImportNamespaceSpecifier {
            local: Identifier {
                name: Cow::Owned(local.to_string()),
                span: Span::ZERO,
            },
            span: Span::ZERO,
        })],
        source: StringLiteral {
            value: Cow::Owned(source.to_string()),
            raw: None,
            span: Span::ZERO,
        },
        type_only: false,
        span: Span::ZERO,
    }))
}

/// `import default_name from 'source';`
pub fn import_default(local: &str, source: &str) -> Statement {
    Statement::Import(Box::new(ImportDeclaration {
        specifiers: vec![ImportSpecifierKind::Default(ImportDefaultSpecifier {
            local: Identifier {
                name: Cow::Owned(local.to_string()),
                span: Span::ZERO,
            },
            span: Span::ZERO,
        })],
        source: StringLiteral {
            value: Cow::Owned(source.to_string()),
            raw: None,
            span: Span::ZERO,
        },
        type_only: false,
        span: Span::ZERO,
    }))
}

pub fn return_stmt(arg: Option<Expression>) -> Statement {
    Statement::Return(Box::new(ReturnStatement {
        argument: arg,
        span: Span::ZERO,
    }))
}

pub fn empty_stmt() -> Statement {
    Statement::Empty(Span::ZERO)
}

pub fn this() -> Expression {
    Expression::This(Span::ZERO)
}

pub fn binary(op: BinaryOperator, left: Expression, right: Expression) -> Expression {
    Expression::Binary(Box::new(BinaryExpression {
        left,
        operator: op,
        right,
        span: Span::ZERO,
    }))
}

pub fn logical(op: LogicalOperator, left: Expression, right: Expression) -> Expression {
    Expression::Logical(Box::new(LogicalExpression {
        left,
        operator: op,
        right,
        span: Span::ZERO,
    }))
}

pub fn unary(op: UnaryOperator, argument: Expression) -> Expression {
    Expression::Unary(Box::new(UnaryExpression {
        operator: op,
        argument,
        prefix: true,
        span: Span::ZERO,
    }))
}

pub fn conditional(test: Expression, consequent: Expression, alternate: Expression) -> Expression {
    Expression::Conditional(Box::new(ConditionalExpression {
        test,
        consequent,
        alternate,
        span: Span::ZERO,
    }))
}

pub fn assignment(left: Expression, op: AssignmentOperator, right: Expression) -> Expression {
    Expression::Assignment(Box::new(AssignmentExpression {
        left: AssignmentTarget::Expression(left),
        operator: op,
        right,
        span: Span::ZERO,
    }))
}

pub fn spread(argument: Expression) -> Expression {
    Expression::Spread(Box::new(SpreadElement {
        argument,
        span: Span::ZERO,
    }))
}

pub fn arrow(params: Vec<Pattern>, body: ArrowBody) -> Expression {
    Expression::Arrow(Box::new(ArrowFunctionExpression {
        params,
        param_type_annotations: Vec::new(),
        body,
        r#async: false,
        span: Span::ZERO,
    }))
}

pub fn arrow_block(params: Vec<Pattern>, body: Vec<Statement>) -> Expression {
    arrow(
        params,
        ArrowBody::Block(Box::new(BlockStatement {
            body,
            span: Span::ZERO,
        })),
    )
}

pub fn arrow_expr(params: Vec<Pattern>, body: Expression) -> Expression {
    arrow(params, ArrowBody::Expression(body))
}

pub fn block_stmt(body: Vec<Statement>) -> Statement {
    Statement::Block(Box::new(BlockStatement {
        body,
        span: Span::ZERO,
    }))
}

pub fn if_stmt(test: Expression, consequent: Statement, alternate: Option<Statement>) -> Statement {
    Statement::If(Box::new(IfStatement {
        test,
        consequent,
        alternate,
        span: Span::ZERO,
    }))
}

pub fn program(body: Vec<Statement>) -> Program {
    Program {
        source_type: SourceType::Module,
        body,
        span: Span::ZERO,
    }
}
