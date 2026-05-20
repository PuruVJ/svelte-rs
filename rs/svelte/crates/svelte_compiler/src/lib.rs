//! Public facade — `compile`, `compileModule`, `parse`, `preprocess`, `migrate`.
//!
//! Mirrors `packages/svelte/src/compiler/index.js`.
//!
//! Status: stub. `parse()` returns an empty `Root` so the test harness can
//! diff against JS output. Real parsing lands in `svelte_parse` (Phase 2).

#![forbid(unsafe_code)]

pub mod options;

pub use options::{
    CompileOptions, CssMode, ExperimentalOptions, FragmentsStrategy, Generate,
    ModuleCompileOptions, ParseOptions,
};
pub use svelte_ast::Root;
pub use svelte_diagnostics::CompileDiagnostic;

/// `parse(source, options)` — delegates to `svelte_parse::parse`.
///
/// Status: Phase 2a-2b implemented (text + comment at top level). Elements,
/// tags, blocks, scripts, and styles are pending.
///
/// Errors from the lower-level parser are propagated as `CompileDiagnostic`s,
/// matching upstream's `InternalCompileError` shape.
pub fn parse(source: &str, options: ParseOptions) -> Result<Root, CompileDiagnostic> {
    svelte_parse::parse(source, options.loose)
}

/// Output of `compile()` — mirrors upstream's `{ js, css, warnings, ast, stats }`.
/// Pruned to the fields currently produced by the port; the rest land alongside
/// their producing crate.
#[derive(Debug, Clone)]
pub struct CompileResult {
    pub js: String,
    pub warnings: Vec<svelte_diagnostics::CompileDiagnostic>,
}

/// `compile(source, options)` — parse, analyze, transform, codegen.
///
/// Status: minimal end-to-end pipeline. Server `generate: 'server'` produces
/// template + script lowering matching the simplest snapshot fixtures.
/// Client generation is not yet wired (Phase 6).
pub fn compile(
    source: &str,
    component_name: &str,
    options: CompileOptions,
) -> Result<CompileResult, CompileDiagnostic> {
    let mut root = svelte_parse::parse(source, false)?;
    let _analysis =
        svelte_analyze::analyze_component(root.clone(), options.module.filename.as_deref())?;

    // The pipeline is typed end-to-end: parse -> typed transform -> typed
    // print. If no typed transform can handle the input shape, we surface
    // an unsupported error (rather than fall back to a Value-based path —
    // none exists). Coverage is being grown fixture-by-fixture.
    let typed = match options.module.generate {
        Some(Generate::Server) => {
            if let Some(p) = svelte_transform_server::try_typed_server(&root, component_name) {
                p
            } else if let Some(p) =
                svelte_transform_server::try_typed_server_component(&root, component_name)
            {
                p
            } else {
                return Err(CompileDiagnostic {
                    code: "typed_server_unsupported",
                    message: "this Svelte source shape isn't yet handled by the typed server transform"
                        .to_string(),
                    position: None,
                });
            }
        }
        Some(Generate::Client) | None => {
            // Apply compile-time Math.X(literal-nums) fold to the fragment
            // before pattern matching so the walker sees constants.
            svelte_transform_client::walker_fold_in_fragment(&mut root.fragment);
            let use_tree = matches!(options.fragments, FragmentsStrategy::Tree);
            // In tree mode, skip typed_client (static-only $.from_html path)
            // and route directly to the walker which knows the tree shape.
            let fast = if use_tree {
                None
            } else {
                svelte_transform_client::try_typed_client(&root, component_name)
            };
            if let Some(p) = fast {
                p
            } else if let Some(p) =
                svelte_transform_client::try_typed_client_walker_with_filename(
                    &root,
                    component_name,
                    use_tree,
                    options.module.filename.as_deref(),
                )
            {
                p
            } else if let Some(p) =
                svelte_transform_client::try_typed_client_component(&root, component_name)
            {
                p
            } else {
                return Err(CompileDiagnostic {
                    code: "typed_client_unsupported",
                    message: "this Svelte source shape isn't yet handled by the typed client transform"
                        .to_string(),
                    position: None,
                });
            }
        }
    };

    let mut typed_opts = svelte_codegen_js::TypedPrintOptions::default();
    typed_opts.comments = root
        .comments
        .iter()
        .map(|c| svelte_codegen_js::TypedComment {
            kind: match c.kind {
                svelte_ast::root::JsCommentKind::Line => {
                    svelte_codegen_js::TypedCommentKind::Line
                }
                svelte_ast::root::JsCommentKind::Block => {
                    svelte_codegen_js::TypedCommentKind::Block
                }
            },
            value: c.value.clone(),
            start: c.start,
            end: c.end,
        })
        .collect();
    let result = svelte_codegen_js::print_typed(&typed, &typed_opts);

    Ok(CompileResult {
        js: result.code,
        warnings: Vec::new(),
    })
}

/// Unused stub kept during the typed-only migration to silence any old
/// references; will be removed once nothing forwards `serde_json::Value`
/// comments at all.
#[allow(dead_code)]
fn value_to_typed_comment_stub(c: &serde_json::Value) -> Option<svelte_codegen_js::TypedComment> {
    let obj = c.as_object()?;
    let kind = match obj.get("type")?.as_str()? {
        "Line" => svelte_codegen_js::TypedCommentKind::Line,
        "Block" => svelte_codegen_js::TypedCommentKind::Block,
        _ => return None,
    };
    Some(svelte_codegen_js::TypedComment {
        kind,
        value: obj.get("value")?.as_str()?.to_string(),
        start: obj.get("start")?.as_u64()? as u32,
        end: obj.get("end")?.as_u64()? as u32,
    })
}


