//! Compile option types.
//!
//! Ported from `packages/svelte/src/compiler/types/index.d.ts:ModuleCompileOptions`
//! and `CompileOptions`. Defaults are taken from
//! `packages/svelte/src/compiler/validate-options.js`. Defer the actual
//! validation (which depends on diagnostics + the dispatch shape of the
//! upstream validator) to Phase 2.

use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

use svelte_ast::Namespace;

/// `ModuleCompileOptions` — applies to both `compile` and `compileModule`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleCompileOptions {
    #[serde(default)]
    pub dev: bool,
    #[serde(default = "default_generate", deserialize_with = "deserialize_generate_option")]
    pub generate: Option<Generate>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default, rename = "rootDir")]
    pub root_dir: Option<String>,
    #[serde(default)]
    pub experimental: ExperimentalOptions,
}

fn default_generate() -> Option<Generate> {
    Some(Generate::Client)
}

impl Default for ModuleCompileOptions {
    fn default() -> Self {
        Self {
            dev: false,
            generate: default_generate(),
            filename: None,
            root_dir: None,
            experimental: ExperimentalOptions::default(),
        }
    }
}

/// `compilerOptions.generate`: `"client" | "server" | false`. We model `false`
/// as `Option::None` in `ModuleCompileOptions.generate`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Generate {
    Client,
    Server,
}

/// `compilerOptions.experimental`. Added in Svelte 5.36. Mirrors the upstream
/// shape exactly — see `validate-options.js:46-48`. Default is all-false.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExperimentalOptions {
    /// Allow `await` in deriveds, template expressions, and the top level of
    /// components. `https://svelte.dev/e/experimental_async` fires when async
    /// is used without this flag.
    #[serde(default, rename = "async")]
    pub async_: bool,
}

/// `CompileOptions` — extends `ModuleCompileOptions` with component-level
/// settings. Modelled as composition (`module: ModuleCompileOptions`) rather
/// than inheritance to match Rust idiom.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompileOptions {
    #[serde(flatten)]
    pub module: ModuleCompileOptions,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "customElement")]
    pub custom_element: bool,
    #[serde(default)]
    pub accessors: bool,
    #[serde(default = "default_namespace", skip)]
    pub namespace: Namespace,
    #[serde(default)]
    pub immutable: bool,
    #[serde(default = "default_css")]
    pub css: CssMode,
    #[serde(default, rename = "preserveComments")]
    pub preserve_comments: bool,
    #[serde(default, rename = "preserveWhitespace")]
    pub preserve_whitespace: bool,
    #[serde(default = "default_fragments")]
    pub fragments: FragmentsStrategy,
    /// `compilerOptions.runes`: `boolean | undefined`. `None` means "infer
    /// from component code"; `Some(true)` forces runes mode; `Some(false)`
    /// forces legacy mode. (The JS signature also allows a callback shape;
    /// we resolve that to a boolean before construction.)
    #[serde(default)]
    pub runes: Option<bool>,
    #[serde(default = "default_disclose_version", rename = "discloseVersion")]
    pub disclose_version: bool,
    #[serde(default, rename = "modernAst")]
    pub modern_ast: bool,
    /// Pre-resolved scoped CSS hash. The JS bridge calls `cssHash({ hash, css,
    /// name, filename })` and passes the resulting string here as `cssHash`.
    #[serde(default, rename = "cssHash")]
    pub css_hash: Option<String>,
}

fn default_namespace() -> Namespace {
    Namespace::Html
}
fn default_css() -> CssMode {
    CssMode::External
}
fn default_fragments() -> FragmentsStrategy {
    FragmentsStrategy::Html
}
fn default_disclose_version() -> bool {
    true
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            module: ModuleCompileOptions::default(),
            name: None,
            custom_element: false,
            accessors: false,
            namespace: default_namespace(),
            immutable: false,
            css: default_css(),
            preserve_comments: false,
            preserve_whitespace: false,
            fragments: default_fragments(),
            runes: None,
            disclose_version: default_disclose_version(),
            modern_ast: false,
            css_hash: None,
        }
    }
}

/// `compilerOptions.css`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CssMode {
    Injected,
    External,
}

/// `compilerOptions.fragments` — DOM-fragment cloning strategy.
/// Added in Svelte 5.33.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FragmentsStrategy {
    Html,
    Tree,
}

/// Derive the exported component name from [`CompileOptions`], matching upstream
/// `get_component_name(options.filename)` with `options.name` override.
pub fn derive_component_name(options: &CompileOptions) -> String {
    let raw = if let Some(ref name) = options.name {
        name.clone()
    } else {
        derive_component_name_from_filename(options.module.filename.as_deref().unwrap_or("(unknown)"))
    };
    sanitize_export_name(&raw)
}

/// Mirrors upstream `module.scope.generate(preferred_name)` name sanitization
/// (before uniquification against bindings).
pub fn sanitize_export_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_digit())
    {
        out.insert(0, '_');
    }
    if out.is_empty() {
        "Component".to_string()
    } else {
        out
    }
}

/// Filename → component name (upstream `phases/2-analyze/index.js:get_component_name`).
pub fn derive_component_name_from_filename(filename: &str) -> String {
    let path = Path::new(filename);
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let parent_dir = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str());
    let mut stem = basename.strip_suffix(".svelte").unwrap_or(basename);
    if stem == "index" {
        if let Some(dir) = parent_dir {
            if dir != "src" {
                stem = dir;
            }
        }
    }
    let mut chars = stem.chars();
    match chars.next() {
        None => "Component".to_string(),
        Some(first) => {
            let mut out = String::new();
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
            out
        }
    }
}

fn deserialize_generate_option<'de, D>(deserializer: D) -> Result<Option<Generate>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match value {
        None => Ok(default_generate()),
        Some(serde_json::Value::Bool(false)) => Ok(None),
        Some(serde_json::Value::Bool(true)) => Err(D::Error::custom(
            "generate: true is invalid; use \"client\" or \"server\"",
        )),
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "client" | "dom" => Ok(Some(Generate::Client)),
            "server" | "ssr" => Ok(Some(Generate::Server)),
            other => Err(D::Error::custom(format!(
                "invalid generate option: {other:?}"
            ))),
        },
        Some(other) => Err(D::Error::custom(format!(
            "invalid generate option: {other}"
        ))),
    }
}

/// Options for `parse(source, options)`.
///
/// Mirrors the JS signature in `index.js:117`:
/// `parse(source, { modern, loose, filename, rootDir }?)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseOptions {
    #[serde(default)]
    pub modern: bool,
    #[serde(default)]
    pub loose: bool,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default, rename = "rootDir")]
    pub root_dir: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_name_from_index_in_fixture_dir() {
        let mut opts = CompileOptions::default();
        opts.module.filename = Some("samples/hello-world/index.svelte".into());
        assert_eq!(derive_component_name(&opts), "Hello_world");
    }

    #[test]
    fn derive_name_explicit_override() {
        let mut opts = CompileOptions::default();
        opts.name = Some("Custom".into());
        opts.module.filename = Some("index.svelte".into());
        assert_eq!(derive_component_name(&opts), "Custom");
    }

    #[test]
    fn deserialize_generate_false() {
        let opts: CompileOptions =
            serde_json::from_str(r#"{"generate":false,"filename":"x.svelte"}"#).unwrap();
        assert!(opts.module.generate.is_none());
    }
}
