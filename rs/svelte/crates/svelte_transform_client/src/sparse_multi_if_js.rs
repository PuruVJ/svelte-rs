//! Direct JS for sparse top-level multi-if / multi-anchor templates.
//! Skips assembling a full `Program` AST (still builds per-function `Statement` bodies).

use svelte_ast::fragment::Fragment;
use svelte_transform_shared::compile_bump::CompileBump;

use crate::direct_codegen::emit_top_level_multi_if_module_js;
use crate::walker::{emit_top_level_multi_if_parts, ScriptInfo};

/// Emit client JS for sparse-island shapes classified by `emit_top_level_multi_if_parts`.
pub(crate) fn try_emit_sparse_multi_if_js(
    fragment: &Fragment<'_>,
    component_name: &str,
    script: &ScriptInfo,
    bump: &CompileBump,
) -> Option<String> {
    let parts = emit_top_level_multi_if_parts(&fragment.nodes, component_name, script)?;
    emit_top_level_multi_if_module_js(&parts, component_name, script, bump)
}
