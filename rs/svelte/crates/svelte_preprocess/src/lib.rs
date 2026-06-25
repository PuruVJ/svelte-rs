//! Preprocess pipeline.
//!
//! Mirrors `packages/svelte/src/compiler/preprocess/`.
//!
//! Status: Rust-side scaffolding. The Rust API mirrors the upstream shape
//! (`Processed`, `PreprocessorGroup`, `preprocess()`), but the actual
//! processor invocation must happen via the WASM/JS bridge — Svelte
//! preprocessors are user-provided JS callbacks (e.g. `vitePreprocess`,
//! `svelte-preprocess`'s sass/typescript wrappers, etc.) and cannot be
//! executed from pure Rust.
//!
//! The WASM facade calls each user-provided JS preprocessor and then re-enters
//! Rust via this module to combine sourcemaps and update the rolling source.

#![forbid(unsafe_code)]

/// One processor's output. Mirrors upstream `Processed`:
/// `{ code, map?, dependencies?, attributes? }`.
#[derive(Debug, Clone, Default)]
pub struct Processed {
    pub code: String,
    pub map: Option<String>,
    pub dependencies: Vec<String>,
}

/// Result of `preprocess()`. Mirrors upstream's `{ code, dependencies, map, toString() }`.
#[derive(Debug, Clone)]
pub struct PreprocessResult {
    pub code: String,
    pub dependencies: Vec<String>,
    /// Combined VLQ-encoded sourcemap, when at least one processor returned one.
    pub map: Option<String>,
}

impl PreprocessResult {
    pub fn identity(source: impl Into<String>) -> Self {
        Self {
            code: source.into(),
            dependencies: Vec::new(),
            map: None,
        }
    }
}

/// Compose a list of already-applied processor outputs into a single result.
/// The WASM glue calls this after invoking each user preprocessor via the JS
/// callback bridge — Rust only has to manage the rolling source + dep list +
/// sourcemap chain, not the processor invocation itself.
pub fn combine(initial_source: &str, processed: Vec<Processed>) -> PreprocessResult {
    let mut code = initial_source.to_string();
    let mut dependencies: Vec<String> = Vec::new();
    let mut last_map: Option<String> = None;
    for p in processed {
        code = p.code;
        dependencies.extend(p.dependencies);
        if let Some(m) = p.map {
            last_map = Some(m);
            // Real implementation: compose sourcemaps via the magic-string
            // sourcemap helpers. For now we take the last map verbatim —
            // good enough for single-processor pipelines.
        }
    }
    PreprocessResult {
        code,
        dependencies,
        map: last_map,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_returns_input_unchanged() {
        let r = PreprocessResult::identity("hello");
        assert_eq!(r.code, "hello");
        assert!(r.dependencies.is_empty());
        assert!(r.map.is_none());
    }

    #[test]
    fn combine_threads_processor_outputs() {
        let r = combine(
            "<p>x</p>",
            vec![
                Processed {
                    code: "<p>X</p>".into(),
                    map: None,
                    dependencies: vec!["a.css".into()],
                },
                Processed {
                    code: "<p>!</p>".into(),
                    map: Some("AAAA".into()),
                    dependencies: vec!["b.css".into()],
                },
            ],
        );
        assert_eq!(r.code, "<p>!</p>");
        assert_eq!(r.dependencies, vec!["a.css", "b.css"]);
        assert_eq!(r.map.as_deref(), Some("AAAA"));
    }
}
