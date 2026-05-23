//! Element-like nodes.

use svelte_js_ast::Expression;

use crate::attributes::ElementAttribute;
use crate::fragment::Fragment;
use crate::position::{Offset, SourceLocation};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentMetadata {
    pub dynamic: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
    pub metadata: ComponentMetadata,
}

/// Compile-time metadata on elements (set by `mark_template_metadata`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElementMetadata {
    pub dynamic: bool,
    /// Set when the element and subtree are fully static for client hydration (mirrors upstream `is_static_element`).
    pub is_static_element: bool,
    /// Pre-serialized outer HTML for static elements (filled by client `precompute_static_html_cache`).
    pub cached_static_html: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegularElement {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
    pub metadata: ElementMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlotElement {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TitleElement {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

/// `svelte:body`, `svelte:head`, `svelte:document`, `svelte:window`,
/// `svelte:boundary`, `svelte:fragment`, `svelte:self`, `svelte:options`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecialElement {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

pub type SvelteBody = SpecialElement;
pub type SvelteBoundary = SpecialElement;
pub type SvelteDocument = SpecialElement;
pub type SvelteFragment = SpecialElement;
pub type SvelteHead = SpecialElement;
pub type SvelteWindow = SpecialElement;
pub type SvelteSelf = SpecialElement;
pub type SvelteOptionsRaw = SpecialElement;

/// `<svelte:component this={Expr}>`.
#[derive(Debug, Clone, PartialEq)]
pub struct SvelteComponent {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
    pub expression: Expression,
}

/// `<svelte:element this={Expr}>`.
#[derive(Debug, Clone, PartialEq)]
pub struct SvelteElement {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
    pub tag: Expression,
}
