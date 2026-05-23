//! `Root`, `Script`, `SvelteOptions`, `JsComment`.

use bumpalo::collections::Vec as BumpVec;

use svelte_js_ast::{Program, Statement};

use crate::attributes::Attribute;
use crate::fragment::Fragment;
use crate::position::{Offset, SourceLocation};

#[derive(Debug, PartialEq)]
pub struct Root<'a> {
    pub css: Option<crate::css::StyleSheet<'a>>,
    pub js: Vec<Statement>,
    pub start: Offset,
    pub end: Offset,
    pub fragment: Fragment<'a>,
    pub options: Option<SvelteOptions<'a>>,
    pub comments: Vec<JsComment>,
    pub instance: Option<Script<'a>>,
    pub module: Option<Script<'a>>,
    pub parse_warnings: Vec<svelte_diagnostics::CompileDiagnostic>,
}

#[derive(Debug, PartialEq)]
pub struct SvelteOptions<'a> {
    pub start: Offset,
    pub end: Offset,
    pub runes: Option<bool>,
    pub immutable: Option<bool>,
    pub accessors: Option<bool>,
    pub preserve_whitespace: Option<bool>,
    pub namespace: Option<Namespace>,
    pub css: Option<SvelteOptionsCss>,
    pub custom_element: Option<CustomElementOpts<'a>>,
    pub attributes: BumpVec<'a, Attribute<'a>>,
}

#[derive(Debug, PartialEq)]
pub struct CustomElementOpts<'a> {
    pub tag: Option<&'a str>,
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

#[derive(Debug, PartialEq)]
pub struct Script<'a> {
    pub start: Offset,
    pub end: Offset,
    pub context: ScriptContext,
    pub content: Program<'a>,
    pub attributes: BumpVec<'a, Attribute<'a>>,
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
