//! `Root`, `Script`, `SvelteOptions`, `JsComment`.

use svelte_js_ast::{Program, Statement};

use crate::attributes::Attribute;
use crate::fragment::Fragment;
use crate::position::{Offset, SourceLocation};

#[derive(Debug, Clone, PartialEq)]
pub struct Root {
    pub css: Option<crate::css::StyleSheet>,
    pub js: Vec<Statement>,
    pub start: Offset,
    pub end: Offset,
    pub fragment: Fragment,
    pub options: Option<SvelteOptions>,
    pub comments: Vec<JsComment>,
    pub instance: Option<Script>,
    pub module: Option<Script>,
    /// Soft diagnostics emitted during parsing (treated as warnings by
    /// analyze). Examples: `element_invalid_self_closing_tag`,
    /// `element_implicitly_closed`. Hard errors are surfaced via the
    /// Result return; these are recoverable.
    pub parse_warnings: Vec<svelte_diagnostics::CompileDiagnostic>,
}

impl Root {
    pub fn empty() -> Self {
        Self {
            css: None,
            js: Vec::new(),
            start: 0,
            end: 0,
            fragment: Fragment::empty(),
            options: None,
            comments: Vec::new(),
            instance: None,
            module: None,
            parse_warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SvelteOptions {
    pub start: Offset,
    pub end: Offset,
    pub runes: Option<bool>,
    pub immutable: Option<bool>,
    pub accessors: Option<bool>,
    pub preserve_whitespace: Option<bool>,
    pub namespace: Option<Namespace>,
    pub css: Option<SvelteOptionsCss>,
    pub custom_element: Option<CustomElementOpts>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CustomElementOpts {
    pub tag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Html,
    Svg,
    Mathml,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvelteOptionsCss {
    Injected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Script {
    pub start: Offset,
    pub end: Offset,
    pub context: ScriptContext,
    pub content: Program,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptContext {
    Default,
    Module,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsComment {
    pub kind: JsCommentKind,
    pub value: String,
    pub start: Offset,
    pub end: Offset,
    pub loc: SourceLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsCommentKind {
    Line,
    Block,
}
