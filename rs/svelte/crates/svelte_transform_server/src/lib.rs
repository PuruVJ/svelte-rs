//! Phase 3 server transform.
//!
//! Mirrors `packages/svelte/src/compiler/phases/3-transform/server/`.
//!
//! Current scope: lower a template fragment to `$$renderer.push(\`...\`)`
//! statements interleaved with control-flow lowered from `{#if}` / `{#each}`.
//! Real visitor coverage — components, snippets, await/key blocks, dynamic
//! tags, all directives, the full script-body integration — is being filled
//! in driven by failing snapshot fixtures.

#![forbid(unsafe_code)]

use serde_json::Value;
use svelte_ast::root::Root;
use svelte_transform_shared::builders as b;

pub mod rewrite;
pub mod template;

pub use template::{ops_to_statements, TemplateChunks, TemplateOp};

/// Transform an analyzed `Root` into an acorn-shaped JSON `Program` ready for
/// `svelte_codegen_js::print()`.
pub fn server_component(root: &Root, component_name: &str) -> Value {
    // Collect derived bindings up front so the template-expression rewriter
    // can call them as thunks.
    let mut rewritten_instance: Option<Value> = None;
    let mut derived_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(instance) = &root.instance {
        let rewritten = rewrite::rewrite_program(instance.content.clone());
        derived_names = rewrite::collect_derived_names(&rewritten);
        rewritten_instance = Some(rewritten);
    }

    // Hoist top-level `{#snippet}` blocks out of the fragment — upstream emits
    // them as sibling `function NAME($$renderer, ...) { ... }` declarations
    // before `export default function Component(...)`.
    let mut root_with_rewritten_template: Root = root.clone();
    if !derived_names.is_empty() {
        rewrite_fragment_derived_refs(&mut root_with_rewritten_template.fragment, &derived_names);
    }
    // Collect script-level constants (`let X = 'literal'`, never reassigned,
    // never originally a $state/$derived binding) and substitute Identifier
    // references in template expressions with the literal value. Enables
    // nullish-coalescence-omittance fold-through.
    if let Some(rewritten) = &rewritten_instance {
        let original_state_names = root
            .instance
            .as_ref()
            .map(|i| collect_original_state_names(&i.content))
            .unwrap_or_default();
        let mut constants = collect_script_constants(rewritten);
        constants.retain(|n, _| !original_state_names.contains(n));
        if !constants.is_empty() {
            substitute_constants_in_fragment(
                &mut root_with_rewritten_template.fragment,
                &constants,
            );
        }
    }
    let hoisted_snippets = extract_top_level_snippets(&mut root_with_rewritten_template.fragment);
    let template_ops = template::lower_fragment_trimmed(&root_with_rewritten_template.fragment);
    let mut function_body: Vec<Value> = Vec::new();

    // Instance script body — runs each render. Strips imports/exports (hoisted).
    if let Some(rewritten) = &rewritten_instance {
        let (hoisted, body) = split_script_body(rewritten);
        function_body.extend(body);
        let _ = hoisted;
    }

    let has_bind_on_component = fragment_has_bind_on_component(&root_with_rewritten_template.fragment);
    let template_stmts = template::ops_to_statements(template_ops);

    if has_bind_on_component {
        // Wrap the template body in the `do { ... } while (!$$settled)`
        // pattern with copy/subsume so `bind:X={target}` round-trips on the
        // server. Mirrors upstream's `Component.js` settled-binding path.
        function_body.push(b::declaration(
            "let",
            vec![b::declarator(b::id("$$settled"), Some(b::literal_bool(true)))],
        ));
        function_body.push(b::declaration(
            "let",
            vec![b::declarator(b::id("$$inner_renderer"), None)],
        ));
        function_body.push(b::function_declaration(
            b::id("$$render_inner"),
            vec![b::id("$$renderer")],
            b::block(template_stmts),
            false,
        ));
        // do { $$settled = true; $$inner_renderer = $$renderer.copy(); $$render_inner($$inner_renderer); } while (!$$settled);
        let do_body = b::block(vec![
            b::stmt(b::assignment("=", b::id("$$settled"), b::literal_bool(true))),
            b::stmt(b::assignment(
                "=",
                b::id("$$inner_renderer"),
                b::call(
                    b::member(b::id("$$renderer"), b::id("copy"), false, false),
                    vec![],
                ),
            )),
            b::stmt(b::call(b::id("$$render_inner"), vec![b::id("$$inner_renderer")])),
        ]);
        function_body.push(serde_json::json!({
            "type": "DoWhileStatement",
            "body": do_body,
            "test": {
                "type": "UnaryExpression",
                "operator": "!",
                "prefix": true,
                "argument": { "type": "Identifier", "name": "$$settled" }
            }
        }));
        function_body.push(b::stmt(b::call(
            b::member(b::id("$$renderer"), b::id("subsume"), false, false),
            vec![b::id("$$inner_renderer")],
        )));
    } else {
        function_body.extend(template_stmts);
    }

    let needs_context = component_needs_context(&root);
    let needs_props = uses_props(&root) || needs_context;
    let mut params = vec![b::id("$$renderer")];
    if needs_props {
        params.push(b::id("$$props"));
    }

    // When the component needs context (class fields with $state, full
    // `let props = $props()` rebind without destructuring, etc.), wrap the
    // body in `$$renderer.component(($$renderer) => { ... })`. Mirrors
    // upstream's `should_inject_context` path in `transform-server.js:259-269`.
    let body_value = if needs_context {
        // Rewrite `let X = $$props` (which came from `let X = $props()` via
        // the rune erasure) to the destructured form
        // `let { $$slots, $$events, ...X } = $$props;`.
        let function_body = rewrite_full_props_rebind(function_body);
        let wrapped = b::block(vec![b::stmt(b::call(
            b::member(b::id("$$renderer"), b::id("component"), false, false),
            vec![b::arrow(
                vec![b::id("$$renderer")],
                b::block(function_body),
                false,
            )],
        ))]);
        wrapped
    } else {
        b::block(function_body)
    };

    let component_fn = b::function_declaration(
        b::id(component_name),
        params,
        body_value,
        false,
    );

    // Module script body — runs once. All of its statements are hoisted to the
    // top of the Program (after the `$` import).
    let mut program_body: Vec<Value> = Vec::new();
    if uses_async(root) {
        // `import 'svelte/internal/flags/async';` as a side-effect — matches
        // upstream's `if (options.experimental.async)` injection in
        // transform-server.js:388-390.
        program_body.push(serde_json::json!({
            "type": "ImportDeclaration",
            "specifiers": [],
            "source": b::literal_str("svelte/internal/flags/async")
        }));
    }
    program_body.push(b::import_all("$", "svelte/internal/server"));

    if let Some(module_script) = &root.module {
        program_body.extend(extract_program_body(&module_script.content));
    }
    if let Some(rewritten) = &rewritten_instance {
        let (hoisted, _body) = split_script_body(rewritten);
        program_body.extend(hoisted);
    }

    // Hoisted snippet function declarations go between the imports and the
    // export-default component function.
    for snippet in hoisted_snippets {
        program_body.push(snippet);
    }

    program_body.push(b::export_default(component_fn));
    b::program(program_body)
}

/// Remove top-level `{#snippet name(...)}{/snippet}` blocks from the fragment
/// and lower each to a `function name($$renderer, ...params) { ... }`
/// declaration. Mirrors upstream's snippet hoisting in
/// `transform-server.js` (handles the `uses_component_bindings` path's
/// snippet collection).
/// True if any Component in the fragment has a `bind:X={target}` directive
/// (not bind:this). Triggers the server-side do-while + copy/subsume wrapper.
fn fragment_has_bind_on_component(f: &svelte_ast::Fragment) -> bool {
    use svelte_ast::attributes::ElementAttribute;
    use svelte_ast::fragment::FragmentChild;
    fn walk(f: &svelte_ast::Fragment) -> bool {
        for n in &f.nodes {
            match n {
                FragmentChild::Component(c) => {
                    for a in &c.attributes {
                        if let ElementAttribute::BindDirective(bd) = a {
                            if bd.name != "this" {
                                return true;
                            }
                        }
                    }
                    if walk(&c.fragment) {
                        return true;
                    }
                }
                FragmentChild::RegularElement(el) => {
                    if walk(&el.fragment) {
                        return true;
                    }
                }
                FragmentChild::IfBlock(b) => {
                    if walk(&b.consequent) {
                        return true;
                    }
                    if let Some(alt) = b.alternate.as_ref() {
                        if walk(alt) {
                            return true;
                        }
                    }
                }
                FragmentChild::EachBlock(b) => {
                    if walk(&b.body) {
                        return true;
                    }
                    if let Some(fb) = b.fallback.as_ref() {
                        if walk(fb) {
                            return true;
                        }
                    }
                }
                FragmentChild::KeyBlock(b) => {
                    if walk(&b.fragment) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }
    walk(f)
}

/// Collect names originally bound to `\$state(...)` / `\$state.raw(...)` /
/// `\$derived(...)` / `\$derived.by(...)`, BEFORE rune rewriting. The
/// pre-rewrite Program is walked.
fn collect_original_state_names(program: &Value) -> std::collections::HashSet<String> {
    let mut out: std::collections::HashSet<String> = Default::default();
    fn walk(v: &Value, out: &mut std::collections::HashSet<String>) {
        match v {
            Value::Array(arr) => arr.iter().for_each(|x| walk(x, out)),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    if let Some(init) = obj.get("init") {
                        if is_rune_state_or_derived(init) {
                            if let Some(n) = obj
                                .get("id")
                                .and_then(|i| i.get("name"))
                                .and_then(|v| v.as_str())
                            {
                                out.insert(n.to_string());
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

fn is_rune_state_or_derived(v: &Value) -> bool {
    if v.get("type").and_then(|v| v.as_str()) != Some("CallExpression") {
        return false;
    }
    let callee = match v.get("callee") {
        Some(c) => c,
        None => return false,
    };
    let ty = callee.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ty {
        "Identifier" => {
            let name = callee.get("name").and_then(|v| v.as_str()).unwrap_or("");
            matches!(name, "$state" | "$derived")
        }
        "MemberExpression" => {
            let obj_n = callee
                .get("object")
                .and_then(|o| o.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let prop_n = callee
                .get("property")
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            (obj_n == "$state" && prop_n == "raw")
                || (obj_n == "$derived" && prop_n == "by")
        }
        _ => false,
    }
}

/// Collect script-level `let X = 'literal' | number | boolean` bindings that
/// are never reassigned. The values are returned as JS Literal AST nodes
/// (ready to substitute into template expressions).
fn collect_script_constants(
    program: &Value,
) -> std::collections::HashMap<String, Value> {
    let body = program.get("body").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    // First pass: candidates (name → init literal).
    let mut candidates: std::collections::HashMap<String, Value> = Default::default();
    for stmt in &body {
        if stmt.get("type").and_then(|v| v.as_str()) != Some("VariableDeclaration") {
            continue;
        }
        let decls = match stmt.get("declarations").and_then(|v| v.as_array()) {
            Some(d) => d,
            None => continue,
        };
        for d in decls {
            let name = d
                .get("id")
                .and_then(|i| i.get("name"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let init = d.get("init").cloned();
            if let (Some(name), Some(init)) = (name, init) {
                if init.get("type").and_then(|v| v.as_str()) == Some("Literal") {
                    candidates.insert(name, init);
                }
            }
        }
    }
    if candidates.is_empty() {
        return candidates;
    }
    // Second pass: drop any candidate that's reassigned anywhere in the
    // program (AssignmentExpression / UpdateExpression with that identifier
    // as LHS).
    fn collect_reassigned(node: &Value, out: &mut std::collections::HashSet<String>) {
        match node {
            Value::Array(arr) => arr.iter().for_each(|v| collect_reassigned(v, out)),
            Value::Object(obj) => {
                let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ty == "AssignmentExpression" {
                    if let Some(left) = obj.get("left") {
                        if left.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                            if let Some(n) = left.get("name").and_then(|v| v.as_str()) {
                                out.insert(n.to_string());
                            }
                        }
                    }
                } else if ty == "UpdateExpression" {
                    if let Some(arg) = obj.get("argument") {
                        if arg.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                            if let Some(n) = arg.get("name").and_then(|v| v.as_str()) {
                                out.insert(n.to_string());
                            }
                        }
                    }
                }
                for (_, v) in obj.iter() {
                    collect_reassigned(v, out);
                }
            }
            _ => {}
        }
    }
    let mut reassigned: std::collections::HashSet<String> = Default::default();
    collect_reassigned(program, &mut reassigned);
    candidates.retain(|n, _| !reassigned.contains(n));
    candidates
}

/// Walk the fragment and replace bare Identifier references (in template
/// expressions, attribute values, block tests, etc.) whose name is in
/// `constants` with the constant Literal value.
fn substitute_constants_in_fragment(
    f: &mut svelte_ast::Fragment,
    constants: &std::collections::HashMap<String, Value>,
) {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    use svelte_ast::fragment::FragmentChild;
    fn substitute_walk(
        node: &mut Value,
        constants: &std::collections::HashMap<String, Value>,
        is_member_property: bool,
    ) {
        if let Some(obj) = node.as_object_mut() {
            let ty = obj
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Skip ArrowFunctionExpression / FunctionExpression bodies — they
            // create their own scopes and may shadow these names.
            if matches!(
                ty.as_str(),
                "ArrowFunctionExpression" | "FunctionExpression" | "FunctionDeclaration"
            ) {
                return;
            }
            if ty == "Identifier" && !is_member_property {
                if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                    if let Some(replacement) = constants.get(name) {
                        *node = replacement.clone();
                        return;
                    }
                }
            }
            if ty == "MemberExpression" {
                if let Some(o) = obj.get_mut("object") {
                    substitute_walk(o, constants, false);
                }
                let computed = obj
                    .get("computed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if computed {
                    if let Some(p) = obj.get_mut("property") {
                        substitute_walk(p, constants, false);
                    }
                }
                return;
            }
            if ty == "Property" {
                let shorthand = obj.get("shorthand").and_then(|v| v.as_bool()).unwrap_or(false);
                if shorthand {
                    return;
                }
                let computed = obj.get("computed").and_then(|v| v.as_bool()).unwrap_or(false);
                if !computed {
                    if let Some(v) = obj.get_mut("value") {
                        substitute_walk(v, constants, false);
                    }
                    return;
                }
            }
            for (_, v) in obj.iter_mut() {
                substitute_walk(v, constants, false);
            }
        } else if let Some(arr) = node.as_array_mut() {
            for v in arr {
                substitute_walk(v, constants, false);
            }
        }
    }
    fn substitute_in_fragment(
        f: &mut svelte_ast::Fragment,
        constants: &std::collections::HashMap<String, Value>,
    ) {
        for node in f.nodes.iter_mut() {
            match node {
                FragmentChild::ExpressionTag(t) => substitute_walk(&mut t.expression, constants, false),
                FragmentChild::HtmlTag(t) => substitute_walk(&mut t.expression, constants, false),
                FragmentChild::ConstTag(t) => substitute_walk(&mut t.declaration, constants, false),
                FragmentChild::RenderTag(t) => substitute_walk(&mut t.expression, constants, false),
                FragmentChild::IfBlock(b) => {
                    substitute_walk(&mut b.test, constants, false);
                    substitute_in_fragment(&mut b.consequent, constants);
                    if let Some(alt) = b.alternate.as_mut() {
                        substitute_in_fragment(alt, constants);
                    }
                }
                FragmentChild::EachBlock(b) => {
                    substitute_walk(&mut b.expression, constants, false);
                    substitute_in_fragment(&mut b.body, constants);
                    if let Some(fb) = b.fallback.as_mut() {
                        substitute_in_fragment(fb, constants);
                    }
                }
                FragmentChild::KeyBlock(b) => {
                    substitute_walk(&mut b.expression, constants, false);
                    substitute_in_fragment(&mut b.fragment, constants);
                }
                FragmentChild::AwaitBlock(b) => {
                    substitute_walk(&mut b.expression, constants, false);
                    if let Some(fb) = b.pending.as_mut() {
                        substitute_in_fragment(fb, constants);
                    }
                    if let Some(fb) = b.then.as_mut() {
                        substitute_in_fragment(fb, constants);
                    }
                    if let Some(fb) = b.catch_.as_mut() {
                        substitute_in_fragment(fb, constants);
                    }
                }
                FragmentChild::SnippetBlock(b) => {
                    substitute_in_fragment(&mut b.body, constants);
                }
                FragmentChild::RegularElement(el) => {
                    for a in el.attributes.iter_mut() {
                        match a {
                            ElementAttribute::Attribute(attr) => match &mut attr.value {
                                AttributeValue::Single(t) => substitute_walk(&mut t.expression, constants, false),
                                AttributeValue::Many(parts) => {
                                    for p in parts.iter_mut() {
                                        if let AttributeValuePart::ExpressionTag(t) = p {
                                            substitute_walk(&mut t.expression, constants, false);
                                        }
                                    }
                                }
                                AttributeValue::Empty(_) => {}
                            },
                            ElementAttribute::BindDirective(bd) => substitute_walk(&mut bd.expression, constants, false),
                            ElementAttribute::SpreadAttribute(sa) => substitute_walk(&mut sa.expression, constants, false),
                            _ => {}
                        }
                    }
                    substitute_in_fragment(&mut el.fragment, constants);
                }
                FragmentChild::Component(c) => {
                    for a in c.attributes.iter_mut() {
                        match a {
                            ElementAttribute::Attribute(attr) => match &mut attr.value {
                                AttributeValue::Single(t) => substitute_walk(&mut t.expression, constants, false),
                                AttributeValue::Many(parts) => {
                                    for p in parts.iter_mut() {
                                        if let AttributeValuePart::ExpressionTag(t) = p {
                                            substitute_walk(&mut t.expression, constants, false);
                                        }
                                    }
                                }
                                AttributeValue::Empty(_) => {}
                            },
                            ElementAttribute::BindDirective(bd) => substitute_walk(&mut bd.expression, constants, false),
                            ElementAttribute::SpreadAttribute(sa) => substitute_walk(&mut sa.expression, constants, false),
                            _ => {}
                        }
                    }
                    substitute_in_fragment(&mut c.fragment, constants);
                }
                FragmentChild::SvelteElement(el) => {
                    substitute_walk(&mut el.tag, constants, false);
                    substitute_in_fragment(&mut el.fragment, constants);
                }
                _ => {}
            }
        }
    }
    substitute_in_fragment(f, constants);
}

fn extract_top_level_snippets(f: &mut svelte_ast::Fragment) -> Vec<Value> {
    use svelte_ast::fragment::FragmentChild;
    let mut hoisted: Vec<Value> = Vec::new();
    let nodes = std::mem::take(&mut f.nodes);
    for node in nodes {
        if let FragmentChild::SnippetBlock(blk) = &node {
            // Snippet bodies always get a leading `<!---->` marker (regardless
            // of whether the body has dynamic content), so upstream's runtime
            // can locate the snippet's start in the parent template.
            let ops = template::lower_fragment_trimmed(&blk.body);
            let body = template::prepend_marker_and_to_statements(ops);
            let name = blk
                .expression
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("$$snippet")
                .to_string();
            let params: Vec<Value> = std::iter::once(b::id("$$renderer"))
                .chain(blk.parameters.iter().cloned())
                .collect();
            hoisted.push(b::function_declaration(b::id(&name), params, b::block(body), false));
        } else {
            f.nodes.push(node);
        }
    }
    hoisted
}

/// Whether the component needs a `$$props` parameter. Returns true if the
/// instance script contains a `$props()` call or `$$props` identifier
/// reference. Mirrors upstream's `should_inject_props` check
/// (`transform-server.js:311-318`).
fn uses_props(root: &Root) -> bool {
    let Some(instance) = &root.instance else {
        return false;
    };
    let json = instance.content.to_string();
    json.contains("\"$props\"") || json.contains("\"$$props\"")
}

/// Decide whether the component needs the `$$renderer.component(...)` wrapper.
/// Activates when:
/// - The instance script has `let X = $props()` (full-rebind, no destructuring).
/// - A class contains `$state`/`$derived` fields.
/// Mirrors upstream's `analysis.needs_context` set in `phases/2-analyze/index.js`.
fn component_needs_context(root: &Root) -> bool {
    let Some(instance) = &root.instance else {
        return false;
    };
    let json = instance.content.to_string();
    if (json.contains("\"ClassDeclaration\"") || json.contains("\"ClassExpression\""))
        && json.contains("\"$state\"")
    {
        return true;
    }
    has_full_props_rebind(&instance.content)
}

/// `let X = $props()` (no destructuring) — walks the parsed program.
fn has_full_props_rebind(program: &Value) -> bool {
    fn walk(node: &Value) -> bool {
        match node {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("VariableDeclarator") {
                    let id_is_identifier = obj
                        .get("id")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("Identifier");
                    let init = obj.get("init");
                    let init_is_props = init
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("CallExpression")
                        && init
                            .and_then(|v| v.get("callee"))
                            .and_then(|v| v.get("name"))
                            .and_then(|v| v.as_str())
                            == Some("$props");
                    if id_is_identifier && init_is_props {
                        return true;
                    }
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    walk(program)
}

/// Rewrite `let X = $$props;` (the result of `let X = $props()` after rune
/// erasure) into `let { $$slots, $$events, ...X } = $$props;` so the context
/// wrapper can pull off slots/events. Mirrors upstream's full-rebind path.
fn rewrite_full_props_rebind(body: Vec<Value>) -> Vec<Value> {
    body.into_iter()
        .map(|stmt| {
            if stmt.get("type").and_then(|v| v.as_str()) != Some("VariableDeclaration") {
                return stmt;
            }
            let mut stmt = stmt;
            if let Some(decls) = stmt.get_mut("declarations").and_then(|v| v.as_array_mut()) {
                for d in decls.iter_mut() {
                    let is_init_props = d
                        .get("init")
                        .and_then(|v| v.get("name"))
                        .and_then(|v| v.as_str())
                        == Some("$$props");
                    let id_is_ident = d
                        .get("id")
                        .and_then(|v| v.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("Identifier");
                    if is_init_props && id_is_ident {
                        let name = d
                            .get("id")
                            .and_then(|v| v.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("props")
                            .to_string();
                        d["id"] = serde_json::json!({
                            "type": "ObjectPattern",
                            "properties": [
                                {
                                    "type": "Property",
                                    "kind": "init",
                                    "key": { "type": "Identifier", "name": "$$slots" },
                                    "value": { "type": "Identifier", "name": "$$slots" },
                                    "computed": false,
                                    "shorthand": true,
                                    "method": false
                                },
                                {
                                    "type": "Property",
                                    "kind": "init",
                                    "key": { "type": "Identifier", "name": "$$events" },
                                    "value": { "type": "Identifier", "name": "$$events" },
                                    "computed": false,
                                    "shorthand": true,
                                    "method": false
                                },
                                {
                                    "type": "RestElement",
                                    "argument": { "type": "Identifier", "name": name }
                                }
                            ]
                        });
                    }
                }
            }
            stmt
        })
        .collect()
}

/// Whether the component uses `experimental.async` features.
fn uses_async(root: &Root) -> bool {
    use svelte_ast::fragment::{Fragment, FragmentChild};
    fn json_has_await(v: &Value) -> bool {
        v.to_string().contains("\"AwaitExpression\"")
    }
    fn fragment_uses_async(f: &Fragment) -> bool {
        for n in &f.nodes {
            match n {
                FragmentChild::ExpressionTag(t) if json_has_await(&t.expression) => return true,
                FragmentChild::HtmlTag(t) if json_has_await(&t.expression) => return true,
                FragmentChild::ConstTag(t) if json_has_await(&t.declaration) => return true,
                FragmentChild::RenderTag(t) if json_has_await(&t.expression) => return true,
                FragmentChild::RegularElement(el) => {
                    if fragment_uses_async(&el.fragment) {
                        return true;
                    }
                }
                FragmentChild::IfBlock(b) => {
                    if json_has_await(&b.test) || fragment_uses_async(&b.consequent) {
                        return true;
                    }
                    if let Some(alt) = &b.alternate {
                        if fragment_uses_async(alt) {
                            return true;
                        }
                    }
                }
                FragmentChild::EachBlock(b) => {
                    if json_has_await(&b.expression) || fragment_uses_async(&b.body) {
                        return true;
                    }
                }
                FragmentChild::KeyBlock(b) => {
                    if json_has_await(&b.expression) || fragment_uses_async(&b.fragment) {
                        return true;
                    }
                }
                FragmentChild::Component(c) => {
                    if fragment_uses_async(&c.fragment) {
                        return true;
                    }
                }
                FragmentChild::SvelteElement(el) => {
                    if fragment_uses_async(&el.fragment) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }
    if fragment_uses_async(&root.fragment) {
        return true;
    }
    if let Some(instance) = &root.instance {
        // Top-level await in instance script — must be experimental.async.
        if instance_has_top_level_await(&instance.content) {
            return true;
        }
    }
    false
}

/// Walk only top-level statements of `program.body[]` (not into nested
/// FunctionDeclaration / ArrowFunctionExpression / FunctionExpression bodies)
/// to detect an await that requires `experimental.async`.
fn instance_has_top_level_await(program: &Value) -> bool {
    fn walk(node: &Value) -> bool {
        match node {
            Value::Array(arr) => arr.iter().any(walk),
            Value::Object(obj) => {
                let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ty == "AwaitExpression" {
                    return true;
                }
                if matches!(
                    ty,
                    "FunctionDeclaration" | "FunctionExpression" | "ArrowFunctionExpression"
                ) {
                    return false;
                }
                obj.values().any(walk)
            }
            _ => false,
        }
    }
    walk(program)
}

/// Walk a `Fragment` rewriting every embedded expression so identifiers in
/// `deriveds` become `name()` calls. Used for server's `$derived` getter
/// invocation pattern.
fn rewrite_fragment_derived_refs(
    f: &mut svelte_ast::Fragment,
    deriveds: &std::collections::HashSet<String>,
) {
    use svelte_ast::fragment::FragmentChild;
    for node in f.nodes.iter_mut() {
        match node {
            FragmentChild::ExpressionTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
            }
            FragmentChild::HtmlTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
            }
            FragmentChild::ConstTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.declaration, deriveds);
            }
            FragmentChild::RenderTag(t) => {
                rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
            }
            FragmentChild::IfBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.test, deriveds);
                rewrite_fragment_derived_refs(&mut b.consequent, deriveds);
                if let Some(alt) = b.alternate.as_mut() {
                    rewrite_fragment_derived_refs(alt, deriveds);
                }
            }
            FragmentChild::EachBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.expression, deriveds);
                rewrite_fragment_derived_refs(&mut b.body, deriveds);
                if let Some(fb) = b.fallback.as_mut() {
                    rewrite_fragment_derived_refs(fb, deriveds);
                }
            }
            FragmentChild::KeyBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.expression, deriveds);
                rewrite_fragment_derived_refs(&mut b.fragment, deriveds);
            }
            FragmentChild::AwaitBlock(b) => {
                rewrite::rewrite_derived_refs(&mut b.expression, deriveds);
                if let Some(f) = b.pending.as_mut() {
                    rewrite_fragment_derived_refs(f, deriveds);
                }
                if let Some(f) = b.then.as_mut() {
                    rewrite_fragment_derived_refs(f, deriveds);
                }
                if let Some(f) = b.catch_.as_mut() {
                    rewrite_fragment_derived_refs(f, deriveds);
                }
            }
            FragmentChild::SnippetBlock(b) => {
                rewrite_fragment_derived_refs(&mut b.body, deriveds);
            }
            FragmentChild::RegularElement(el) => {
                rewrite_attrs_derived_refs(&mut el.attributes, deriveds);
                rewrite_fragment_derived_refs(&mut el.fragment, deriveds);
            }
            FragmentChild::Component(c) => {
                rewrite_attrs_derived_refs(&mut c.attributes, deriveds);
                rewrite_fragment_derived_refs(&mut c.fragment, deriveds);
            }
            FragmentChild::SvelteElement(el) => {
                rewrite::rewrite_derived_refs(&mut el.tag, deriveds);
                rewrite_attrs_derived_refs(&mut el.attributes, deriveds);
                rewrite_fragment_derived_refs(&mut el.fragment, deriveds);
            }
            FragmentChild::SvelteHead(el) => rewrite_fragment_derived_refs(&mut el.fragment, deriveds),
            FragmentChild::SvelteFragment(el) => rewrite_fragment_derived_refs(&mut el.fragment, deriveds),
            FragmentChild::TitleElement(el) => rewrite_fragment_derived_refs(&mut el.fragment, deriveds),
            _ => {}
        }
    }
}

fn rewrite_attrs_derived_refs(
    attrs: &mut Vec<svelte_ast::ElementAttribute>,
    deriveds: &std::collections::HashSet<String>,
) {
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for a in attrs.iter_mut() {
        match a {
            ElementAttribute::Attribute(attr) => match &mut attr.value {
                AttributeValue::Single(tag) => {
                    rewrite::rewrite_derived_refs(&mut tag.expression, deriveds);
                }
                AttributeValue::Many(parts) => {
                    for p in parts.iter_mut() {
                        if let AttributeValuePart::ExpressionTag(t) = p {
                            rewrite::rewrite_derived_refs(&mut t.expression, deriveds);
                        }
                    }
                }
                AttributeValue::Empty(_) => {}
            },
            ElementAttribute::BindDirective(bd) => {
                rewrite::rewrite_derived_refs(&mut bd.expression, deriveds);
            }
            ElementAttribute::SpreadAttribute(sa) => {
                rewrite::rewrite_derived_refs(&mut sa.expression, deriveds);
            }
            _ => {}
        }
    }
}

/// Pull `body[]` out of a parsed ESTree Program JSON value.
fn extract_program_body(program: &Value) -> Vec<Value> {
    program
        .get("body")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Split the instance `<script>` content into (hoisted top-level decls, body
/// stmts that run each render). Imports are hoisted; everything else stays.
fn split_script_body(program: &Value) -> (Vec<Value>, Vec<Value>) {
    let body = extract_program_body(program);
    let mut hoisted: Vec<Value> = Vec::new();
    let mut keep: Vec<Value> = Vec::new();
    for stmt in body {
        let ty = stmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "ImportDeclaration" => hoisted.push(stmt),
            "ExportNamedDeclaration" => {
                // `export const x = ...` → the inner declaration stays in the
                // function body; the `export` wrapper is dropped. (For now.)
                if let Some(decl) = stmt.get("declaration") {
                    if !decl.is_null() {
                        keep.push(decl.clone());
                    }
                }
            }
            _ => keep.push(stmt),
        }
    }
    (hoisted, keep)
}
