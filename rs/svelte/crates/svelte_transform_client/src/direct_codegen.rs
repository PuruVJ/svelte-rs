//! Direct string emission for client output — bypasses `print_typed` for known shapes.

use svelte_ast::root::Root;
use svelte_js_ast::*;
use svelte_transform_shared::compile_bump::{BumpString, CompileBump};

use crate::typed_fast;

/// Try emitting fully-static client JS from the Svelte AST (no `Program`, no `print_typed`).
pub fn try_emit_fully_static_client_js(
    root: &Root,
    component_name: &str,
    bump: &CompileBump,
) -> Option<String> {
    if !is_fully_static_root(root) {
        return None;
    }
    let (inner_var, html, top_count) = typed_fast::static_root_info(&root.fragment)?;
    let mut out = bump.string();
    emit_static_client_prelude(&mut out);
    emit_from_html_var(&mut out, &html, top_count > 1);
    out.push_str("\nexport default function ");
    out.push_str(component_name);
    out.push_str("($$anchor) {\n");
    out.push_str("\tvar ");
    out.push_str(&inner_var);
    out.push_str(" = root();\n");
    if top_count > 1 {
        let offset = 2 * (top_count - 1);
        out.push_str("\t$.next(");
        write_usize(&mut out, offset);
        out.push_str(");\n");
    }
    out.push_str("\t$.append($$anchor, ");
    out.push_str(&inner_var);
    out.push_str(");\n}\n");
    Some(out.into_owned())
}

/// Try emitting client JS directly from a typed `Program` (sparse / static slab shapes).
pub fn try_emit_client_program_direct(program: &Program) -> Option<String> {
    if !program_direct_printable(program) {
        return None;
    }
    let mut out = String::with_capacity(estimate_program_js_len(program));
    emit_program_direct(program, &mut out)?;
    Some(out)
}

pub fn is_fully_static_root(root: &Root) -> bool {
    root.instance.is_none() && root.module.is_none() && root.css.is_none()
}

fn emit_static_client_prelude(out: &mut BumpString<'_>) {
    out.push_str("import 'svelte/internal/disclose-version';\n");
    out.push_str("import 'svelte/internal/flags/legacy';\n");
    out.push_str("import * as $ from 'svelte/internal/client';\n\n");
}

fn emit_from_html_var(out: &mut impl StringSink, html: &str, multi_root: bool) {
    out.push_str("var root = $.from_html(`");
    for ch in html.chars() {
        match ch {
            '`' => out.push_str("\\`"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('`');
    if multi_root {
        out.push_str(", 1");
    }
    out.push_str(");\n");
}

trait StringSink {
    fn push_str(&mut self, s: &str);
    fn push(&mut self, c: char);
}

impl StringSink for BumpString<'_> {
    fn push_str(&mut self, s: &str) {
        BumpString::push_str(self, s);
    }
    fn push(&mut self, c: char) {
        BumpString::push(self, c);
    }
}

impl StringSink for String {
    fn push_str(&mut self, s: &str) {
        self.push_str(s);
    }
    fn push(&mut self, c: char) {
        self.push(c);
    }
}

fn write_usize(out: &mut impl StringSink, n: usize) {
    out.push_str(&n.to_string());
}

// --- Program direct printer -------------------------------------------------

fn estimate_program_js_len(program: &Program) -> usize {
    program.body.len().saturating_mul(80) + 256
}

fn program_direct_printable(program: &Program) -> bool {
    let mut saw_export = false;
    for stmt in &program.body {
        match stmt {
            Statement::Import(_) | Statement::Variable(_) => {}
            Statement::ExportDefault(d) => {
                if saw_export {
                    return false;
                }
                saw_export = true;
                if !export_default_direct_printable(d) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    saw_export
}

fn export_default_direct_printable(d: &ExportDefaultDeclaration) -> bool {
    let ExportDefault::Function(f) = &d.declaration else {
        return false;
    };
    function_body_direct_printable(&f.body)
}

fn function_body_direct_printable(block: &BlockStatement) -> bool {
    block
        .body
        .iter()
        .all(|s| function_stmt_direct_printable(s))
}

fn function_stmt_direct_printable(s: &Statement) -> bool {
    match s {
        Statement::Variable(d) => {
            d.kind == VariableKind::Var
                && d.declarations.len() == 1
                && d.declarations[0].init.is_some()
        }
        Statement::Expression(e) => expr_stmt_direct_printable(&e.expression),
        Statement::Return(r) => r
            .argument
            .as_ref()
            .map(|a| expr_direct_printable(a))
            .unwrap_or(true),
        Statement::If(i) => {
            // `{ consequent }` blocks from multi-if emit
            i.alternate.is_none()
                && matches!(&i.test, Expression::Identifier(_))
                && matches!(&i.consequent, Statement::Block(b) if b.body.iter().all(function_stmt_direct_printable))
        }
        Statement::Empty(_) => true,
        _ => false,
    }
}

fn expr_stmt_direct_printable(e: &Expression) -> bool {
    match e {
        Expression::Call(_) => expr_direct_printable(e),
        Expression::Assignment(a) => {
            matches!(
                &a.left,
                AssignmentTarget::Expression(Expression::Member(_))
            ) && expr_direct_printable(&a.right)
        }
        _ => false,
    }
}

fn expr_direct_printable(e: &Expression) -> bool {
    match e {
        Expression::Identifier(_)
        | Expression::Literal(_)
        | Expression::Member(_)
        | Expression::Call(_)
        | Expression::Unary(_)
        | Expression::Binary(_)
        | Expression::Logical(_)
        | Expression::Assignment(_)
        | Expression::Sequence(_)
        | Expression::Arrow(_)
        | Expression::Object(_)
        | Expression::Array(_) => true,
        Expression::Template(_) => true,
        _ => false,
    }
}

fn emit_program_direct(program: &Program, out: &mut String) -> Option<()> {
    let mut export: Option<&ExportDefaultDeclaration> = None;
    let mut import_count = 0usize;
    let mut var_count = 0usize;

    for stmt in &program.body {
        match stmt {
            Statement::Import(_) => import_count += 1,
            Statement::Variable(_) => var_count += 1,
            Statement::ExportDefault(e) => export = Some(e),
            _ => return None,
        }
    }
    let export = export?;

    for stmt in &program.body {
        if let Statement::Import(i) = stmt {
            emit_import_direct(i, out);
            out.push('\n');
        }
    }
    if import_count > 0 {
        out.push('\n');
    }
    for stmt in &program.body {
        if let Statement::Variable(v) = stmt {
            emit_var_decl_direct(v, out, false);
            out.push('\n');
        }
    }
    if var_count > 0 {
        out.push('\n');
    }

    let ExportDefault::Function(f) = &export.declaration else {
        return None;
    };
    out.push_str("export default function ");
    out.push_str(f.id.as_ref()?.name.as_ref());
    out.push('(');
    emit_params_direct(&f.params, out);
    out.push_str(") {\n");
    for stmt in &f.body.body {
        emit_function_stmt_direct(stmt, out, 1)?;
    }
    out.push_str("}\n");
    Some(())
}

fn emit_import_direct(imp: &ImportDeclaration, out: &mut String) {
    if imp.specifiers.is_empty() {
        out.push_str("import '");
        out.push_str(&imp.source.value);
        out.push_str("';");
        return;
    }
    for spec in &imp.specifiers {
        match spec {
            ImportSpecifierKind::Namespace(s) => {
                out.push_str("import * as ");
                out.push_str(s.local.name.as_ref());
                out.push_str(" from '");
                out.push_str(imp.source.value.as_ref());
                out.push_str("';");
                return;
            }
            ImportSpecifierKind::Default(s) => {
                out.push_str("import ");
                out.push_str(s.local.name.as_ref());
                out.push_str(" from '");
                out.push_str(imp.source.value.as_ref());
                out.push_str("';");
                return;
            }
            ImportSpecifierKind::Named(s) => {
                out.push_str("import ");
                match &s.imported {
                    ModuleExportName::Identifier(id) => {
                        out.push_str(id.name.as_ref());
                        if id.name != s.local.name {
                            out.push_str(" as ");
                            out.push_str(s.local.name.as_ref());
                        }
                    }
                    ModuleExportName::String(lit) => {
                        out.push_str(lit.value.as_ref());
                        out.push_str(" as ");
                        out.push_str(s.local.name.as_ref());
                    }
                }
                out.push_str(" from '");
                out.push_str(imp.source.value.as_ref());
                out.push_str("';");
                return;
            }
        }
    }
}

fn emit_params_direct(params: &[Pattern], out: &mut String) {
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        emit_pattern_direct(p, out);
    }
}

fn emit_pattern_direct(p: &Pattern, out: &mut String) {
    match p {
        Pattern::Identifier(i) => out.push_str(i.name.as_ref()),
        _ => out.push_str("/*unsupported*/"),
    }
}

fn emit_function_stmt_direct(stmt: &Statement, out: &mut String, depth: usize) -> Option<()> {
    let indent = "\t".repeat(depth);
    match stmt {
        Statement::Variable(d) => {
            out.push_str(&indent);
            emit_var_decl_direct(d, out, true);
            out.push_str(";\n");
            Some(())
        }
        Statement::Expression(e) => {
            out.push_str(&indent);
            emit_expression_direct(&e.expression, out)?;
            out.push_str(";\n");
            Some(())
        }
        Statement::Return(r) => {
            out.push_str(&indent);
            out.push_str("return");
            if let Some(a) = &r.argument {
                out.push(' ');
                emit_expression_direct(a, out)?;
            }
            out.push_str(";\n");
            Some(())
        }
        Statement::If(i) => {
            out.push_str(&indent);
            out.push_str("{\n");
            emit_function_stmt_direct(&i.consequent, out, depth + 1)?;
            out.push_str(&indent);
            out.push_str("}\n");
            Some(())
        }
        Statement::Empty(_) => Some(()),
        _ => None,
    }
}

fn emit_var_decl_direct(v: &VariableDeclaration, out: &mut String, inline: bool) {
    out.push_str(match v.kind {
        VariableKind::Var => "var ",
        VariableKind::Let => "let ",
        VariableKind::Const => "const ",
    });
    for (i, d) in v.declarations.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        emit_pattern_direct(&d.id, out);
        if let Some(init) = &d.init {
            out.push_str(" = ");
            let _ = emit_expression_direct(init, out);
        }
    }
    if !inline {
        out.push(';');
    }
}

fn emit_expression_direct(e: &Expression, out: &mut String) -> Option<()> {
    match e {
        Expression::Identifier(i) => {
            out.push_str(i.name.as_ref());
            Some(())
        }
        Expression::Literal(l) => {
            emit_literal_direct(l, out);
            Some(())
        }
        Expression::Member(m) => {
            emit_expression_direct(&m.object, out)?;
            if m.computed {
                out.push('[');
                match &m.property {
                    MemberProperty::Expression(e) => emit_expression_direct(e, out)?,
                    MemberProperty::Identifier(i) => out.push_str(i.name.as_ref()),
                    MemberProperty::Private(p) => {
                        out.push_str(&p.name);
                    }
                }
                out.push(']');
            } else {
                out.push('.');
                match &m.property {
                    MemberProperty::Identifier(i) => out.push_str(i.name.as_ref()),
                    MemberProperty::Private(p) => out.push_str(&p.name),
                    MemberProperty::Expression(e) => emit_expression_direct(e, out)?,
                }
            }
            Some(())
        }
        Expression::Call(c) => {
            emit_expression_direct(&c.callee, out)?;
            out.push('(');
            for (i, arg) in c.arguments.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_argument_direct(arg, out)?;
            }
            out.push(')');
            Some(())
        }
        Expression::Unary(u) => {
            out.push_str(u.operator.as_str());
            if matches!(
                u.operator,
                UnaryOperator::Void | UnaryOperator::TypeOf | UnaryOperator::Delete
            ) {
                out.push(' ');
            }
            emit_expression_direct(&u.argument, out)?;
            Some(())
        }
        Expression::Binary(b) => {
            emit_expression_direct(&b.left, out)?;
            out.push(' ');
            out.push_str(b.operator.as_str());
            out.push(' ');
            emit_expression_direct(&b.right, out)?;
            Some(())
        }
        Expression::Logical(l) => {
            emit_expression_direct(&l.left, out)?;
            out.push(' ');
            out.push_str(l.operator.as_str());
            out.push(' ');
            emit_expression_direct(&l.right, out)?;
            Some(())
        }
        Expression::Assignment(a) => {
            emit_assignment_target_direct(&a.left, out)?;
            out.push(' ');
            out.push_str(a.operator.as_str());
            out.push(' ');
            emit_expression_direct(&a.right, out)?;
            Some(())
        }
        Expression::Sequence(s) => {
            for (i, e) in s.expressions.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_expression_direct(e, out)?;
            }
            Some(())
        }
        Expression::Arrow(a) => {
            if a.params.len() == 1 && matches!(a.body, ArrowBody::Expression(_)) {
                emit_pattern_direct(&a.params[0], out);
            } else if a.params.is_empty() {
                out.push_str("()");
            } else {
                out.push('(');
                emit_params_direct(&a.params, out);
                out.push(')');
            }
            out.push_str(" => ");
            match &a.body {
                ArrowBody::Expression(e) => emit_expression_direct(e, out)?,
                ArrowBody::Block(b) => {
                    out.push_str("{\n");
                    for stmt in &b.body {
                        emit_function_stmt_direct(stmt, out, 2)?;
                    }
                    out.push_str("\t}");
                }
            }
            Some(())
        }
        Expression::Object(o) => {
            out.push('{');
            for (i, m) in o.properties.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match m {
                    ObjectMember::Property(p) => {
                        if let PropertyKey::Identifier(k) = &p.key {
                            out.push_str(k.name.as_ref());
                            out.push_str(": ");
                        }
                        emit_expression_direct(&p.value, out)?;
                    }
                    ObjectMember::Spread(s) => {
                        out.push_str("...");
                        emit_expression_direct(&s.argument, out)?;
                    }
                }
            }
            out.push('}');
            Some(())
        }
        Expression::Array(a) => {
            out.push('[');
            for (i, el) in a.elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if let ArrayElement::Expression(e) = el {
                    emit_expression_direct(e, out)?;
                }
            }
            out.push(']');
            Some(())
        }
        Expression::Template(t) => {
            out.push('`');
            for (i, q) in t.quasis.iter().enumerate() {
                out.push_str(&q.raw);
                if !q.tail {
                    out.push_str("${");
                    if let Some(e) = t.expressions.get(i) {
                        emit_expression_direct(e, out)?;
                    }
                    out.push('}');
                }
            }
            out.push('`');
            Some(())
        }
        _ => None,
    }
}

fn emit_argument_direct(arg: &Argument, out: &mut String) -> Option<()> {
    match arg {
        Argument::Expression(e) => emit_expression_direct(e, out),
        Argument::Spread(s) => {
            out.push_str("...");
            emit_expression_direct(&s.argument, out)
        }
    }
}

fn emit_assignment_target_direct(t: &AssignmentTarget, out: &mut String) -> Option<()> {
    match t {
        AssignmentTarget::Pattern(p) => {
            emit_pattern_direct(p, out);
            Some(())
        }
        AssignmentTarget::Expression(e) => emit_expression_direct(e, out),
    }
}

fn emit_literal_direct(l: &Literal, out: &mut String) {
    match l {
        Literal::String(s) => {
            out.push('\'');
            for c in s.value.chars() {
                match c {
                    '\'' => out.push_str("\\'"),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    _ => out.push(c),
                }
            }
            out.push('\'');
        }
        Literal::Number(n) => {
            if (n.value - n.value.round()).abs() < f64::EPSILON {
                write_usize(out, n.value as usize);
            } else {
                out.push_str(&n.value.to_string());
            }
        }
        Literal::Boolean(b) => {
            if b.value {
                out.push_str("true");
            } else {
                out.push_str("false");
            }
        }
        Literal::Null(_) => out.push_str("null"),
        Literal::Regex(r) => {
            out.push('/');
            out.push_str(&r.pattern);
            out.push('/');
            out.push_str(&r.flags);
        }
        Literal::BigInt(b) => {
            out.push_str(&b.raw);
        }
    }
}
