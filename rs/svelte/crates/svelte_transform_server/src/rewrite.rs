//! JS rewriting passes applied to the parsed `<script>` body before it goes
//! into the output Program.
//!
//! Currently:
//! - **Runes erasure** (server-side): `$state(x)` / `$state.raw(x)` / `$derived(x)` /
//!   `$derived.by(x)` → `x`; `$props()` → an empty placeholder (real handling
//!   needs the analyzer's prop list — TODO); `$effect(fn)`, `$inspect(...)` →
//!   no-op (dropped). Mirrors `phases/3-transform/server/visitors/CallExpression.js`.

use serde_json::Value;

/// Walk a JSON AST in place and erase rune calls. Returns the rewritten value.
pub fn rewrite_program(mut program: Value) -> Value {
    walk(&mut program);
    transform_class_state_fields(&mut program);
    program
}

/// For each `class { name = $.derived(...); ... }` declaration, rename the
/// field to `#name` and insert `get name()` + `set name($$value)` accessors.
/// Mirrors upstream's class-state-field transform pattern.
fn transform_class_state_fields(node: &mut Value) {
    fn class_walk(v: &mut Value) {
        if let Some(obj) = v.as_object_mut() {
            let ty = obj
                .get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if matches!(ty.as_str(), "ClassDeclaration" | "ClassExpression") {
                if let Some(body) = obj.get_mut("body") {
                    if let Some(inner) = body.get_mut("body").and_then(|v| v.as_array_mut()) {
                        let original = std::mem::take(inner);
                        let mut transformed: Vec<Value> = Vec::with_capacity(original.len());
                        for member in original {
                            transform_class_member(&member, &mut transformed);
                        }
                        *inner = transformed;
                    }
                }
            }
            for (_, v) in obj.iter_mut() {
                class_walk(v);
            }
        } else if let Some(arr) = v.as_array_mut() {
            for v in arr {
                class_walk(v);
            }
        }
    }
    class_walk(node);
}

fn transform_class_member(member: &Value, out: &mut Vec<Value>) {
    let ty = member.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if ty != "PropertyDefinition" {
        out.push(member.clone());
        return;
    }
    let value = match member.get("value") {
        Some(v) if !v.is_null() => v,
        _ => {
            out.push(member.clone());
            return;
        }
    };
    if !is_derived_call(value) {
        out.push(member.clone());
        return;
    }
    let key = match member.get("key") {
        Some(k) if k.get("type").and_then(|v| v.as_str()) == Some("Identifier") => k,
        _ => {
            out.push(member.clone());
            return;
        }
    };
    let name = key
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        out.push(member.clone());
        return;
    }
    let private_key = serde_json::json!({ "type": "PrivateIdentifier", "name": name });

    // Renamed field
    let mut renamed = member.clone();
    renamed["key"] = private_key.clone();
    out.push(renamed);

    // Getter
    let getter_body = serde_json::json!({
        "type": "BlockStatement",
        "body": [{
            "type": "ReturnStatement",
            "argument": {
                "type": "CallExpression",
                "callee": {
                    "type": "MemberExpression",
                    "object": { "type": "ThisExpression" },
                    "property": private_key.clone(),
                    "computed": false,
                    "optional": false
                },
                "arguments": [],
                "optional": false
            }
        }]
    });
    out.push(serde_json::json!({
        "type": "MethodDefinition",
        "kind": "get",
        "static": false,
        "computed": false,
        "key": { "type": "Identifier", "name": name },
        "value": {
            "type": "FunctionExpression",
            "async": false,
            "generator": false,
            "id": null,
            "params": [],
            "body": getter_body
        }
    }));

    // Setter
    let setter_body = serde_json::json!({
        "type": "BlockStatement",
        "body": [{
            "type": "ReturnStatement",
            "argument": {
                "type": "CallExpression",
                "callee": {
                    "type": "MemberExpression",
                    "object": { "type": "ThisExpression" },
                    "property": private_key,
                    "computed": false,
                    "optional": false
                },
                "arguments": [{ "type": "Identifier", "name": "$$value" }],
                "optional": false
            }
        }]
    });
    out.push(serde_json::json!({
        "type": "MethodDefinition",
        "kind": "set",
        "static": false,
        "computed": false,
        "key": { "type": "Identifier", "name": name },
        "value": {
            "type": "FunctionExpression",
            "async": false,
            "generator": false,
            "id": null,
            "params": [{ "type": "Identifier", "name": "$$value" }],
            "body": setter_body
        }
    }));
}

/// Walk a JSON AST and return the set of names bound to `$derived(...)` /
/// `$derived.by(...)` initializers. These names need `name()` call sites in
/// template expressions (server-side, the derived getter is a thunk).
pub fn collect_derived_names(program: &Value) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn walk(node: &Value, out: &mut std::collections::HashSet<String>) {
        match node {
            Value::Array(arr) => {
                for v in arr {
                    walk(v, out);
                }
            }
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    if let Some(init) = obj.get("init") {
                        if is_derived_call(init) {
                            if let Some(name) = obj
                                .get("id")
                                .and_then(|i| i.get("name"))
                                .and_then(|v| v.as_str())
                            {
                                out.insert(name.to_string());
                            }
                        }
                    }
                }
                for v in obj.values() {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    walk(program, &mut out);
    out
}

fn is_derived_call(v: &Value) -> bool {
    if v.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return false;
    }
    let callee = match v.get("callee") {
        Some(c) => c,
        None => return false,
    };
    // Detect `$.derived(...)` (post-rewrite).
    if callee.get("type").and_then(|v| v.as_str()) == Some("MemberExpression") {
        let obj = callee
            .get("object")
            .and_then(|o| o.get("name"))
            .and_then(|v| v.as_str());
        let prop = callee
            .get("property")
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str());
        return obj == Some("$") && prop == Some("derived");
    }
    false
}

/// Rewrite an expression so that any Identifier whose name is in `deriveds`
/// becomes `name()`. Used to turn `<p>{count}</p>` where `count` is a
/// derived binding into `<p>${$.escape(count())}</p>`.
pub fn rewrite_derived_refs(expr: &mut Value, deriveds: &std::collections::HashSet<String>) {
    rewrite_derived(expr, deriveds, true);
}

fn rewrite_derived(node: &mut Value, deriveds: &std::collections::HashSet<String>, allow_self_call: bool) {
    let ty = node
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match ty.as_str() {
        "Identifier" => {
            if !allow_self_call {
                return;
            }
            let name = node
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if deriveds.contains(&name) {
                let id = node.clone();
                *node = serde_json::json!({
                    "type": "CallExpression",
                    "callee": id,
                    "arguments": [],
                    "optional": false
                });
            }
        }
        "MemberExpression" => {
            // Walk object — `counter.count` where `counter` is derived should
            // become `counter().count`. But don't recurse into the property
            // (which is itself an Identifier but used as a member name).
            if let Some(obj) = node.get_mut("object") {
                rewrite_derived(obj, deriveds, true);
            }
            let computed = node
                .get("computed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if computed {
                if let Some(prop) = node.get_mut("property") {
                    rewrite_derived(prop, deriveds, true);
                }
            }
        }
        // Don't recurse into function bodies — derived refs inside `$derived(() => x)`
        // are already inside the closure, calling them creates infinite loops.
        "ArrowFunctionExpression" | "FunctionExpression" | "FunctionDeclaration" => {}
        _ => {
            // Generic recursion.
            if let Some(obj) = node.as_object_mut() {
                for (_, v) in obj.iter_mut() {
                    match v {
                        Value::Array(arr) => {
                            for v in arr.iter_mut() {
                                rewrite_derived(v, deriveds, allow_self_call);
                            }
                        }
                        Value::Object(_) => rewrite_derived(v, deriveds, allow_self_call),
                        _ => {}
                    }
                }
            }
        }
    }
}

fn walk(node: &mut Value) {
    match node {
        Value::Array(arr) => {
            for v in arr {
                walk(v);
            }
        }
        Value::Object(obj) => {
            // Rewrite CallExpression nodes before recursing
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

/// If this CallExpression is a rune, return the rewritten value. Otherwise None.
fn try_rune_rewrite(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let callee = obj.get("callee")?;
    let rune_name = rune_call_name(callee)?;
    let args = obj.get("arguments").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    match rune_name.as_str() {
        // $state(x) / $state.raw(x) — server-erased to bare value.
        // $state() / $state.raw() → undefined
        "$state" | "$state.raw" => Some(
            args.into_iter()
                .next()
                .unwrap_or_else(|| serde_json::json!({ "type": "Identifier", "name": "undefined" })),
        ),
        // $derived(x) → $.derived(() => x); $derived.by(fn) → $.derived(fn).
        // Server still needs laziness so the value is computed when read.
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
            Some(serde_json::json!({
                "type": "CallExpression",
                "callee": {
                    "type": "MemberExpression",
                    "object": { "type": "Identifier", "name": "$" },
                    "property": { "type": "Identifier", "name": "derived" },
                    "computed": false,
                    "optional": false
                },
                "arguments": [arrow],
                "optional": false
            }))
        }
        "$derived.by" => {
            let fn_arg = args
                .into_iter()
                .next()
                .unwrap_or_else(|| serde_json::json!({ "type": "Identifier", "name": "undefined" }));
            Some(serde_json::json!({
                "type": "CallExpression",
                "callee": {
                    "type": "MemberExpression",
                    "object": { "type": "Identifier", "name": "$" },
                    "property": { "type": "Identifier", "name": "derived" },
                    "computed": false,
                    "optional": false
                },
                "arguments": [fn_arg],
                "optional": false
            }))
        }
        // $effect / $effect.pre / $inspect — server-side no-op. Replace with `undefined`.
        "$effect" | "$effect.pre" | "$effect.root" | "$inspect" | "$inspect.trace" => Some(
            serde_json::json!({ "type": "Identifier", "name": "undefined" }),
        ),
        // $host → $$payload.context (legacy). Drop for now.
        "$host" => Some(serde_json::json!({ "type": "Identifier", "name": "undefined" })),
        // $bindable(default) → default (or undefined).
        "$bindable" => Some(
            args.into_iter()
                .next()
                .unwrap_or_else(|| serde_json::json!({ "type": "Identifier", "name": "undefined" })),
        ),
        // $props() → $$props (provided as a function parameter on the server).
        // Defaults from the destructuring pattern are preserved at the AST
        // level; the rewriter only swaps the rune call for the identifier.
        "$props" => Some(serde_json::json!({ "type": "Identifier", "name": "$$props" })),
        "$props.id" => Some(serde_json::json!({
            "type": "Identifier",
            "name": "$$props_id"
        })),
        _ => None,
    }
}

/// If `callee` is `$xxx` or `$xxx.yyy`, return the keypath ("$state", "$state.raw").
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_call_erased() {
        let prog = json!({
            "type": "Program",
            "body": [{
                "type": "VariableDeclaration",
                "kind": "let",
                "declarations": [{
                    "type": "VariableDeclarator",
                    "id": { "type": "Identifier", "name": "x" },
                    "init": {
                        "type": "CallExpression",
                        "callee": { "type": "Identifier", "name": "$state" },
                        "arguments": [{ "type": "Literal", "raw": "1", "value": 1 }]
                    }
                }]
            }]
        });
        let rewritten = rewrite_program(prog);
        let init = &rewritten["body"][0]["declarations"][0]["init"];
        assert_eq!(init["type"], "Literal");
        assert_eq!(init["raw"], "1");
    }

    #[test]
    fn effect_call_erased_to_undefined() {
        let prog = json!({
            "type": "Program",
            "body": [{
                "type": "ExpressionStatement",
                "expression": {
                    "type": "CallExpression",
                    "callee": { "type": "Identifier", "name": "$effect" },
                    "arguments": [{
                        "type": "ArrowFunctionExpression",
                        "params": [],
                        "body": { "type": "BlockStatement", "body": [] }
                    }]
                }
            }]
        });
        let rewritten = rewrite_program(prog);
        let expr = &rewritten["body"][0]["expression"];
        assert_eq!(expr["type"], "Identifier");
        assert_eq!(expr["name"], "undefined");
    }

    #[test]
    fn nested_derived_call_wraps_in_arrow() {
        // $derived(y) → $.derived(() => y) — server keeps laziness via thunk.
        let prog = json!({
            "type": "Program",
            "body": [{
                "type": "VariableDeclaration",
                "kind": "let",
                "declarations": [{
                    "type": "VariableDeclarator",
                    "id": { "type": "Identifier", "name": "x" },
                    "init": {
                        "type": "CallExpression",
                        "callee": { "type": "Identifier", "name": "$derived" },
                        "arguments": [{ "type": "Identifier", "name": "y" }]
                    }
                }]
            }]
        });
        let rewritten = rewrite_program(prog);
        let init = &rewritten["body"][0]["declarations"][0]["init"];
        assert_eq!(init["type"], "CallExpression");
        assert_eq!(init["callee"]["property"]["name"], "derived");
        assert_eq!(init["arguments"][0]["type"], "ArrowFunctionExpression");
        assert_eq!(init["arguments"][0]["body"]["type"], "Identifier");
        assert_eq!(init["arguments"][0]["body"]["name"], "y");
    }

    #[test]
    fn nested_state_call_erased() {
        let prog = json!({
            "type": "Program",
            "body": [{
                "type": "VariableDeclaration",
                "kind": "let",
                "declarations": [{
                    "type": "VariableDeclarator",
                    "id": { "type": "Identifier", "name": "x" },
                    "init": {
                        "type": "BinaryExpression",
                        "operator": "+",
                        "left": {
                            "type": "CallExpression",
                            "callee": { "type": "Identifier", "name": "$state" },
                            "arguments": [{ "type": "Identifier", "name": "y" }]
                        },
                        "right": { "type": "Literal", "raw": "1", "value": 1 }
                    }
                }]
            }]
        });
        let rewritten = rewrite_program(prog);
        let init = &rewritten["body"][0]["declarations"][0]["init"];
        // $state(y) → y, so left is the Identifier `y`.
        assert_eq!(init["left"]["type"], "Identifier");
        assert_eq!(init["left"]["name"], "y");
    }
}
