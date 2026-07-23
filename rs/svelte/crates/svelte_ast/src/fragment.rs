//! `Fragment` and its children — allocated in a [`TemplateArena`](crate::arena::TemplateArena).

use bumpalo::collections::Vec as BumpVec;

use crate::blocks::{AwaitBlock, EachBlock, IfBlock, KeyBlock, SnippetBlock};
use crate::elements::{
    Component, RegularElement, SlotElement, SvelteBody, SvelteBoundary, SvelteComponent,
    SvelteDocument, SvelteElement, SvelteFragment, SvelteHead, SvelteOptionsRaw, SvelteSelf,
    SvelteWindow, TitleElement,
};
use crate::position::Offset;
use crate::tags::{AttachTag, ConstTag, DebugTag, ExpressionTag, HtmlTag, RenderTag};

/// Compile-time metadata on template fragments (set by `svelte_transform_shared::mark_template_metadata`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FragmentMetadata {
    /// True when the fragment or any descendant can affect runtime output beyond static HTML.
    pub dynamic: bool,
}

#[derive(Debug, PartialEq)]
pub struct Fragment<'a> {
    pub nodes: BumpVec<'a, FragmentChild<'a>>,
    pub metadata: FragmentMetadata,
}

impl<'a> Fragment<'a> {
    pub fn empty_in(bump: &'a bumpalo::Bump) -> Self {
        Self {
            nodes: BumpVec::new_in(bump),
            metadata: FragmentMetadata::default(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum FragmentChild<'a> {
    Text(Text<'a>),
    Comment(Comment<'a>),
    AttachTag(AttachTag),
    ConstTag(ConstTag),
    DebugTag(DebugTag),
    ExpressionTag(ExpressionTag),
    HtmlTag(HtmlTag),
    RenderTag(RenderTag),
    Component(bumpalo::boxed::Box<'a, Component<'a>>),
    RegularElement(RegularElement<'a>),
    SlotElement(SlotElement<'a>),
    TitleElement(TitleElement<'a>),
    SvelteBody(SvelteBody<'a>),
    SvelteBoundary(SvelteBoundary<'a>),
    SvelteComponent(bumpalo::boxed::Box<'a, SvelteComponent<'a>>),
    SvelteDocument(SvelteDocument<'a>),
    SvelteElement(bumpalo::boxed::Box<'a, SvelteElement<'a>>),
    SvelteFragment(SvelteFragment<'a>),
    SvelteHead(SvelteHead<'a>),
    SvelteOptions(SvelteOptionsRaw<'a>),
    SvelteSelf(SvelteSelf<'a>),
    SvelteWindow(SvelteWindow<'a>),
    AwaitBlock(bumpalo::boxed::Box<'a, AwaitBlock<'a>>),
    EachBlock(bumpalo::boxed::Box<'a, EachBlock<'a>>),
    IfBlock(bumpalo::boxed::Box<'a, IfBlock<'a>>),
    KeyBlock(KeyBlock<'a>),
    SnippetBlock(bumpalo::boxed::Box<'a, SnippetBlock<'a>>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Text<'a> {
    pub start: Offset,
    pub end: Offset,
    pub raw: &'a str,
    pub data: bumpalo::collections::String<'a>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Comment<'a> {
    pub start: Offset,
    pub end: Offset,
    pub data: &'a str,
}

pub type ScriptAttributes<'a> = BumpVec<'a, crate::attributes::Attribute<'a>>;
