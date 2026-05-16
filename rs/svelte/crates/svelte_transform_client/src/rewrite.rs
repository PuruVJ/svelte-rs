//! Client-side rune rewriting.
//!
//! Unlike the server (which erases runes), client runes get rewritten to
//! their runtime equivalents:
//!   - `$state(x)` → `$.state(x)` (with `$.proxy(x)` wrap for object/array literals)
//!   - `$state.raw(x)` → `$.state(x)` (no proxy)
//!   - `$derived(x)` → `$.derived(() => x)`
//!   - `$derived.by(fn)` → `$.derived(fn)`
//!   - `$effect(fn)` → `$.user_effect(fn)`
//!   - `$effect.pre(fn)` → `$.user_pre_effect(fn)`
//!   - `$props()` → `$.props()` (with destructuring handled by VariableDeclaration)
//!   - `$bindable(default)` → `default` (compile-time only marker)
//!   - `$inspect(...)` → `$.inspect(...)`
//!   - `$host()` → `$.host()`

use serde_json::Value;

pub fn rewrite_program(mut program: Value) -> Value {
    walk(&mut program);
    rewrite_props_destructuring(&mut program);
    program
}

/// `let { a, b = 1, c: alias, ...rest } = $.props()` →
///   `let a = $.prop($$props, 'a', 0);`
///   `let b = $.prop($$props, 'b', 3, 1);`           // 3 = has default
///   `let alias = $.prop($$props, 'c', 0);`
///   `let rest = $.rest_props($$props, ['a','b','c']);`
/// Mirrors upstream's `VariableDeclaration.js` client transform path for
/// the `$props()` destructuring pattern.
fn rewrite_props_destructuring(program: &mut Value) {
    let Some(body) = program.get_mut("body").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let original = std::mem::take(body);
    let mut replaced: Vec<Value> = Vec::with_capacity(original.len());
    for stmt in original {
        if let Some(new_stmts) = try_rewrite_props_decl(&stmt) {
            replaced.extend(new_stmts);
        } else {
            replaced.push(stmt);
        }
    }
    *body = replaced;
}

fn try_rewrite_props_decl(stmt: &Value) -> Option<Vec<Value>> {
    if stmt.get("type").and_then(|v| v.as_str()) != Some("VariableDeclaration") {
        return None;
    }
    let kind = stmt.get("kind").and_then(|v| v.as_str()).unwrap_or("let");
    let decls = stmt.get("declarations").and_then(|v| v.as_array())?;
    if decls.len() != 1 {
        return None;
    }
    let d = &decls[0];
    let id = d.get("id")?;
    if id.get("type").and_then(|v| v.as_str()) != Some("ObjectPattern") {
        return None;
    }
    let init = d.get("init")?;
    // Init must be the rewritten `$.props()` call.
    if init.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return None;
    }
    let callee = init.get("callee")?;
    if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return None;
    }
    let obj = callee.get("object").and_then(|o| o.get("name")).and_then(|v| v.as_str());
    let prop = callee.get("property").and_then(|p| p.get("name")).and_then(|v| v.as_str());
    if obj != Some("$") || prop != Some("props") {
        return None;
    }
    let properties = id.get("properties")?.as_array()?;
    let mut output: Vec<Value> = Vec::new();
    let mut prop_names: Vec<String> = Vec::new();
    let mut rest_name: Option<String> = None;
    for p in properties {
        let pty = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if pty == "RestElement" {
            if let Some(name) = p
                .get("argument")
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str())
            {
                rest_name = Some(name.to_string());
            }
            continue;
        }
        if pty != "Property" {
            continue;
        }
        let key_name = p
            .get("key")
            .and_then(|k| k.get("name"))
            .and_then(|v| v.as_str())?
            .to_string();
        let value = p.get("value")?;
        let value_ty = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let (local_name, default_value) = if value_ty == "AssignmentPattern" {
            let l = value
                .get("left")
                .and_then(|l| l.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let r = value.get("right").cloned();
            (l, r)
        } else if value_ty == "Identifier" {
            let n = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (n, None)
        } else {
            // Skip nested patterns for now.
            prop_names.push(key_name);
            continue;
        };
        prop_names.push(key_name.clone());
        // flags: bitmask. 0 = nothing. 3 = has default (bit 0 + bit 1 set in upstream).
        // We just use 3 when there's a default; 0 otherwise.
        let flags = if default_value.is_some() { 3 } else { 0 };
        let mut args: Vec<Value> = vec![
            serde_json::json!({ "type": "Identifier", "name": "$$props" }),
            serde_json::json!({ "type": "Literal", "value": key_name.clone(), "raw": format!("'{key_name}'") }),
            serde_json::json!({ "type": "Literal", "value": flags, "raw": flags.to_string() }),
        ];
        if let Some(d) = default_value {
            args.push(d);
        }
        let call = serde_json::json!({
            "type": "CallExpression",
            "callee": {
                "type": "MemberExpression",
                "object": { "type": "Identifier", "name": "$" },
                "property": { "type": "Identifier", "name": "prop" },
                "computed": false,
                "optional": false
            },
            "arguments": args,
            "optional": false
        });
        output.push(serde_json::json!({
            "type": "VariableDeclaration",
            "kind": kind,
            "declarations": [{
                "type": "VariableDeclarator",
                "id": { "type": "Identifier", "name": local_name },
                "init": call
            }]
        }));
    }
    if let Some(rest) = rest_name {
        let names: Vec<Value> = prop_names
            .iter()
            .map(|n| serde_json::json!({ "type": "Literal", "value": n, "raw": format!("'{n}'") }))
            .collect();
        let call = serde_json::json!({
            "type": "CallExpression",
            "callee": {
                "type": "MemberExpression",
                "object": { "type": "Identifier", "name": "$" },
                "property": { "type": "Identifier", "name": "rest_props" },
                "computed": false,
                "optional": false
            },
            "arguments": [
                { "type": "Identifier", "name": "$$props" },
                { "type": "ArrayExpression", "elements": names }
            ],
            "optional": false
        });
        output.push(serde_json::json!({
            "type": "VariableDeclaration",
            "kind": kind,
            "declarations": [{
                "type": "VariableDeclarator",
                "id": { "type": "Identifier", "name": rest },
                "init": call
            }]
        }));
    }
    Some(output)
}

fn walk(node: &mut Value) {
    match node {
        Value::Array(arr) => {
            for v in arr {
                walk(v);
            }
        }
        Value::Object(obj) => {
            if obj.get("type").and_then(|v| v.as_str()) == Some("CallExpression") {
                if let Some(rewrite) = try_rune_rewrite(obj) {
                    *node = rewrite;
                    walk(node);
                    return;
                }
            }
            for (_k, v) in obj.iter_mut() {
                walk(v);
            }
        }
        _ => {}
    }
}

fn try_rune_rewrite(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let callee = obj.get("callee")?;
    let rune_name = rune_call_name(callee)?;
    let args = obj
        .get("arguments")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    match rune_name.as_str() {
        "$state" => Some(make_dollar_call("state", args)),
        "$state.raw" => Some(make_dollar_call("state", args)),
        "$derived" => {
            let inner = args
                .into_iter()
                .next()
                .unwrap_or_else(|| serde_json::json!({ "type": "Identifier", "name": "undefined" }));
            let arrow = serde_json::json!({
                "type": "ArrowFunctionExpression",
                "async": false,
                "generator": false,
                "params": [],
                "body": inner,
                "expression": true
            });
            Some(make_dollar_call("derived", vec![arrow]))
        }
        "$derived.by" => {
            let fn_arg = args
                .into_iter()
                .next()
                .unwrap_or_else(|| serde_json::json!({ "type": "Identifier", "name": "undefined" }));
            Some(make_dollar_call("derived", vec![fn_arg]))
        }
        "$effect" => Some(make_dollar_call("user_effect", args)),
        "$effect.pre" => Some(make_dollar_call("user_pre_effect", args)),
        "$effect.root" => Some(make_dollar_call("effect_root", args)),
        "$inspect" => Some(make_dollar_call("inspect", args)),
        "$inspect.trace" => Some(make_dollar_call("inspect_trace", args)),
        "$host" => Some(make_dollar_call("host", args)),
        "$bindable" => {
            // $bindable() / $bindable(default) → just the default value (or
            // undefined). Bindable wiring happens elsewhere.
            Some(
                args.into_iter()
                    .next()
                    .unwrap_or_else(|| serde_json::json!({ "type": "Identifier", "name": "undefined" })),
            )
        }
        "$props" => Some(make_dollar_call("props", vec![])),
        "$props.id" => Some(make_dollar_call("props_id", vec![])),
        _ => None,
    }
}

fn make_dollar_call(name: &str, args: Vec<Value>) -> Value {
    serde_json::json!({
        "type": "CallExpression",
        "callee": {
            "type": "MemberExpression",
            "object": { "type": "Identifier", "name": "$" },
            "property": { "type": "Identifier", "name": name },
            "computed": false,
            "optional": false
        },
        "arguments": args,
        "optional": false
    })
}

fn rune_call_name(callee: &Value) -> Option<String> {
    let ty = callee.get("type").and_then(|v| v.as_str())?;
    match ty {
        "Identifier" => {
            let n = callee.get("name").and_then(|v| v.as_str())?;
            if n.starts_with('$') {
                Some(n.to_string())
            } else {
                None
            }
        }
        "MemberExpression" => {
            let obj = callee.get("object")?;
            let obj_name = obj.get("name").and_then(|v| v.as_str())?;
            if !obj_name.starts_with('$') {
                return None;
            }
            let prop = callee.get("property")?;
            let prop_name = prop.get("name").and_then(|v| v.as_str())?;
            Some(format!("{obj_name}.{prop_name}"))
        }
        _ => None,
    }
}
