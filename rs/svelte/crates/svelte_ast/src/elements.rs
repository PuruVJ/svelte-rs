//! Element-like nodes.

use bumpalo::collections::Vec as BumpVec;

use svelte_js_ast::Expression;

use crate::attributes::ElementAttribute;
use crate::fragment::Fragment;
use crate::position::{Offset, SourceLocation};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComponentMetadata {
    pub dynamic: bool,
}

#[derive(Debug, PartialEq)]
pub struct Component<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: SourceLocation,
    pub attributes: BumpVec<'a, ElementAttribute<'a>>,
    pub fragment: Fragment<'a>,
    pub metadata: ComponentMetadata,
}

/// Compile-time metadata on elements (set by `mark_template_metadata`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElementMetadata<'a> {
    pub dynamic: bool,
    pub is_static_element: bool,
    pub cached_static_html: Option<bumpalo::collections::String<'a>>,
}

#[derive(Debug, PartialEq)]
pub struct RegularElement<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: SourceLocation,
    pub attributes: BumpVec<'a, ElementAttribute<'a>>,
    pub fragment: Fragment<'a>,
    pub metadata: ElementMetadata<'a>,
}

#[derive(Debug, PartialEq)]
pub struct SlotElement<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: BumpVec<'a, ElementAttribute<'a>>,
    pub fragment: Fragment<'a>,
}

#[derive(Debug, PartialEq)]
pub struct TitleElement<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: BumpVec<'a, ElementAttribute<'a>>,
    pub fragment: Fragment<'a>,
}

#[derive(Debug, PartialEq)]
pub struct SpecialElement<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: BumpVec<'a, ElementAttribute<'a>>,
    pub fragment: Fragment<'a>,
}

pub type SvelteBody<'a> = SpecialElement<'a>;
pub type SvelteBoundary<'a> = SpecialElement<'a>;
pub type SvelteDocument<'a> = SpecialElement<'a>;
pub type SvelteFragment<'a> = SpecialElement<'a>;
pub type SvelteHead<'a> = SpecialElement<'a>;
pub type SvelteWindow<'a> = SpecialElement<'a>;
pub type SvelteSelf<'a> = SpecialElement<'a>;
pub type SvelteOptionsRaw<'a> = SpecialElement<'a>;

#[derive(Debug, PartialEq)]
pub struct SvelteComponent<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: BumpVec<'a, ElementAttribute<'a>>,
    pub fragment: Fragment<'a>,
    pub expression: Expression,
}

#[derive(Debug, PartialEq)]
pub struct SvelteElement<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name_loc: SourceLocation,
    pub attributes: BumpVec<'a, ElementAttribute<'a>>,
    pub fragment: Fragment<'a>,
    pub tag: Expression,
}
