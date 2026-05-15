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
