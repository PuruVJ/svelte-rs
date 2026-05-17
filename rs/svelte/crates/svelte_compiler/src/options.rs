//! Compile option types.
//!
//! Ported from `packages/svelte/src/compiler/types/index.d.ts:ModuleCompileOptions`
//! and `CompileOptions`. Defaults are taken from
//! `packages/svelte/src/compiler/validate-options.js`. Defer the actual
//! validation (which depends on diagnostics + the dispatch shape of the
//! upstream validator) to Phase 2.

use serde::{Deserialize, Serialize};

use svelte_ast::Namespace;

/// `ModuleCompileOptions` — applies to both `compile` and `compileModule`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleCompileOptions {
    #[serde(default)]
    pub dev: bool,
    #[serde(default = "default_generate")]
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
