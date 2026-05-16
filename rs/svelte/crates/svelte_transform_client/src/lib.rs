//! Phase 3 client transform.
//!
//! Mirrors `packages/svelte/src/compiler/phases/3-transform/client/`.
//!
//! Coverage so far: structural skeleton + static HTML + simple Component
//! invocations + HMR wrapper + runes lowering (client-side: `$state(x)` →
//! `$.state(x)`, `$derived(x)` → `$.derived(() => x)`, etc.).
//!
//! The real 53-visitor port is being filled in incrementally driven by
//! failing snapshot fixtures. See `rs_port_status.md` for the up-to-date
//! progress sheet.

#![forbid(unsafe_code)]

use serde_json::Value;
use svelte_ast::root::Root;
use svelte_transform_shared::builders as b;

pub mod rewrite;
pub mod template;

pub use template::serialize_static_html;

/// Options forwarded to the client transform. Matches the relevant subset of
/// upstream's `ValidatedCompileOptions`.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub hmr: bool,
    pub dev: bool,
    pub filename: Option<String>,
}

/// Transform a parsed `Root` into a client-side ESTree `Program`.
pub fn client_component(root: &Root, component_name: &str) -> Value {
    client_component_with_options(root, component_name, &ClientOptions::default())
}

/// Like [`client_component`] but accepts compile options (HMR, dev mode, etc.).
pub fn client_component_with_options(
    root: &Root,
    component_name: &str,
    options: &ClientOptions,
) -> Value {
    let html = template::serialize_static_html(&root.fragment);
    let runes_mode = uses_runes(root);

    let mut program_body: Vec<Value> = Vec::new();
    program_body.push(import_side_effect("svelte/internal/disclose-version"));
    if !runes_mode {
        program_body.push(import_side_effect("svelte/internal/flags/legacy"));
    }
    program_body.push(b::import_all("$", "svelte/internal/client"));

    // Hoisted instance imports
    let rewritten_instance = root.instance.as_ref().map(|i| rewrite::rewrite_program(i.content.clone()));

    if let Some(rewritten) = &rewritten_instance {
        let body = rewritten
            .get("body")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for stmt in body {
            if stmt.get("type").and_then(|v| v.as_str()) == Some("ImportDeclaration") {
                program_body.push(stmt);
            }
        }
    }

    let mut fn_body: Vec<Value> = Vec::new();

    // Instance-script non-import statements appear inside the component fn.
    if let Some(rewritten) = &rewritten_instance {
        let body = rewritten
            .get("body")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for stmt in body {
            let ty = stmt.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(ty, "ImportDeclaration") {
                continue;
            }
            fn_body.push(stmt);
        }
    }

    let has_meaningful_html = !html.trim().is_empty();
    if has_meaningful_html {
        program_body.push(b::declaration(
            "var",
            vec![b::declarator(
                b::id("root"),
                Some(b::call(
                    b::member(b::id("$"), b::id("from_html"), false, false),
                    vec![b::template_literal(vec![&html], vec![])],
                )),
            )],
        ));

        let single_root_name = guess_single_root_var(&root.fragment.nodes);
        if let Some(top_name) = single_root_name {
            fn_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id(&top_name),
                    Some(b::call(b::id("root"), vec![])),
                )],
            ));
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("append"), false, false),
                vec![b::id("$$anchor"), b::id(&top_name)],
            )));
        } else {
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("append"), false, false),
                vec![b::id("$$anchor"), b::call(b::id("root"), vec![])],
            )));
        }
    }

    // If the only template content is a single Component invocation, emit
    // `Foo($$anchor, props)` directly inside the body.
    if !has_meaningful_html {
        if let Some(single_comp) = find_single_component(&root.fragment.nodes) {
            let call_with_directives = build_component_call(single_comp, runes_mode);
            for stmt in call_with_directives {
                fn_body.push(stmt);
            }
        } else if let Some(svelte_el) = find_single_svelte_element(&root.fragment.nodes) {
            // `<svelte:element this={tag}>...</svelte:element>` client lowering.
            fn_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id("fragment"),
                    Some(b::call(
                        b::member(b::id("$"), b::id("comment"), false, false),
                        vec![],
                    )),
                )],
            ));
            fn_body.push(b::declaration(
                "var",
                vec![b::declarator(
                    b::id("node"),
                    Some(b::call(
                        b::member(b::id("$"), b::id("first_child"), false, false),
                        vec![b::id("fragment")],
                    )),
                )],
            ));
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("element"), false, false),
                vec![b::id("node"), svelte_el.tag.clone(), b::literal_bool(false)],
            )));
            fn_body.push(b::stmt(b::call(
                b::member(b::id("$"), b::id("append"), false, false),
                vec![b::id("$$anchor"), b::id("fragment")],
            )));
        }
    }

    // Detect $$props usage in body (via fn_body's identifiers) so we know
    // whether to include the parameter.
    let uses_props = body_uses_identifier(&fn_body, "$$props");
    let mut params = vec![b::id("$$anchor")];
    if uses_props {
        params.push(b::id("$$props"));
    }
    let component_fn = b::function_declaration(
        b::id(component_name),
        params,
        b::block(fn_body),
        false,
    );

    if options.hmr {
        // HMR wrapper: declare component as `function`, wrap with $.hmr,
        // accept module updates, export at the end.
        program_body.push(component_fn);
        program_body.push(serde_json::json!({
            "type": "IfStatement",
            "test": {
                "type": "MemberExpression",
                "object": {
                    "type": "MetaProperty",
                    "meta": { "type": "Identifier", "name": "import" },
                    "property": { "type": "Identifier", "name": "meta" }
                },
                "property": { "type": "Identifier", "name": "hot" },
                "computed": false,
                "optional": false
            },
            "consequent": b::block(vec![
                b::stmt(b::assignment("=",
                    b::id(component_name),
                    b::call(
                        b::member(b::id("$"), b::id("hmr"), false, false),
                        vec![b::id(component_name)],
                    ),
                )),
                b::stmt(b::call(
                    b::member(
                        b::member(
                            b::member(
                                b::id("import"),
                                b::id("meta"),
                                false, false,
                            ),
                            b::id("hot"),
                            false, false,
                        ),
                        b::id("accept"),
                        false, false,
                    ),
                    vec![b::arrow(
                        vec![b::id("module")],
                        b::block(vec![
                            b::stmt(b::call(
                                b::member(
                                    b::member(
                                        b::id(component_name),
                                        b::member(b::id("$"), b::id("HMR"), false, false),
                                        true, false,
                                    ),
                                    b::id("update"),
                                    false, false,
                                ),
                                vec![b::member(b::id("module"), b::id("default"), false, false)],
                            )),
                        ]),
                        false,
                    )],
                )),
            ]),
            "alternate": serde_json::Value::Null
        }));
        program_body.push(b::export_default(b::id(component_name)));
    } else {
        program_body.push(b::export_default(component_fn));
    }

    b::program(program_body)
}

fn import_side_effect(source: &str) -> Value {
    serde_json::json!({
        "type": "ImportDeclaration",
        "specifiers": [],
        "source": b::literal_str(source)
    })
}

/// Detect whether the component uses runes. Includes the explicit
/// `<svelte:options runes />` opt-in and any rune call site.
fn uses_runes(root: &Root) -> bool {
    if let Some(opts) = &root.options {
        if opts.runes == Some(true) {
            return true;
        }
    }
    let Some(instance) = &root.instance else {
        return false;
    };
    let json = instance.content.to_string();
    for rune in [
        "\"$state\"",
        "\"$state.raw\"",
        "\"$derived\"",
        "\"$derived.by\"",
        "\"$effect\"",
        "\"$effect.pre\"",
        "\"$effect.root\"",
        "\"$props\"",
        "\"$props.id\"",
        "\"$bindable\"",
        "\"$inspect\"",
        "\"$inspect.trace\"",
        "\"$host\"",
    ] {
        if json.contains(rune) {
            return true;
        }
    }
    false
}

fn guess_single_root_var(nodes: &[svelte_ast::fragment::FragmentChild]) -> Option<String> {
    use svelte_ast::fragment::FragmentChild;
    let mut element: Option<&str> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::RegularElement(el) => {
                if element.is_some() {
                    return None;
                }
                element = Some(el.name.as_str());
            }
            _ => return None,
        }
    }
    element.map(|s| s.to_string())
}

/// When the only non-whitespace content of a fragment is a single Component
/// invocation, return it. Used to emit a direct `Foo($$anchor, props)` call
/// without a template literal.
/// Walk a list of statements looking for any Identifier reference matching `name`.
fn body_uses_identifier(stmts: &[Value], name: &str) -> bool {
    fn walk(v: &Value, name: &str) -> bool {
        match v {
            Value::Array(arr) => arr.iter().any(|x| walk(x, name)),
            Value::Object(obj) => {
                if obj.get("type").and_then(|v| v.as_str()) == Some("Identifier")
                    && obj.get("name").and_then(|v| v.as_str()) == Some(name)
                {
                    return true;
                }
                obj.values().any(|v| walk(v, name))
            }
            _ => false,
        }
    }
    stmts.iter().any(|s| walk(s, name))
}

fn find_single_svelte_element(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<&svelte_ast::elements::SvelteElement> {
    use svelte_ast::fragment::FragmentChild;
    let mut el: Option<&svelte_ast::elements::SvelteElement> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::SvelteElement(e) => {
                if el.is_some() {
                    return None;
                }
                el = Some(e);
            }
            _ => return None,
        }
    }
    el
}

fn find_single_component(
    nodes: &[svelte_ast::fragment::FragmentChild],
) -> Option<&svelte_ast::elements::Component> {
    use svelte_ast::fragment::FragmentChild;
    let mut comp: Option<&svelte_ast::elements::Component> = None;
    for n in nodes {
        match n {
            FragmentChild::Text(t) if t.data.trim().is_empty() => continue,
            FragmentChild::Comment(_) => continue,
            FragmentChild::Component(c) => {
                if comp.is_some() {
                    return None;
                }
                comp = Some(c);
            }
            _ => return None,
        }
    }
    comp
}

/// Build the call `Foo($$anchor, { ...props })` for a Component, plus any
/// `bind:` directive wrappers (e.g. `$.bind_this(Foo(...), setter, getter)`).
/// Returns the statement list to add to the function body.
fn build_component_call(
    c: &svelte_ast::elements::Component,
    runes_mode: bool,
) -> Vec<Value> {
    use svelte_ast::attributes::{Attribute, AttributeValue, ElementAttribute};
    let mut props: Vec<Value> = Vec::new();
    let mut bind_this: Option<&Value> = None;
    for a in &c.attributes {
        match a {
            ElementAttribute::Attribute(Attribute { name, value, .. }) => {
                let value_expr = match value {
                    AttributeValue::Empty(_) => b::literal_bool(true),
                    AttributeValue::Single(tag) => tag.expression.clone(),
                    AttributeValue::Many(_) => b::literal_str(""),
                };
                props.push(b::init(name, value_expr));
            }
            ElementAttribute::BindDirective(bd) if bd.name == "this" => {
                bind_this = Some(&bd.expression);
            }
            _ => {}
        }
    }
    // Non-runes (legacy) mode adds `$$legacy: true` to props.
    if !runes_mode {
        props.push(b::init("$$legacy", b::literal_bool(true)));
    }

    let component_call = b::call(b::id(&c.name), vec![b::id("$$anchor"), b::object(props)]);

    if let Some(expr) = bind_this {
        // $.bind_this(call, ($$value) => target = $$value, () => target)
        let setter = b::arrow(
            vec![b::id("$$value")],
            b::assignment("=", expr.clone(), b::id("$$value")),
            false,
        );
        let getter = b::arrow(vec![], expr.clone(), false);
        return vec![b::stmt(b::call(
            b::member(b::id("$"), b::id("bind_this"), false, false),
            vec![component_call, setter, getter],
        ))];
    }

    vec![b::stmt(component_call)]
}
