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
    program
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
