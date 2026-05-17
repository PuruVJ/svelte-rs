//! Fully-typed `client_component` fast path for the simple-static
//! template case. See `svelte_transform_server::typed_fast` for the
//! sibling implementation — same intent, different output shape.
//!
//! Recognized shape:
//! - No `<script>` / `<script context="module">`.
//! - No `<style>` (no CSS scoping hooks).
//! - Single root element with static-only children (no `{expr}`, no
//!   blocks, no components, no directives).
//!
//! Output shape (matches what `_expected/client/hello-world.svelte.js`
//! encodes):
//! ```js
//! import 'svelte/internal/disclose-version';
//! import 'svelte/internal/flags/legacy';
//! import * as $ from 'svelte/internal/client';
//!
//! var root = $.from_html(`HTML`);
//!
//! export default function Name($$anchor) {
//!     var ROOT_VAR = root();
//!     $.append($$anchor, ROOT_VAR);
//! }
//! ```

use svelte_ast::root::Root;
use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_js_ast::*;
use svelte_transform_shared::builders_typed as t;

pub fn try_typed_client(root: &Root, component_name: &str) -> Option<Program> {
    if root.instance.is_some() || root.module.is_some() {
        return None;
    }
    if root.css.is_some() {
        return None;
    }
    let (root_el_tag, html) = static_single_root(&root.fragment)?;

    let import_disclose = t::import_side_effect("svelte/internal/disclose-version");
    let import_flags = t::import_side_effect("svelte/internal/flags/legacy");
    let import_internal = t::import_namespace("$", "svelte/internal/client");

    // `var root = $.from_html(\`HTML\`);`
    let from_html_call = t::call(
        t::member_id(t::id("$"), "from_html"),
        vec![t::template_raw(vec![html], vec![])],
    );
    let root_var = t::var("root", from_html_call);

    // Inside the function: `var TAG = root(); $.append($$anchor, TAG);`
    let tag_var = root_el_tag;
    let inner_var = t::var(&tag_var, t::call(t::id("root"), vec![]));
    let append_call = t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(&tag_var)],
    );
    let func_body = vec![inner_var, t::stmt(append_call)];

    let export = t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
        func_body,
    );

    Some(t::program(vec![
        import_disclose,
        import_flags,
        import_internal,
        root_var,
        export,
    ]))
}

/// If the fragment is a single root regular element whose children are all
/// static, return `(tag_name, html_for_from_html)`. Otherwise None.
///
/// `tag_name` is the variable name we'll bind the element to inside the
/// component function — for `<h1>`, it's `"h1"`. For multi-element roots
/// or anything dynamic we bail.
fn static_single_root(fragment: &Fragment) -> Option<(String, String)> {
    // Skip leading/trailing whitespace-only Text nodes the same way
    // `lower_fragment_trimmed` does.
    let non_ws: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            _ => true,
        })
        .collect();
    if non_ws.len() != 1 {
        return None;
    }
    let FragmentChild::RegularElement(el) = non_ws[0] else {
        return None;
    };
    if !el.attributes.is_empty() {
        return None;
    }
    let mut html = String::with_capacity(32);
    html.push('<');
    html.push_str(&el.name);
    html.push('>');
    for child in &el.fragment.nodes {
        append_static(child, &mut html)?;
    }
    if !is_void(&el.name) {
        html.push_str("</");
        html.push_str(&el.name);
        html.push('>');
    }
    Some((el.name.clone(), html))
}

fn append_static(child: &FragmentChild, out: &mut String) -> Option<()> {
    match child {
        FragmentChild::Text(t) => {
            for ch in t.data.chars() {
                match ch {
                    '`' => out.push_str("\\`"),
                    '\\' => out.push_str("\\\\"),
                    _ => out.push(ch),
                }
            }
            Some(())
        }
        FragmentChild::RegularElement(el) => {
            if !el.attributes.is_empty() {
                return None;
            }
            out.push('<');
            out.push_str(&el.name);
            out.push('>');
            if !is_void(&el.name) {
                for child in &el.fragment.nodes {
                    append_static(child, out)?;
                }
                out.push('<');
                out.push('/');
                out.push_str(&el.name);
                out.push('>');
            }
            Some(())
        }
        _ => None,
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input"
            | "link" | "meta" | "param" | "source" | "track" | "wbr"
    )
}
