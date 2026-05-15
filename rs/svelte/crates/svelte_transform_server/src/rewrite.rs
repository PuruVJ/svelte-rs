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
        // $state(x) / $state.raw(x) / $derived(x) / $derived.by(x) → x
        // $state() / $derived() → undefined
        "$state" | "$state.raw" | "$derived" | "$derived.by" => Some(
            args.into_iter()
                .next()
                .unwrap_or_else(|| serde_json::json!({ "type": "Identifier", "name": "undefined" })),
        ),
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
                            "callee": { "type": "Identifier", "name": "$derived" },
                            "arguments": [{ "type": "Identifier", "name": "y" }]
                        },
                        "right": { "type": "Literal", "raw": "1", "value": 1 }
                    }
                }]
            }]
        });
        let rewritten = rewrite_program(prog);
        let init = &rewritten["body"][0]["declarations"][0]["init"];
        // $derived(y) becomes y, so init becomes `y + 1` — left should be the Identifier `y`.
        assert_eq!(init["left"]["type"], "Identifier");
        assert_eq!(init["left"]["name"], "y");
    }
}
