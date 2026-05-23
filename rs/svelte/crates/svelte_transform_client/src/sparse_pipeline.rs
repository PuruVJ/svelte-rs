//! Fast path for sparse-island components (one `$.from_html` slab + `$.sibling`/`$.next`).
//!
//! Matches upstream `skip-static-subtree` style output without running the full
//! PRE-DETECT funnel in `walker.rs`.

use svelte_ast::fragment::{Fragment, FragmentChild};
use svelte_js_ast::Program;

use crate::walker::{analyze_script, emit_top_level_multi_if_program, ScriptInfo};

/// Try the slot-based sparse-islands emitter used by `skip-static-subtree` and similar fixtures.
pub fn try_sparse_islands_program(
    fragment: &Fragment<'_>,
    component_name: &str,
    script: &ScriptInfo,
) -> Option<Program<'static>> {
    if script.async_info.is_some()
        || script.has_class_with_runes
        || !script.state_bindings.is_empty()
        || !script.derived_bindings.is_empty()
    {
        return None;
    }
    if fragment
        .nodes
        .iter()
        .any(|n| matches!(n, FragmentChild::SnippetBlock(_)))
    {
        return None;
    }

    let non_ws: Vec<&FragmentChild> = fragment
        .nodes
        .iter()
        .filter(|n| match n {
            FragmentChild::Text(t) => !t.data.trim().is_empty(),
            FragmentChild::Comment(_) => false,
            FragmentChild::SvelteOptions(_) => false,
            _ => true,
        })
        .collect();

    let has_anchor = non_ws.iter().any(|n| match n {
        FragmentChild::IfBlock(_)
        | FragmentChild::EachBlock(_)
        | FragmentChild::ExpressionTag(_)
        | FragmentChild::HtmlTag(_)
        | FragmentChild::Component(_) => true,
        FragmentChild::RegularElement(el) => el.metadata.dynamic || !el.metadata.is_static_element,
        _ => false,
    });

    if non_ws.len() >= 2
        && has_anchor
        && non_ws.iter().all(|n| {
            matches!(
                n,
                FragmentChild::IfBlock(_)
                    | FragmentChild::EachBlock(_)
                    | FragmentChild::RegularElement(_)
                    | FragmentChild::Text(_)
                    | FragmentChild::ExpressionTag(_)
                    | FragmentChild::HtmlTag(_)
                    | FragmentChild::Component(_)
            )
        })
    {
        return emit_top_level_multi_if_program(&fragment.nodes, component_name, script);
    }
    None
}

/// Classify + emit in one step when the root fragment qualifies.
pub fn try_sparse_islands_root(
    root: &svelte_ast::root::Root<'_>,
    component_name: &str,
    template_assigned: &std::collections::HashSet<String>,
) -> Option<Program<'static>> {
    let script = analyze_script(root.instance.as_ref(), template_assigned)?;
    try_sparse_islands_program(&root.fragment, component_name, &script)
}
