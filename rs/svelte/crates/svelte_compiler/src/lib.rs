//! Public facade — `compile`, `compileModule`, `parse`, `preprocess`, `migrate`.
//!
//! Mirrors `packages/svelte/src/compiler/index.js`.
//!
//! Status: stub. `parse()` returns an empty `Root` so the test harness can
//! diff against JS output. Real parsing lands in `svelte_parse` (Phase 2).

#![forbid(unsafe_code)]


pub mod options;

pub use options::{
    derive_component_name, derive_component_name_from_filename, sanitize_export_name,
    CompileOptions, CssMode, ExperimentalOptions, FragmentsStrategy, Generate,
    ModuleCompileOptions, ParseOptions,
};
pub use svelte_ast::{Root, TemplateArena};
pub use svelte_diagnostics::CompileDiagnostic;
pub use svelte_migrate::{migrate, MigrateOptions, MigrateResult};
pub use svelte_parse::{parse_in_arena, AstBundle, ParsedComponent};
pub use svelte_transform_shared::compile_bump::CompileBump;
pub use svelte_print::{print as print_root, PrintOptions, PrintResult};

/// `parse(source, options)` — delegates to `svelte_parse::parse`.
///
/// Status: Phase 2a-2b implemented (text + comment at top level). Elements,
/// tags, blocks, scripts, and styles are pending.
///
/// Errors from the lower-level parser are propagated as `CompileDiagnostic`s,
/// matching upstream's `InternalCompileError` shape.
pub fn parse(source: &str, options: ParseOptions) -> Result<AstBundle, CompileDiagnostic> {
    svelte_parse::parse(source, options.loose)
}

/// Parse into an existing [`CompileBump`] template arena (same bump as [`compile`]).
pub fn parse_with_bump<'a>(
    bump: &'a CompileBump,
    source: &str,
    options: ParseOptions,
) -> Result<Root<'a>, CompileDiagnostic> {
    parse_in_arena(&bump.template, source, options.loose)
}

/// JS-shaped `{ code, map }` for compiled JavaScript.
#[derive(Debug, Clone)]
pub struct CompileJsOutput {
    pub code: String,
    /// Empty source map until codegen emits mappings.
    pub map: serde_json::Value,
}

impl CompileJsOutput {
    pub fn new(code: String) -> Self {
        Self {
            code,
            map: empty_source_map(),
        }
    }
}

/// Metadata returned by upstream `compile()`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompileMetadata {
    pub runes: bool,
}

/// Output of `compile()` — mirrors upstream's `{ js, css, warnings, metadata, ast }`.
#[derive(Debug, Clone)]
pub struct CompileResult {
    pub js: CompileJsOutput,
    pub css: Option<CompileCssOutput>,
    pub warnings: Vec<svelte_diagnostics::CompileDiagnostic>,
    pub metadata: CompileMetadata,
}

/// Compiled CSS when `css: 'external'` and a `<style>` block is present.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompileCssOutput {
    pub code: String,
    pub map: serde_json::Value,
    pub has_global: bool,
}

fn empty_source_map() -> serde_json::Value {
    serde_json::json!({
        "version": 3,
        "sources": [],
        "sourcesContent": [],
        "names": [],
        "mappings": ""
    })
}

fn finish_compile_result(
    code: String,
    warnings: Vec<svelte_diagnostics::CompileDiagnostic>,
    options: &CompileOptions,
) -> CompileResult {
    CompileResult {
        js: CompileJsOutput::new(code),
        css: None,
        warnings,
        metadata: CompileMetadata {
            runes: options.runes.unwrap_or(true),
        },
    }
}

/// `compile(source, options)` — same entry shape as `svelte/compiler`.
pub fn compile(
    source: &str,
    options: CompileOptions,
) -> Result<CompileResult, CompileDiagnostic> {
    let component_name = derive_component_name(&options);
    compile_with_name(source, &component_name, options)
}

/// `compile` with an explicit component name (tests, CLI `--name`).
pub fn compile_with_name(
    source: &str,
    component_name: &str,
    options: CompileOptions,
) -> Result<CompileResult, CompileDiagnostic> {
    if options.module.generate.is_none() {
        return Err(CompileDiagnostic {
            code: "options_invalid_value",
            message: "generate: false (analyze-only) is not supported by the Rust compiler yet"
                .to_string(),
            position: None,
        });
    }
    let compile_bump = svelte_transform_shared::compile_bump::CompileBump::new();
    let mut root = svelte_parse::parse_in_arena(&compile_bump.template, source, false)?;
    svelte_transform_shared::template_meta::mark_template_metadata(&mut root);
    if matches!(
        options.module.generate,
        Some(Generate::Client) | None
    ) {
        svelte_transform_client::precompute_static_html_cache(&mut root, compile_bump.bump());
    }
    // The pipeline is typed end-to-end: parse -> typed transform -> typed
    // print. If no typed transform can handle the input shape, we surface
    // an unsupported error (rather than fall back to a Value-based path —
    // none exists). Coverage is being grown fixture-by-fixture.
    let comments = std::mem::take(&mut root.comments);
    let typed = match options.module.generate {
        Some(Generate::Server) => {
            let exp_async = options.module.experimental.async_;
            let filename = options.module.filename.as_deref();
            let preserve_comments = options.preserve_comments;

            // CSS injection (compile option OR `<svelte:options css="injected" />`).
            // The svelte:options form wins when present.
            let svelte_options_inject = svelte_options_css_is_injected(&root);
            let css_inject = svelte_options_inject
                .unwrap_or(matches!(options.css, CssMode::Injected));
            // Pre-render the CSS when in injected mode + the source has
            // a `<style>` block.
            let css_inject_args: Option<(String, String)> = if css_inject && root.css.is_some() {
                let analysis_res = svelte_analyze::analyze_component(
                    &root,
                    options.module.filename.as_deref(),
                );
                let mut analysis = match analysis_res {
                    Ok(a) => a,
                    Err(_) => return Err(CompileDiagnostic {
                        code: "css_inject_analyze_failed",
                        message: "failed to analyze CSS for injection".to_string(),
                        position: None,
                    }),
                };
                let basis = options.module.filename.as_deref().unwrap_or("(unknown)");
                let hash = format!(
                    "svelte-{}",
                    svelte_transform_server::svelte_filename_hash_pub(basis)
                );
                analysis.css_hash = hash.clone();
                let rendered = analysis.root.css.as_ref().map(|sheet| {
                    let raw = svelte_analyze::css_render::render_stylesheet_with_opts_minify(
                        source, sheet, &analysis.css_meta, &hash, false, true,
                    );
                    // Upstream's `inject_styles && !dev` triggers minification.
                    // The renderer already removed pruned content under
                    // minify mode; post-process whitespace.
                    minify_css(&raw)
                });
                rendered.map(|code| (hash, code))
            } else {
                None
            };

            if let Some(p) = svelte_transform_server::try_typed_server_with(
                &root, component_name, exp_async, compile_bump.bump(),
            ) {
                p
            } else if let Some(p) =
                svelte_transform_server::try_typed_server_component_full(
                    root, component_name, exp_async, filename, preserve_comments,
                    css_inject_args, compile_bump.bump(),
                )
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
            if !use_tree {
                // Sparse islands: analyze + emit + direct JS (skip walker + print_typed).
                if let Some(js) = svelte_transform_client::try_emit_sparse_islands_client_js(
                    &mut root,
                    component_name,
                    &compile_bump,
                ) {
                    return Ok(finish_compile_result(js, Vec::new(), &options));
                }
                // Fully-static: emit JS directly (no Program, no print_typed).
                if let Some(js) = svelte_transform_client::try_emit_fully_static_client_js(
                    &root,
                    component_name,
                    &compile_bump,
                ) {
                    return Ok(finish_compile_result(js, Vec::new(), &options));
                }
            }
            // In tree mode, skip typed_client (static-only $.from_html path)
            // and route directly to the walker which knows the tree shape.
            let fast = if use_tree {
                None
            } else {
                svelte_transform_client::try_typed_client(&root, component_name, compile_bump.bump())
            };
            if let Some(p) = fast {
                p
            } else if let Some(p) =
                svelte_transform_client::try_typed_client_component(
                    &root, component_name, compile_bump.bump(),
                )
            {
                p
            } else if let Some(p) =
                svelte_transform_client::try_typed_client_walker_with_filename(
                    root,
                    component_name,
                    use_tree,
                    options.module.filename.as_deref(),
                    compile_bump.bump(),
                )
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

    if matches!(
        options.module.generate,
        Some(Generate::Client) | None
    ) {
        if let Some(js) = svelte_transform_client::try_emit_client_program_direct(&typed) {
            return Ok(finish_compile_result(js, Vec::new(), &options));
        }
    }

    let mut typed_opts = svelte_codegen_js::TypedPrintOptions::default();
    typed_opts.comments = comments
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
    typed_opts.code_init_capacity =
        svelte_codegen_js::estimate_code_init_capacity(source.len());
    let result = svelte_codegen_js::print_typed(&typed, &typed_opts);

    Ok(finish_compile_result(result.code, Vec::new(), &options))
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

/// Simple CSS minifier — collapses whitespace around `{`, `}`, `:`, `;`
/// and strips line breaks inside rule bodies. Used for SSR injected CSS
/// to match upstream's `minify: inject_styles && !dev` output.
fn minify_css(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    // States for whitespace handling. After certain tokens we suppress
    // following whitespace (no space after `{` etc.).
    let mut just_emitted_space = true; // suppress leading WS
    let mut suppress_next_ws = true;
    while let Some(c) = chars.next() {
        match c {
            // `/* ... */` comments are dropped in minify mode.
            '/' if matches!(chars.peek(), Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for cc in chars.by_ref() {
                    if prev == '*' && cc == '/' {
                        break;
                    }
                    prev = cc;
                }
                // Treat the dropped comment as whitespace: collapse to a
                // single space (unless suppressed).
                if !suppress_next_ws && !just_emitted_space {
                    out.push(' ');
                    just_emitted_space = true;
                }
                continue;
            }
            // `{` keeps a leading space (`.foo {color}` — selector-brace gap).
            '{' => {
                out.push(c);
                suppress_next_ws = true;
                just_emitted_space = false;
            }
            ';' | ':' => {
                // Strip any trailing space we just emitted.
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
                suppress_next_ws = true;
                just_emitted_space = false;
            }
            '}' => {
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
                // Don't suppress trailing whitespace — the renderer's minify
                // mode strips preceding whitespace from each rule, so any
                // space that survives is meaningful (a leading-pruned-
                // selector boundary kept its space).
                suppress_next_ws = false;
                just_emitted_space = false;
            }
            // `,` in selector lists keeps a following space (upstream's
            // injected-CSS output emits `.a, .b {…}`).
            ',' => {
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
                suppress_next_ws = false;
                just_emitted_space = false;
            }
            ' ' | '\t' | '\n' | '\r' => {
                if suppress_next_ws || just_emitted_space {
                    continue;
                }
                out.push(' ');
                just_emitted_space = true;
            }
            _ => {
                out.push(c);
                just_emitted_space = false;
                suppress_next_ws = false;
            }
        }
    }
    // Trim final whitespace.
    while out.ends_with(' ') || out.ends_with('\n') || out.ends_with('\t') {
        out.pop();
    }
    out
}

/// Detect `<svelte:options css="injected" />` at the root level.
/// Returns Some(true) when present and set to "injected", Some(false) when
/// present but set to "external", None when no svelte:options css attr.
fn svelte_options_css_is_injected(root: &Root<'_>) -> Option<bool> {
    use svelte_ast::fragment::FragmentChild;
    use svelte_ast::attributes::{AttributeValue, AttributeValuePart, ElementAttribute};
    for n in &root.fragment.nodes {
        if let FragmentChild::SvelteOptions(opt) = n {
            for a in &opt.attributes {
                if let ElementAttribute::Attribute(attr) = a {
                    if attr.name == "css" {
                        if let AttributeValue::Many(parts) = &attr.value {
                            if parts.len() == 1 {
                                if let AttributeValuePart::Text(t) = &parts[0] {
                                    return Some(t.data == "injected");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}
