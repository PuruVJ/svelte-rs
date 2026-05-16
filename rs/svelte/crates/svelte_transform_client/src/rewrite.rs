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

pub fn rewrite_program(program: Value) -> Value {
    let (rewritten, _) = rewrite_program_with_state_and_hints(program, &Default::default());
    rewritten
}

/// Variant that returns the collected state names alongside the rewritten
/// program, so callers can apply the same state-access rewrite to template
/// expressions.
pub fn rewrite_program_with_state(program: Value) -> (Value, std::collections::HashSet<String>) {
    rewrite_program_with_state_and_hints(program, &Default::default())
}

/// Like `rewrite_program_with_state`, but accepts a set of names known to be
/// reassigned outside the script body (e.g. in template event handlers or
/// expression tags). These names will NOT be eligible for the
/// never-reassigned `$.state(x)` → `x` unwrap optimization.
pub fn rewrite_program_with_state_and_hints(
    mut program: Value,
    extra_reassigned: &std::collections::HashSet<String>,
) -> (Value, std::collections::HashSet<String>) {
    walk(&mut program);
    rewrite_props_destructuring(&mut program);
    transform_class_state_fields(&mut program);
    let state_names = collect_state_names(&program);
    // Detect which `$state(...)` bindings are never reassigned in this script.
    // Upstream's optimization: a `$state` that is never reassigned doesn't need
    // the reactive wrap — unwrap `$.state(initial)` back to `initial`, and
    // skip the `$.get(x)` read rewrite. Only applies to `$.state` (not derived).
    let mut reassigned = collect_reassigned_names(&program, &state_names);
    for n in extra_reassigned {
        if state_names.contains(n) {
            reassigned.insert(n.clone());
        }
    }
    let mut nonreactive_state: std::collections::HashSet<String> = Default::default();
    for n in &state_names {
        if !reassigned.contains(n) {
            // Only $.state bindings can be unwrapped (derived must stay).
            if is_state_call_binding(&program, n) {
                nonreactive_state.insert(n.clone());
            }
        }
    }
    if !nonreactive_state.is_empty() {
        unwrap_state_initializers(&mut program, &nonreactive_state);
    }
    let active_state: std::collections::HashSet<String> = state_names
        .iter()
        .filter(|n| !nonreactive_state.contains(*n))
        .cloned()
        .collect();
    if !active_state.is_empty() {
        rewrite_state_accesses(&mut program, &active_state);
    }
    (program, active_state)
}

/// Names that appear as the LHS of an AssignmentExpression or as the argument
/// of an UpdateExpression (++ / --) anywhere in `program`, restricted to the
/// `candidates` set.
fn collect_reassigned_names(
    program: &Value,
    candidates: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn walk(
        node: &Value,
        candidates: &std::collections::HashSet<String>,
        out: &mut std::collections::HashSet<String>,
    ) {
        match node {
            Value::Array(arr) => {
                for v in arr {
                    walk(v, candidates, out);
                }
            }
            Value::Object(obj) => {
                let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ty {
                    "AssignmentExpression" => {
                        if let Some(left) = obj.get("left") {
                            if left.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                                if let Some(name) = left.get("name").and_then(|v| v.as_str()) {
                                    if candidates.contains(name) {
                                        out.insert(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                    "UpdateExpression" => {
                        if let Some(arg) = obj.get("argument") {
                            if arg.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                                if let Some(name) = arg.get("name").and_then(|v| v.as_str()) {
                                    if candidates.contains(name) {
                                        out.insert(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                for (_, v) in obj.iter() {
                    walk(v, candidates, out);
                }
            }
            _ => {}
        }
    }
    walk(program, candidates, &mut out);
    out
}

/// True if `name` is bound to `$.state(...)` (not `$.derived(...)`) anywhere
/// in `program`.
fn is_state_call_binding(program: &Value, name: &str) -> bool {
    fn walk(node: &Value, name: &str) -> bool {
        match node {
            Value::Array(arr) => arr.iter().any(|v| walk(v, name)),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    let bound_name = obj
                        .get("id")
                        .and_then(|i| i.get("name"))
                        .and_then(|v| v.as_str());
                    if bound_name == Some(name) {
                        if let Some(init) = obj.get("init") {
                            if is_dollar_state_call(init) {
                                return true;
                            }
                        }
                    }
                }
                obj.values().any(|v| walk(v, name))
            }
            _ => false,
        }
    }
    walk(program, name)
}

fn is_dollar_state_call(v: &Value) -> bool {
    if v.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return false;
    }
    let Some(callee) = v.get("callee") else {
        return false;
    };
    if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return false;
    }
    let obj = callee
        .get("object")
        .and_then(|o| o.get("name"))
        .and_then(|v| v.as_str());
    let prop = callee
        .get("property")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str());
    obj == Some("$") && prop == Some("state")
}

/// For each `let X = $.state(initial)` where X is in `names`, replace `init`
/// with `initial`. (The `$.state(...)` wrap is unnecessary because the binding
/// is never reassigned.)
fn unwrap_state_initializers(program: &mut Value, names: &std::collections::HashSet<String>) {
    fn walk(node: &mut Value, names: &std::collections::HashSet<String>) {
        if let Some(obj) = node.as_object_mut() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                let bound = obj
                    .get("id")
                    .and_then(|i| i.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(name) = bound {
                    if names.contains(&name) {
                        if let Some(init) = obj.get_mut("init") {
                            if is_dollar_state_call(init) {
                                let inner = init
                                    .get("arguments")
                                    .and_then(|v| v.as_array())
                                    .and_then(|a| a.first().cloned())
                                    .unwrap_or_else(|| {
                                        serde_json::json!({ "type": "Identifier", "name": "undefined" })
                                    });
                                *init = inner;
                            }
                        }
                    }
                }
            }
            for (_, v) in obj.iter_mut() {
                walk(v, names);
            }
        } else if let Some(arr) = node.as_array_mut() {
            for v in arr {
                walk(v, names);
            }
        }
    }
    walk(program, names);
}

/// Apply state-access rewriting to an arbitrary JSON expression value
/// (e.g. an ExpressionTag's `.expression`). Skips when state names is empty.
pub fn rewrite_expression(expr: &mut Value, state_names: &std::collections::HashSet<String>) {
    if state_names.is_empty() {
        return;
    }
    rewrite_state_accesses(expr, state_names);
}

/// Collect names of variables bound to `$.state(...)` or `$.derived(...)` /
/// `$.derived(() => ...)`. These need read/write rewriting:
/// - Read `x` → `$.get(x)`
/// - Write `x = v` → `$.set(x, v)`
/// - Compound `x += v` → `$.set(x, $.get(x) + v)`
fn collect_state_names(program: &Value) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn walk(node: &Value, out: &mut std::collections::HashSet<String>) {
        match node {
            Value::Array(arr) => arr.iter().for_each(|v| walk(v, out)),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    if let Some(init) = obj.get("init") {
                        if is_state_or_derived_call(init) {
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
                for (_, v) in obj.iter() {
                    walk(v, out);
                }
            }
            _ => {}
        }
    }
    walk(program, &mut out);
    out
}

fn is_state_or_derived_call(v: &Value) -> bool {
    if v.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return false;
    }
    let Some(callee) = v.get("callee") else {
        return false;
    };
    if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return false;
    }
    let obj = callee
        .get("object")
        .and_then(|o| o.get("name"))
        .and_then(|v| v.as_str());
    let prop = callee
        .get("property")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str());
    obj == Some("$") && matches!(prop, Some("state") | Some("derived"))
}

/// Walk the program rewriting Identifier reads and assignments to state.
/// Skips the binding's own declaration site (the `let x = $.state(...)` row).
fn rewrite_state_accesses(node: &mut Value, names: &std::collections::HashSet<String>) {
    rewrite_walk(node, names, false);
}

fn rewrite_walk(node: &mut Value, names: &std::collections::HashSet<String>, is_member_property: bool) {
    let ty = node
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match ty.as_str() {
        "VariableDeclarator" => {
            // Skip the `id` (it's the declaration itself) but recurse into `init`.
            if let Some(init) = node.get_mut("init") {
                rewrite_walk(init, names, false);
            }
            return;
        }
        "AssignmentExpression" => {
            let op = node
                .get("operator")
                .and_then(|v| v.as_str())
                .unwrap_or("=")
                .to_string();
            let left_name = node
                .get("left")
                .and_then(|l| {
                    if l.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                        l.get("name").and_then(|v| v.as_str())
                    } else {
                        None
                    }
                })
                .map(|s| s.to_string());
            if let Some(name) = left_name {
                if names.contains(&name) {
                    if let Some(right) = node.get_mut("right") {
                        rewrite_walk(right, names, false);
                    }
                    let right_val = node.get("right").cloned().unwrap_or(Value::Null);
                    let needs_proxy_flag = op == "=" && rhs_needs_proxy(&right_val);
                    let new_value = if op == "=" {
                        right_val
                    } else {
                        let bin_op = op.trim_end_matches('=');
                        serde_json::json!({
                            "type": "BinaryExpression",
                            "operator": bin_op,
                            "left": make_get_call(&name),
                            "right": right_val
                        })
                    };
                    let mut args = vec![
                        serde_json::json!({ "type": "Identifier", "name": name }),
                        new_value,
                    ];
                    if needs_proxy_flag {
                        args.push(serde_json::json!({
                            "type": "Literal", "value": true, "raw": "true"
                        }));
                    }
                    *node = serde_json::json!({
                        "type": "CallExpression",
                        "callee": {
                            "type": "MemberExpression",
                            "object": { "type": "Identifier", "name": "$" },
                            "property": { "type": "Identifier", "name": "set" },
                            "computed": false,
                            "optional": false
                        },
                        "arguments": args,
                        "optional": false
                    });
                    return;
                }
            }
            // Other assignments — recurse into children.
        }
        "UpdateExpression" => {
            // x++, ++x, x--, --x. Only for Identifier args.
            if let Some(arg) = node.get("argument") {
                if arg.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                    let name = arg
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if names.contains(&name) {
                        let op = node
                            .get("operator")
                            .and_then(|v| v.as_str())
                            .unwrap_or("++");
                        let bin_op = if op == "++" { "+" } else { "-" };
                        *node = serde_json::json!({
                            "type": "CallExpression",
                            "callee": {
                                "type": "MemberExpression",
                                "object": { "type": "Identifier", "name": "$" },
                                "property": { "type": "Identifier", "name": "update" },
                                "computed": false,
                                "optional": false
                            },
                            "arguments": if bin_op == "-" {
                                serde_json::json!([
                                    { "type": "Identifier", "name": name },
                                    { "type": "Literal", "value": -1, "raw": "-1" }
                                ])
                            } else {
                                serde_json::json!([
                                    { "type": "Identifier", "name": name }
                                ])
                            },
                            "optional": false
                        });
                        return;
                    }
                }
            }
        }
        "Identifier" => {
            if !is_member_property {
                let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if names.contains(name) {
                    *node = make_get_call(name);
                    return;
                }
            }
            return;
        }
        "MemberExpression" => {
            let computed = node
                .get("computed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(obj) = node.get_mut("object") {
                rewrite_walk(obj, names, false);
            }
            if let Some(prop) = node.get_mut("property") {
                rewrite_walk(prop, names, !computed);
            }
            return;
        }
        "Property" => {
            // For shorthand `{ onmouseup }` where onmouseup is just an Identifier,
            // we should NOT rewrite to `{ onmouseup: $.get(onmouseup) }`.
            // Only rewrite when the value field is not a shorthand mirror.
            let shorthand = node
                .get("shorthand")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if shorthand {
                // Skip — don't recurse into key or value.
                return;
            }
            let computed = node
                .get("computed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if computed {
                if let Some(k) = node.get_mut("key") {
                    rewrite_walk(k, names, false);
                }
            }
            if let Some(v) = node.get_mut("value") {
                rewrite_walk(v, names, false);
            }
            return;
        }
        _ => {}
    }
    // Generic recurse.
    if let Some(obj) = node.as_object_mut() {
        for (_, v) in obj.iter_mut() {
            rewrite_walk(v, names, false);
        }
    } else if let Some(arr) = node.as_array_mut() {
        for v in arr.iter_mut() {
            rewrite_walk(v, names, false);
        }
    }
}

/// Decide whether a direct-assignment value needs the `true` 3rd arg to
/// `$.set` — indicating "treat as a possibly-proxied object". Mirrors
/// upstream's heuristic: function call result, member access on non-state
/// identifier, or anything else that isn't a known primitive.
fn rhs_needs_proxy(v: &Value) -> bool {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "Literal" => false,
        "Identifier" => false,
        // $.get(x) call — primitive read; no flag.
        "CallExpression" => {
            let callee = v.get("callee");
            let is_dot_get = callee
                .and_then(|c| c.get("type"))
                .and_then(|x| x.as_str())
                == Some("MemberExpression")
                && callee
                    .and_then(|c| c.get("object"))
                    .and_then(|o| o.get("name"))
                    .and_then(|x| x.as_str())
                    == Some("$")
                && callee
                    .and_then(|c| c.get("property"))
                    .and_then(|p| p.get("name"))
                    .and_then(|x| x.as_str())
                    == Some("get");
            if is_dot_get {
                false
            } else {
                // Generic call — could return anything; flag.
                true
            }
        }
        "ArrayExpression" | "ObjectExpression" => true,
        _ => false,
    }
}

fn make_get_call(name: &str) -> Value {
    serde_json::json!({
        "type": "CallExpression",
        "callee": {
            "type": "MemberExpression",
            "object": { "type": "Identifier", "name": "$" },
            "property": { "type": "Identifier", "name": "get" },
            "computed": false,
            "optional": false
        },
        "arguments": [{ "type": "Identifier", "name": name }],
        "optional": false
    })
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
    // Collect names of `let X = $props()` bindings — these turn into
    // `$.rest_props(...)` AND every read of `X.STATIC` is redirected to
    // `$$props.STATIC` (unless it's the direct LHS of an assignment).
    let mut rest_binding_names: std::collections::HashSet<String> = Default::default();
    let original = std::mem::take(body);
    let mut replaced: Vec<Value> = Vec::with_capacity(original.len());
    for stmt in original {
        if let Some(new_stmts) = try_rewrite_props_decl(&stmt) {
            replaced.extend(new_stmts);
        } else if let Some((new_stmt, name)) = try_rewrite_bare_props_decl(&stmt) {
            replaced.push(new_stmt);
            rest_binding_names.insert(name);
        } else {
            replaced.push(stmt);
        }
    }
    *body = replaced;
    if !rest_binding_names.is_empty() {
        rewrite_rest_member_reads(program, &rest_binding_names);
    }
}

/// `let X = $.props();` → `let X = $.rest_props($$props, ['$$slots', '$$events', '$$legacy']);`.
/// Returns the new statement plus the bound name X.
fn try_rewrite_bare_props_decl(stmt: &Value) -> Option<(Value, String)> {
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
    if id.get("type").and_then(|v| v.as_str()) != Some("Identifier") {
        return None;
    }
    let name = id.get("name").and_then(|v| v.as_str())?.to_string();
    let init = d.get("init")?;
    if init.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return None;
    }
    let callee = init.get("callee")?;
    if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return None;
    }
    let obj = callee
        .get("object")
        .and_then(|o| o.get("name"))
        .and_then(|v| v.as_str());
    let prop = callee
        .get("property")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str());
    if obj != Some("$") || prop != Some("props") {
        return None;
    }
    let rest_call = serde_json::json!({
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
            {
                "type": "ArrayExpression",
                "elements": [
                    { "type": "Literal", "value": "$$slots", "raw": "'$$slots'" },
                    { "type": "Literal", "value": "$$events", "raw": "'$$events'" },
                    { "type": "Literal", "value": "$$legacy", "raw": "'$$legacy'" }
                ]
            }
        ],
        "optional": false
    });
    let new_stmt = serde_json::json!({
        "type": "VariableDeclaration",
        "kind": kind,
        "declarations": [{
            "type": "VariableDeclarator",
            "id": { "type": "Identifier", "name": name.clone() },
            "init": rest_call
        }]
    });
    Some((new_stmt, name))
}

/// Walk a program. For every MemberExpression whose object is a "rest props"
/// binding name (via `let X = $props()`) and whose property is a non-computed
/// Identifier — rewrite `X` to `$$props`, EXCEPT when this MemberExpression is
/// the direct LHS of an AssignmentExpression (e.g. `X.foo = bar` keeps `X`).
fn rewrite_rest_member_reads(
    node: &mut Value,
    names: &std::collections::HashSet<String>,
) {
    fn is_target_member(member: &Value, names: &std::collections::HashSet<String>) -> bool {
        if member.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
            return false;
        }
        let obj = member.get("object").cloned().unwrap_or(Value::Null);
        if obj.get("type").and_then(|v| v.as_str()) != Some("Identifier") {
            return false;
        }
        let obj_name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !names.contains(obj_name) {
            return false;
        }
        let computed = member
            .get("computed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if computed {
            return false;
        }
        true
    }

    fn rewrite_target(member: &mut Value) {
        if let Some(obj) = member.get_mut("object") {
            *obj = serde_json::json!({ "type": "Identifier", "name": "$$props" });
        }
    }

    fn walk(
        node: &mut Value,
        names: &std::collections::HashSet<String>,
        skip_member_object_rewrite: bool,
    ) {
        if let Some(obj) = node.as_object_mut() {
            let ty = obj
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match ty.as_str() {
                "AssignmentExpression" => {
                    // The LHS member with our target name should be skipped.
                    // But nested member targets (e.g. `X.a.b = ...`) — the
                    // outer is the assignment LHS but the INNER `X.a` is a
                    // read of the chain; upstream rewrites `X.a.b = ...` →
                    // `$$props.a.b = ...`. So only skip the rewrite when the
                    // LHS is *directly* `X.foo`.
                    if let Some(left) = obj.get_mut("left") {
                        let left_is_direct_target = is_target_member(left, names) && {
                            // Ensure left.object is an Identifier (not nested member).
                            left.get("object")
                                .and_then(|o| o.get("type"))
                                .and_then(|v| v.as_str())
                                == Some("Identifier")
                        };
                        if left_is_direct_target {
                            // Don't rewrite the object of the LHS — but still
                            // recurse into deeper sub-nodes.
                            walk(left, names, true);
                        } else {
                            walk(left, names, false);
                        }
                    }
                    if let Some(right) = obj.get_mut("right") {
                        walk(right, names, false);
                    }
                    return;
                }
                "MemberExpression" => {
                    if !skip_member_object_rewrite && is_target_member(node, names) {
                        rewrite_target(node);
                    }
                    if let Some(o) = node.get_mut("object") {
                        walk(o, names, false);
                    }
                    let computed = node
                        .get("computed")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if computed {
                        if let Some(p) = node.get_mut("property") {
                            walk(p, names, false);
                        }
                    }
                    return;
                }
                _ => {}
            }
            for (_, v) in obj.iter_mut() {
                walk(v, names, false);
            }
        } else if let Some(arr) = node.as_array_mut() {
            for v in arr {
                walk(v, names, false);
            }
        }
    }
    walk(node, names, false);
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

/// Walk a program looking for `class { name = $.state(...) }` /
/// `class { name = $.derived(() => ...) }` patterns and rewrite them to
/// `#name = $.state(...)` plus getter/setter accessor pairs, mirroring
/// upstream's client-side ClassBody visitor.
///
/// Also rewrites assignments to private state fields inside the class
/// constructor: `this.#name = v` → `$.set(this.#name, v)`.
fn transform_class_state_fields(node: &mut Value) {
    fn walker(v: &mut Value) {
        if let Some(obj) = v.as_object_mut() {
            let ty = obj
                .get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if matches!(ty.as_str(), "ClassDeclaration" | "ClassExpression") {
                transform_class(obj);
            }
            for (_, v) in obj.iter_mut() {
                walker(v);
            }
        } else if let Some(arr) = v.as_array_mut() {
            for v in arr {
                walker(v);
            }
        }
    }
    walker(node);
}

fn transform_class(class_obj: &mut serde_json::Map<String, Value>) {
    let Some(body) = class_obj.get_mut("body") else {
        return;
    };
    let Some(inner) = body.get_mut("body").and_then(|v| v.as_array_mut()) else {
        return;
    };
    // First pass: collect which fields are state-backed (private name → was_public).
    // After renaming, the field key becomes `#name`. Constructor assignments to
    // these private names → `$.set(this.#name, value)`.
    let mut state_private_names: std::collections::HashSet<String> = Default::default();
    let original = std::mem::take(inner);
    let mut transformed: Vec<Value> = Vec::with_capacity(original.len());
    for member in original {
        transform_class_member(&member, &mut transformed, &mut state_private_names);
    }
    *inner = transformed;
    // Second pass: walk into the constructor body and rewrite `this.#X = v`
    // assignments for `X` in state_private_names.
    if !state_private_names.is_empty() {
        if let Some(inner) = body.get_mut("body").and_then(|v| v.as_array_mut()) {
            for member in inner.iter_mut() {
                if member.get("type").and_then(|v| v.as_str()) == Some("MethodDefinition")
                    && member.get("kind").and_then(|v| v.as_str()) == Some("constructor")
                {
                    if let Some(value) = member.get_mut("value") {
                        rewrite_private_assignments(value, &state_private_names);
                    }
                }
            }
        }
    }
}

fn transform_class_member(
    member: &Value,
    out: &mut Vec<Value>,
    state_private_names: &mut std::collections::HashSet<String>,
) {
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
    let kind = match dollar_call_kind(value) {
        Some(k) => k,
        None => {
            out.push(member.clone());
            return;
        }
    };
    let key = member.get("key").cloned().unwrap_or(Value::Null);
    let key_ty = key.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let (name, is_private_already) = match key_ty {
        "Identifier" => (
            key.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            false,
        ),
        "PrivateIdentifier" => (
            key.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            true,
        ),
        _ => {
            out.push(member.clone());
            return;
        }
    };
    if name.is_empty() {
        out.push(member.clone());
        return;
    }

    if is_private_already {
        // Already private — emit unchanged. Track for constructor rewrite if
        // it's a state (not derived) backing.
        if matches!(kind, DollarCallKind::State) {
            state_private_names.insert(name.clone());
        }
        out.push(member.clone());
        return;
    }

    // Public — rename to #name + emit getter/setter pair.
    let private_key = serde_json::json!({ "type": "PrivateIdentifier", "name": name });

    let mut renamed = member.clone();
    renamed["key"] = private_key.clone();
    out.push(renamed);

    // get NAME() { return $.get(this.#NAME); }
    let getter = serde_json::json!({
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
            "body": {
                "type": "BlockStatement",
                "body": [{
                    "type": "ReturnStatement",
                    "argument": {
                        "type": "CallExpression",
                        "callee": {
                            "type": "MemberExpression",
                            "object": { "type": "Identifier", "name": "$" },
                            "property": { "type": "Identifier", "name": "get" },
                            "computed": false,
                            "optional": false
                        },
                        "arguments": [{
                            "type": "MemberExpression",
                            "object": { "type": "ThisExpression" },
                            "property": private_key.clone(),
                            "computed": false,
                            "optional": false
                        }],
                        "optional": false
                    }
                }]
            }
        }
    });
    out.push(getter);

    // set NAME(value) { $.set(this.#NAME, value, true?); }
    let mut set_args = vec![
        serde_json::json!({
            "type": "MemberExpression",
            "object": { "type": "ThisExpression" },
            "property": private_key.clone(),
            "computed": false,
            "optional": false
        }),
        serde_json::json!({ "type": "Identifier", "name": "value" }),
    ];
    if matches!(kind, DollarCallKind::State) {
        set_args.push(serde_json::json!({
            "type": "Literal",
            "value": true,
            "raw": "true"
        }));
    }
    let setter = serde_json::json!({
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
            "params": [{ "type": "Identifier", "name": "value" }],
            "body": {
                "type": "BlockStatement",
                "body": [{
                    "type": "ExpressionStatement",
                    "expression": {
                        "type": "CallExpression",
                        "callee": {
                            "type": "MemberExpression",
                            "object": { "type": "Identifier", "name": "$" },
                            "property": { "type": "Identifier", "name": "set" },
                            "computed": false,
                            "optional": false
                        },
                        "arguments": set_args,
                        "optional": false
                    }
                }]
            }
        }
    });
    out.push(setter);
}

#[derive(Clone, Copy)]
enum DollarCallKind {
    State,
    Derived,
}

fn dollar_call_kind(v: &Value) -> Option<DollarCallKind> {
    if v.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return None;
    }
    let callee = v.get("callee")?;
    if callee.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return None;
    }
    let obj = callee.get("object")?.get("name").and_then(|v| v.as_str())?;
    let prop = callee.get("property")?.get("name").and_then(|v| v.as_str())?;
    if obj != "$" {
        return None;
    }
    match prop {
        "state" => Some(DollarCallKind::State),
        "derived" => Some(DollarCallKind::Derived),
        _ => None,
    }
}

/// Walk a function/method body. Rewrite `this.#X = v` to `$.set(this.#X, v)`
/// where X is in `state_private_names`. Also rewrite `this.#X` reads to
/// `$.get(this.#X)` is NOT applied here — upstream only does that outside the
/// class body (in non-method contexts the field's setter would normally handle
/// it, and constructors / methods access #X directly to participate in `$.set`).
fn rewrite_private_assignments(
    node: &mut Value,
    state_private_names: &std::collections::HashSet<String>,
) {
    if let Some(obj) = node.as_object_mut() {
        let ty = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if ty == "AssignmentExpression" {
            let op = obj
                .get("operator")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let left = obj.get("left").cloned().unwrap_or(Value::Null);
            if op == "=" {
                if let Some(name) = this_private_member_name(&left) {
                    if state_private_names.contains(&name) {
                        let right = obj.get("right").cloned().unwrap_or(Value::Null);
                        // Replace this AssignmentExpression with `$.set(left, right)`
                        *node = serde_json::json!({
                            "type": "CallExpression",
                            "callee": {
                                "type": "MemberExpression",
                                "object": { "type": "Identifier", "name": "$" },
                                "property": { "type": "Identifier", "name": "set" },
                                "computed": false,
                                "optional": false
                            },
                            "arguments": [left, right],
                            "optional": false
                        });
                        return;
                    }
                }
            }
        }
        for (_, v) in obj.iter_mut() {
            rewrite_private_assignments(v, state_private_names);
        }
    } else if let Some(arr) = node.as_array_mut() {
        for v in arr {
            rewrite_private_assignments(v, state_private_names);
        }
    }
}

fn this_private_member_name(expr: &Value) -> Option<String> {
    if expr.get("type").and_then(|v| v.as_str()) != Some("MemberExpression") {
        return None;
    }
    let object = expr.get("object")?;
    if object.get("type").and_then(|v| v.as_str()) != Some("ThisExpression") {
        return None;
    }
    let property = expr.get("property")?;
    if property.get("type").and_then(|v| v.as_str()) != Some("PrivateIdentifier") {
        return None;
    }
    property
        .get("name")
        .and_then(|v| v.as_str())
        .map(String::from)
}
