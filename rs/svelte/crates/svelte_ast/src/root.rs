//! `Root`, `Script`, `SvelteOptions` (post-hoist), and `JsComment`.
//!
//! Root field order is taken from the live parser at
//! `packages/svelte/src/compiler/phases/1-parse/index.js:106-120`.
//! Serde emits in field-declaration order, which keeps future byte-level
//! snapshot diffs clean even though the parser-modern test compares
//! order-insensitively.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attributes::Attribute;
use crate::fragment::Fragment;
use crate::position::{Offset, SourceLocation};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Root {
    pub css: Option<Value>, // CSS::StyleSheet — placeholder until svelte_css_parser lands.
    pub js: Vec<Value>,     // Wire field present in parser output; not in template.d.ts.
    pub start: Offset,
    pub end: Offset,
    #[serde(rename = "type")]
    pub kind: RootKind,
    pub fragment: Fragment,
    pub options: Option<SvelteOptions>,
    pub comments: Vec<JsComment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<Script>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<Script>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RootKind {
    Root,
}

/// Hoisted `<svelte:options>` — populated from the SvelteOptionsRaw element by
/// `read_options` (`packages/svelte/src/compiler/phases/1-parse/read/options.js`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SvelteOptions {
    pub start: Offset,
    pub end: Offset,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessors: Option<bool>,
    #[serde(rename = "preserveWhitespace", skip_serializing_if = "Option::is_none")]
    pub preserve_whitespace: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<Namespace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub css: Option<SvelteOptionsCss>,
    #[serde(rename = "customElement", skip_serializing_if = "Option::is_none")]
    pub custom_element: Option<Value>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Namespace {
    Html,
    Svg,
    Mathml,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SvelteOptionsCss {
    Injected,
}

/// `<script>` element (`template.d.ts:568-573`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Script {
    #[serde(rename = "type")]
    pub kind: ScriptKind,
    pub start: Offset,
    pub end: Offset,
    pub context: ScriptContext,
    /// ESTree `Program` — opaque until OXC integration lands.
    pub content: Value,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScriptKind {
    Script,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScriptContext {
    Default,
    Module,
}

/// `template.d.ts:575-584` — JSComment nodes collected from inside `<script>`
/// and `{expressions}`. The wire shape is acorn's `onComment` callback shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsComment {
    #[serde(rename = "type")]
    pub kind: JsCommentKind,
    pub value: String,
    pub start: Offset,
    pub end: Offset,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum JsCommentKind {
    Line,
    Block,
}
