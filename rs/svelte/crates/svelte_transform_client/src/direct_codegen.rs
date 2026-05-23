//! Direct string emission for fully-static client components (bypasses large `Program` trees).

use svelte_ast::root::Root;
use svelte_js_ast::*;

use svelte_transform_shared::builders_typed as t;

/// Build a minimal `Program` for the `try_typed_client` shape using fewer allocations
/// than the generic typed-fast path when the template is a single HTML slab.
pub fn program_fully_static_client(
    component_name: &str,
    html: &str,
    multi_root: bool,
) -> Program {
    let mut from_html_args = vec![t::template_raw(vec![html.to_string()], vec![])];
    if multi_root {
        from_html_args.push(t::lit_number(1.0));
    }
    let root_var = t::var(
        "root",
        t::call(t::member_id(t::id("$"), "from_html"), from_html_args),
    );
    let inner_name = if multi_root {
        "fragment"
    } else {
        // Caller must pass tag name for single-root; default fragment for multi only.
        "fragment"
    };
    let mut func_body = vec![t::var(inner_name, t::call(t::id("root"), vec![]))];
    if multi_root {
        func_body.push(t::stmt(t::call(
            t::member_id(t::id("$"), "next"),
            vec![t::lit_number(2.0)],
        )));
    }
    func_body.push(t::stmt(t::call(
        t::member_id(t::id("$"), "append"),
        vec![t::id("$$anchor"), t::id(inner_name)],
    )));
    let export = t::export_default_function(
        component_name,
        vec![t::pat_id("$$anchor")],
        func_body,
    );
    t::program(vec![
        t::import_side_effect("svelte/internal/disclose-version"),
        t::import_side_effect("svelte/internal/flags/legacy"),
        t::import_namespace("$", "svelte/internal/client"),
        root_var,
        export,
    ])
}

/// Returns true when `root` matches the typed-fast fully-static contract.
pub fn is_fully_static_root(root: &Root) -> bool {
    root.instance.is_none() && root.module.is_none() && root.css.is_none()
}
