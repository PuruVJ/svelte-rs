//! Early sparse-islands compile: analyze + emit + direct JS without entering the walker funnel.

use svelte_ast::root::Root;
use svelte_transform_shared::compile_bump::CompileBump;

use crate::deep_static_js::try_emit_deep_static_walker_js;
use crate::direct_codegen::try_emit_client_program_direct;
use crate::script_fast::analyze_script_props_only;
use crate::sparse_multi_if_js::try_emit_sparse_multi_if_js;
use crate::sparse_pipeline::try_sparse_islands_program;
use crate::walker::{analyze_script, fold_fragment_with_consts, scan_fragment_assignments};

/// Full client compile for sparse-island shapes: direct JS when possible (no Program / print_typed).
pub fn try_emit_sparse_islands_client_js(
    root: &mut Root<'_>,
    component_name: &str,
    bump: &CompileBump,
) -> Option<String> {
    if root.css.is_some() || root.module.is_some() {
        return None;
    }

    let template_assigned = scan_fragment_assignments(&root.fragment);
    let script = analyze_script_props_only(root.instance.as_ref(), &template_assigned)
        .or_else(|| analyze_script(root.instance.as_ref(), &template_assigned))?;

    if !script.constants.is_empty() {
        fold_fragment_with_consts(&mut root.fragment, &script.constants);
    }

    // Deep-static walker (skip-static-subtree class): emit JS strings directly.
    if let Some(js) = try_emit_deep_static_walker_js(&root.fragment, component_name, &script, bump) {
        return Some(js);
    }

    // Multi-if sparse islands: direct module JS (no `Program` wrapper / `print_typed`).
    if let Some(js) = try_emit_sparse_multi_if_js(&root.fragment, component_name, &script, bump) {
        return Some(js);
    }

    // Fallback: full `Program` + direct printer.
    let program = try_sparse_islands_program(&root.fragment, component_name, &script)?;
    try_emit_client_program_direct(&program)
}
