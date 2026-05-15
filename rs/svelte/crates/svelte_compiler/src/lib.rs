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
    let root = svelte_parse::parse(source, false)?;
    let _analysis =
        svelte_analyze::analyze_component(root.clone(), options.module.filename.as_deref())?;

    let program = match options.module.generate {
        Some(Generate::Server) => svelte_transform_server::server_component(&root, component_name),
        Some(Generate::Client) | None => {
            svelte_transform_client::client_component(&root, component_name)
        }
    };

    let result = svelte_codegen_js::print(
        &program,
        &svelte_codegen_js::default_visitors(),
        &svelte_codegen_js::PrintOptions::default(),
    );

    Ok(CompileResult {
        js: result.code,
        warnings: Vec::new(),
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn parse_empty_returns_empty_root() {
        let r = parse("", ParseOptions::default()).unwrap();
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 0);
        assert!(r.fragment.nodes.is_empty());
        assert!(r.instance.is_none());
        assert!(r.module.is_none());
    }

    #[test]
    fn parse_strips_bom() {
        let r = parse("\u{feff}", ParseOptions::default()).unwrap();
        assert_eq!(r.end, 0);
    }

    #[test]
    fn experimental_async_default_is_false() {
        let opts = CompileOptions::default();
        assert!(!opts.module.experimental.async_);
    }

    /// Deserialize a JSON shape matching what a bundler plugin would send.
    /// Confirms field names map to upstream JS camelCase.
    #[test]
    fn options_json_deserialize() {
        let json = serde_json::json!({
            "dev": true,
            "generate": "server",
            "filename": "App.svelte",
            "rootDir": "/project",
            "experimental": { "async": true },
            "customElement": true,
            "modernAst": true,
            "preserveWhitespace": false,
            "preserveComments": true,
            "discloseVersion": false,
            "fragments": "tree",
            "css": "injected"
        });
        let opts: CompileOptions = serde_json::from_value(json).unwrap();
        assert!(opts.module.dev);
        assert_eq!(opts.module.generate, Some(Generate::Server));
        assert_eq!(opts.module.filename.as_deref(), Some("App.svelte"));
        assert!(opts.module.experimental.async_);
        assert!(opts.custom_element);
        assert!(opts.modern_ast);
        assert!(!opts.preserve_whitespace);
        assert!(opts.preserve_comments);
        assert!(!opts.disclose_version);
        assert_eq!(opts.fragments, FragmentsStrategy::Tree);
        assert_eq!(opts.css, CssMode::Injected);
    }

    #[test]
    fn compile_hello_world_server_byte_equal() {
        let source = "<h1>hello world</h1>";
        let mut opts = CompileOptions::default();
        opts.module.generate = Some(Generate::Server);
        let result = compile(source, "Hello_world", opts).expect("compile should succeed");
        let expected = "import * as $ from 'svelte/internal/server';\n\nexport default function Hello_world($$renderer) {\n\t$$renderer.push(`<h1>hello world</h1>`);\n}";
        assert_eq!(result.js, expected);
    }

    #[test]
    fn compile_hello_world_client_byte_equal() {
        let source = "<h1>hello world</h1>";
        let mut opts = CompileOptions::default();
        opts.module.generate = Some(Generate::Client);
        let result = compile(source, "Hello_world", opts).expect("compile should succeed");
        let expected = "import 'svelte/internal/disclose-version';\nimport 'svelte/internal/flags/legacy';\nimport * as $ from 'svelte/internal/client';\n\nvar root = $.from_html(`<h1>hello world</h1>`);\n\nexport default function Hello_world($$anchor) {\n\tvar h1 = root();\n\n\t$.append($$anchor, h1);\n}";
        assert_eq!(result.js, expected);
    }

    /// The Rust empty-Root serializes to JSON with the same key set as the JS
    /// parser would emit for a truly empty `.svelte` file. (Modulo the runtime
    /// parser stripping `comments` when empty in some test paths — we keep it.)
    #[test]
    fn empty_root_json_shape() {
        let r = parse("", ParseOptions::default()).unwrap();
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["type"], "Root");
        assert_eq!(j["start"], 0);
        assert_eq!(j["end"], 0);
        assert_eq!(j["css"], Value::Null);
        assert_eq!(j["options"], Value::Null);
        assert_eq!(j["fragment"]["type"], "Fragment");
        assert_eq!(j["fragment"]["nodes"].as_array().unwrap().len(), 0);
    }
}
