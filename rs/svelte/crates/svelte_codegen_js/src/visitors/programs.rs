//! Program visitor + default visitor table assembly.

use serde_json::Value;
use std::collections::HashMap;

use crate::context::{Context, VisitorTable};
use crate::visitors::{classes, declarations, expressions, identifiers, literals, modules,
    patterns, statements, typescript};

pub fn program(node: &Value, ctx: &mut Context) {
    emit_body(node, ctx);
}

/// Port of `function sequence(context, nodes, until, pad, separator = ',')`
/// from `esrap/src/languages/ts/index.js:251-321`.
///
/// Used by everything that emits a comma-separated list (call arguments,
/// function params, array elements, object properties, ...). The function
/// measures total content length and decides between single-line / multi-line
/// layout: any child being multiline OR total length > 60 triggers multi-line.
///
/// `pad` adds a leading + trailing space when the result is single-line and
/// non-empty (matches upstream's `{ a, b }` vs `(a, b)` distinction — `{`
/// objects pad, `(` calls do not).
pub fn sequence(
    ctx: &mut Context,
    nodes: &[Value],
    pad: bool,
) {
    sequence_with_separator(ctx, nodes, pad, ",")
}

/// Like [`sequence`] but with a customizable separator (`;` for `for(;;)`
/// init/test/update, etc.).
pub fn sequence_with_separator(
    ctx: &mut Context,
    nodes: &[Value],
    pad: bool,
    separator: &str,
) {
    if nodes.is_empty() {
        return;
    }

    let mut multiline_total = false;
    let mut length: i32 = -1;
    let mut multiline_nodes: Vec<bool> = Vec::with_capacity(nodes.len());
    let mut children: Vec<Context> = Vec::with_capacity(nodes.len());

    for (i, child) in nodes.iter().enumerate() {
        let mut child_ctx = ctx.fresh();
        if !child.is_null() {
            child_ctx.visit(child);
        }
        multiline_nodes.push(child_ctx.is_multiline());
        if i < nodes.len() - 1 || child.is_null() {
            child_ctx.write(separator, None);
        }
        // flush_trailing_comments after each child — port of `ts/index.js:268-269`.
        let child_end = child.get("loc").and_then(|l| l.get("end")).and_then(|p| {
            Some((
                p.get("line")?.as_u64()? as u32,
                p.get("column")?.as_u64()? as u32,
            ))
        });
        let next_start = nodes.get(i + 1).and_then(|n| {
            n.get("loc").and_then(|l| l.get("start")).and_then(|p| {
                Some((
                    p.get("line")?.as_u64()? as u32,
                    p.get("column")?.as_u64()? as u32,
                ))
            })
        });
        crate::comments::flush_trailing_comments(&mut child_ctx, child_end, next_start);
        length += (child_ctx.measure() as i32) + 1;
        if child_ctx.is_multiline() {
            multiline_total = true;
        }
        children.push(child_ctx);
    }

    if length > 60 {
        multiline_total = true;
    }

    if multiline_total {
        ctx.indent();
        ctx.newline();
    } else if pad && length > 0 {
        ctx.write(" ", None);
    }

    let mut prev_was_some = false;
    for (i, child_ctx) in children.into_iter().enumerate() {
        if prev_was_some {
            // upstream's "object/array siblings: don't double-space" rule
            if i > 0
                && multiline_nodes.get(i - 1).copied().unwrap_or(false)
                && multiline_nodes.get(i).copied().unwrap_or(false)
            {
                let prev_is_obj = has_object_or_array_value(&nodes[i - 1]);
                let curr_is_obj = has_object_or_array_value(&nodes[i]);
                if !prev_is_obj || !curr_is_obj {
                    ctx.margin();
                }
            }
            if !nodes[i].is_null() {
                if multiline_total {
                    ctx.newline();
                } else {
                    ctx.write(" ", None);
                }
            }
        }
        ctx.append(child_ctx);
        prev_was_some = true;
    }

    if multiline_total {
        ctx.dedent();
        ctx.newline();
    } else if pad && length > 0 {
        ctx.write(" ", None);
    }
}

/// Port of `has_object_or_array_value` — `ts/index.js:239-243`.
fn has_object_or_array_value(node: &Value) -> bool {
    if !matches!(node.get("type").and_then(|v| v.as_str()), Some("Property")) {
        return false;
    }
    let value = match node.get("value") {
        Some(v) if v.get("type").and_then(|x| x.as_str()) == Some("AssignmentPattern") => &v["left"],
        Some(v) => v,
        None => return false,
    };
    matches!(
        value.get("type").and_then(|v| v.as_str()),
        Some("ObjectExpression") | Some("ArrayExpression")
    )
}

/// Port of `function body(context, node)` from `esrap/src/languages/ts/index.js:329-372`.
/// Walks `node.body[]`, emits each statement into a fresh child context, and
/// inserts a `margin` between adjacent statements when either is multi-line or
/// the types differ — which is what makes import groups stay packed but creates
/// a blank line between imports and the first declaration.
pub(crate) fn emit_body(node: &Value, ctx: &mut Context) {
    use crate::comments::{flush_comments_until, flush_trailing_comments, reset_comment_index};

    let body = node
        .get("body")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    reset_comment_index(node, ctx);

    let mut prev_type: Option<String> = None;
    let mut prev_multiline = false;

    // Drain any comments that appear before the first statement.
    if let Some(first) = body.first() {
        let first_start = first.get("loc").and_then(|l| l.get("start")).and_then(|p| {
            Some((
                p.get("line")?.as_u64()? as u32,
                p.get("column")?.as_u64()? as u32,
            ))
        });
        flush_comments_until(ctx, None, first_start, false);
    }

    for (i, stmt) in body.iter().enumerate() {
        let ty = stmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if ty == "EmptyStatement" {
            continue;
        }
        let mut child = ctx.fresh();
        child.visit(stmt);

        if prev_type.is_some() {
            let prev = prev_type.as_deref().unwrap();
            if child.is_multiline() || prev_multiline || prev != ty {
                ctx.margin();
            }
            ctx.newline();
        }

        prev_multiline = child.is_multiline();
        prev_type = Some(ty.to_string());
        ctx.append(child);

        // mirror upstream's flush_trailing_comments after each statement
        let prev_end = stmt
            .get("loc")
            .and_then(|l| l.get("end"))
            .and_then(|p| {
                Some((
                    p.get("line")?.as_u64()? as u32,
                    p.get("column")?.as_u64()? as u32,
                ))
            });
        let next_end = body.get(i + 1).or(Some(node)).and_then(|n| {
            n.get("loc")
                .and_then(|l| l.get("end"))
                .and_then(|p| {
                    Some((
                        p.get("line")?.as_u64()? as u32,
                        p.get("column")?.as_u64()? as u32,
                    ))
                })
        });
        flush_trailing_comments(ctx, prev_end, next_end);
    }
}


/// Build the default visitor table — the equivalent of esrap's
/// `default export` in `languages/ts/index.js`.
pub fn default_visitors() -> VisitorTable {
    let mut t: VisitorTable = HashMap::new();

    // top-level
    t.insert("Program", program);

    // identifiers / refs
    t.insert("Identifier", identifiers::identifier);
    t.insert("PrivateIdentifier", identifiers::private_identifier);
    t.insert("ThisExpression", identifiers::this_expression);
    t.insert("Super", identifiers::super_expression);

    // literals
    t.insert("Literal", literals::literal);
    t.insert("TemplateLiteral", literals::template_literal);

    // expressions
    t.insert("BinaryExpression", expressions::binary_expression);
    t.insert("LogicalExpression", expressions::logical_expression);
    t.insert("UnaryExpression", expressions::unary_expression);
    t.insert("UpdateExpression", expressions::update_expression);
    t.insert("AssignmentExpression", expressions::assignment_expression);
    t.insert("ConditionalExpression", expressions::conditional_expression);
    t.insert("SequenceExpression", expressions::sequence_expression);
    t.insert("MemberExpression", expressions::member_expression);
    t.insert("ChainExpression", expressions::chain_expression);
    t.insert("CallExpression", expressions::call_expression);
    t.insert("NewExpression", expressions::new_expression);
    t.insert("ArrayExpression", expressions::array_expression);
    t.insert("ObjectExpression", expressions::object_expression);
    t.insert("Property", expressions::property);
    t.insert("SpreadElement", expressions::spread_element);
    t.insert("ArrowFunctionExpression", expressions::arrow_function_expression);
    t.insert("FunctionExpression", expressions::function_expression);
    t.insert("AwaitExpression", expressions::await_expression);
    t.insert("YieldExpression", expressions::yield_expression);
    t.insert("ImportExpression", expressions::import_expression);
    t.insert("TaggedTemplateExpression", expressions::tagged_template_expression);
    t.insert("MetaProperty", expressions::meta_property);

    // statements
    t.insert("ExpressionStatement", statements::expression_statement);
    t.insert("BlockStatement", statements::block_statement);
    t.insert("EmptyStatement", statements::empty_statement);
    t.insert("DebuggerStatement", statements::debugger_statement);
    t.insert("ReturnStatement", statements::return_statement);
    t.insert("BreakStatement", statements::break_statement);
    t.insert("ContinueStatement", statements::continue_statement);
    t.insert("ThrowStatement", statements::throw_statement);
    t.insert("IfStatement", statements::if_statement);
    t.insert("WhileStatement", statements::while_statement);
    t.insert("DoWhileStatement", statements::do_while_statement);
    t.insert("ForStatement", statements::for_statement);
    t.insert("ForInStatement", statements::for_in_statement);
    t.insert("ForOfStatement", statements::for_of_statement);
    t.insert("TryStatement", statements::try_statement);
    t.insert("CatchClause", statements::catch_clause);
    t.insert("SwitchStatement", statements::switch_statement);
    t.insert("SwitchCase", statements::switch_case);
    t.insert("LabeledStatement", statements::labeled_statement);
    t.insert("WithStatement", statements::with_statement);

    // declarations
    t.insert("VariableDeclaration", declarations::variable_declaration);
    t.insert("VariableDeclarator", declarations::variable_declarator);
    t.insert("FunctionDeclaration", declarations::function_declaration);

    // patterns
    t.insert("ArrayPattern", patterns::array_pattern);
    t.insert("ObjectPattern", patterns::object_pattern);
    t.insert("RestElement", patterns::rest_element);
    t.insert("AssignmentPattern", patterns::assignment_pattern);

    // classes
    t.insert("ClassDeclaration", classes::class_declaration);
    t.insert("ClassExpression", classes::class_expression);
    t.insert("ClassBody", classes::class_body);
    t.insert("MethodDefinition", classes::method_definition);
    t.insert("PropertyDefinition", classes::property_definition);
    t.insert("StaticBlock", classes::static_block);

    // modules
    t.insert("ImportDeclaration", modules::import_declaration);
    t.insert("ImportSpecifier", modules::import_specifier);
    t.insert("ExportNamedDeclaration", modules::export_named_declaration);
    t.insert("ExportDefaultDeclaration", modules::export_default_declaration);
    t.insert("ExportAllDeclaration", modules::export_all_declaration);

    // typescript (mostly stripped)
    t.insert("TSAsExpression", typescript::ts_as_expression);
    t.insert("TSSatisfiesExpression", typescript::ts_satisfies_expression);
    t.insert("TSNonNullExpression", typescript::ts_non_null_expression);
    t.insert("TSTypeAssertion", typescript::ts_type_assertion);
    t.insert("TSInstantiationExpression", typescript::ts_instantiation_expression);

    t
}
