//! `Fragment` and its children.

use crate::blocks::{AwaitBlock, EachBlock, IfBlock, KeyBlock, SnippetBlock};
use crate::elements::{
    Component, RegularElement, SlotElement, SvelteBody, SvelteBoundary, SvelteComponent,
    SvelteDocument, SvelteElement, SvelteFragment, SvelteHead, SvelteOptionsRaw, SvelteSelf,
    SvelteWindow, TitleElement,
};
use crate::position::Offset;
use crate::tags::{AttachTag, ConstTag, DebugTag, ExpressionTag, HtmlTag, RenderTag};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fragment {
    pub nodes: Vec<FragmentChild>,
}

impl Fragment {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FragmentChild {
    Text(Text),
    Comment(Comment),
    AttachTag(AttachTag),
    ConstTag(ConstTag),
    DebugTag(DebugTag),
    ExpressionTag(ExpressionTag),
    HtmlTag(HtmlTag),
    RenderTag(RenderTag),
    Component(Component),
    RegularElement(RegularElement),
    SlotElement(SlotElement),
    TitleElement(TitleElement),
    SvelteBody(SvelteBody),
    SvelteBoundary(SvelteBoundary),
    SvelteComponent(SvelteComponent),
    SvelteDocument(SvelteDocument),
    SvelteElement(SvelteElement),
    SvelteFragment(SvelteFragment),
    SvelteHead(SvelteHead),
    SvelteOptions(SvelteOptionsRaw),
    SvelteSelf(SvelteSelf),
    SvelteWindow(SvelteWindow),
    AwaitBlock(AwaitBlock),
    EachBlock(EachBlock),
    IfBlock(IfBlock),
    KeyBlock(KeyBlock),
    SnippetBlock(SnippetBlock),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub start: Offset,
    pub end: Offset,
    pub raw: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub start: Offset,
    pub end: Offset,
    pub data: String,
}

pub type ScriptAttributes = Vec<crate::attributes::Attribute>;
