//! AST builders producing acorn-shaped JSON (consumed by `svelte_codegen_js`).
//!
//! Port of `packages/svelte/src/compiler/utils/builders.js`. Each builder
//! mirrors the JS function 1:1 — same parameter order, same node shape.
//! Output is `serde_json::Value` so transforms can compose results from
//! both ported visitors and (later) helpers that emit ad-hoc AST.

use serde_json::{json, Value};

pub fn id(name: &str) -> Value {
    json!({ "type": "Identifier", "name": name })
}

pub fn private_id(name: &str) -> Value {
    json!({ "type": "PrivateIdentifier", "name": name })
}

pub fn literal_str(value: &str) -> Value {
    json!({
        "type": "Literal",
        "value": value,
        "raw": format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
    })
}

pub fn literal_num(value: f64) -> Value {
    let raw = if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    };
    json!({ "type": "Literal", "value": value, "raw": raw })
}

pub fn literal_bool(value: bool) -> Value {
    json!({
        "type": "Literal",
        "value": value,
        "raw": if value { "true" } else { "false" }
    })
}

pub fn literal_null() -> Value {
    json!({ "type": "Literal", "value": null, "raw": "null" })
}

pub fn array(elements: Vec<Value>) -> Value {
    json!({ "type": "ArrayExpression", "elements": elements })
}

pub fn object(properties: Vec<Value>) -> Value {
    json!({ "type": "ObjectExpression", "properties": properties })
}

pub fn init(name: &str, value: Value) -> Value {
    json!({
        "type": "Property",
        "kind": "init",
        "key": id(name),
        "value": value,
        "computed": false,
        "shorthand": false,
        "method": false
    })
}

pub fn prop(kind: &str, key: Value, value: Value, computed: bool) -> Value {
    json!({
        "type": "Property",
        "kind": kind,
        "key": key,
        "value": value,
        "computed": computed,
        "shorthand": false,
        "method": false
    })
}

pub fn block(body: Vec<Value>) -> Value {
    json!({ "type": "BlockStatement", "body": body })
}

pub fn stmt(expression: Value) -> Value {
    json!({ "type": "ExpressionStatement", "expression": expression })
}

pub fn call(callee: Value, args: Vec<Value>) -> Value {
    json!({
        "type": "CallExpression",
        "callee": callee,
        "arguments": args,
        "optional": false
    })
}

pub fn call_id(name: &str, args: Vec<Value>) -> Value {
    call(id(name), args)
}

pub fn new_expr(callee: Value, args: Vec<Value>) -> Value {
    json!({ "type": "NewExpression", "callee": callee, "arguments": args })
}

pub fn binary(op: &str, left: Value, right: Value) -> Value {
    json!({
        "type": "BinaryExpression",
        "operator": op,
        "left": left,
        "right": right
    })
}

pub fn logical(op: &str, left: Value, right: Value) -> Value {
    json!({
        "type": "LogicalExpression",
        "operator": op,
        "left": left,
        "right": right
    })
}

pub fn unary(op: &str, argument: Value) -> Value {
    json!({
        "type": "UnaryExpression",
        "operator": op,
        "argument": argument,
        "prefix": true
    })
}

pub fn update(op: &str, argument: Value, prefix: bool) -> Value {
    json!({
        "type": "UpdateExpression",
        "operator": op,
        "argument": argument,
        "prefix": prefix
    })
}

pub fn assignment(op: &str, left: Value, right: Value) -> Value {
    json!({
        "type": "AssignmentExpression",
        "operator": op,
        "left": left,
        "right": right
    })
}

pub fn conditional(test: Value, consequent: Value, alternate: Value) -> Value {
    json!({
        "type": "ConditionalExpression",
        "test": test,
        "consequent": consequent,
        "alternate": alternate
    })
}

pub fn member(object: Value, property: Value, computed: bool, optional: bool) -> Value {
    json!({
        "type": "MemberExpression",
        "object": object,
        "property": property,
        "computed": computed,
        "optional": optional
    })
}

/// Parse a dotted path like `"a.b.c"` into nested MemberExpressions.
pub fn member_id(path: &str) -> Value {
    let parts: Vec<&str> = path.split('.').collect();
    let mut acc = id(parts[0]);
    for p in &parts[1..] {
        acc = member(acc, id(p), false, false);
    }
    acc
}

pub fn arrow(params: Vec<Value>, body: Value, is_async: bool) -> Value {
    json!({
        "type": "ArrowFunctionExpression",
        "async": is_async,
        "generator": false,
        "params": params,
        "body": body,
        "expression": body.get("type").and_then(|v| v.as_str()) != Some("BlockStatement"),
    })
}

pub fn function(id_opt: Option<Value>, params: Vec<Value>, body: Value, is_async: bool) -> Value {
    json!({
        "type": "FunctionExpression",
        "async": is_async,
        "generator": false,
        "id": id_opt.unwrap_or(Value::Null),
        "params": params,
        "body": body
    })
}

pub fn function_declaration(id_v: Value, params: Vec<Value>, body: Value, is_async: bool) -> Value {
    json!({
        "type": "FunctionDeclaration",
        "async": is_async,
        "generator": false,
        "id": id_v,
        "params": params,
        "body": body
    })
}

pub fn declarator(name: Value, init: Option<Value>) -> Value {
    json!({
        "type": "VariableDeclarator",
        "id": name,
        "init": init.unwrap_or(Value::Null)
    })
}

pub fn declaration(kind: &str, declarators: Vec<Value>) -> Value {
    json!({
        "type": "VariableDeclaration",
        "kind": kind,
        "declarations": declarators
    })
}

pub fn const_decl(name: &str, init: Value) -> Value {
    declaration("const", vec![declarator(id(name), Some(init))])
}

pub fn let_decl(name: &str, init: Option<Value>) -> Value {
    declaration("let", vec![declarator(id(name), init)])
}

pub fn var_decl(name: &str, init: Option<Value>) -> Value {
    declaration("var", vec![declarator(id(name), init)])
}

pub fn export_default(decl: Value) -> Value {
    json!({ "type": "ExportDefaultDeclaration", "declaration": decl })
}

pub fn import_all(local: &str, source: &str) -> Value {
    json!({
        "type": "ImportDeclaration",
        "specifiers": [{
            "type": "ImportNamespaceSpecifier",
            "local": id(local)
        }],
        "source": literal_str(source)
    })
}

/// `imports([['x', 'y'], 'z'], 'src')` → `import { x as y, z } from 'src'`.
/// Each entry is either `[imported, local]` or `"name"`.
pub fn imports(parts: Vec<(&str, &str)>, source: &str) -> Value {
    let specs: Vec<Value> = parts
        .iter()
        .map(|(imp, loc)| {
            json!({
                "type": "ImportSpecifier",
                "imported": id(imp),
                "local": id(loc)
            })
        })
        .collect();
    json!({
        "type": "ImportDeclaration",
        "specifiers": specs,
        "source": literal_str(source)
    })
}

pub fn return_stmt(arg: Option<Value>) -> Value {
    json!({ "type": "ReturnStatement", "argument": arg.unwrap_or(Value::Null) })
}

pub fn empty_stmt() -> Value {
    json!({ "type": "EmptyStatement" })
}

pub fn if_stmt(test: Value, consequent: Value, alternate: Option<Value>) -> Value {
    json!({
        "type": "IfStatement",
        "test": test,
        "consequent": consequent,
        "alternate": alternate.unwrap_or(Value::Null)
    })
}

pub fn template_literal(quasis_raw: Vec<&str>, expressions: Vec<Value>) -> Value {
    let last = quasis_raw.len().saturating_sub(1);
    let quasis: Vec<Value> = quasis_raw
        .iter()
        .enumerate()
        .map(|(i, s)| {
            json!({
                "type": "TemplateElement",
                "value": { "raw": s, "cooked": s },
                "tail": i == last
            })
        })
        .collect();
    json!({
        "type": "TemplateLiteral",
        "quasis": quasis,
        "expressions": expressions
    })
}

pub fn program(body: Vec<Value>) -> Value {
    json!({ "type": "Program", "sourceType": "module", "body": body })
}

pub fn spread(argument: Value) -> Value {
    json!({ "type": "SpreadElement", "argument": argument })
}

pub fn rest(argument: Value) -> Value {
    json!({ "type": "RestElement", "argument": argument })
}

pub fn array_pattern(elements: Vec<Value>) -> Value {
    json!({ "type": "ArrayPattern", "elements": elements })
}

pub fn object_pattern(properties: Vec<Value>) -> Value {
    json!({ "type": "ObjectPattern", "properties": properties })
}

pub fn this_expression() -> Value {
    json!({ "type": "ThisExpression" })
}
