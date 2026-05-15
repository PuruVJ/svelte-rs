//! `Fragment` and its children.
//!
//! Ported from `packages/svelte/src/compiler/types/template.d.ts:37-148`.
//! Fragment.metadata is intentionally absent here — `to_public_ast`
//! (`packages/svelte/src/compiler/index.js:153`) strips it before
//! `parse()` returns.

use serde::{Deserialize, Serialize};

use crate::attributes::Attribute;
use crate::blocks::{AwaitBlock, EachBlock, IfBlock, KeyBlock, SnippetBlock};
use crate::elements::{
    Component, RegularElement, SlotElement, SvelteBody, SvelteBoundary, SvelteComponent,
    SvelteDocument, SvelteElement, SvelteFragment, SvelteHead, SvelteOptionsRaw, SvelteSelf,
    SvelteWindow, TitleElement,
};
use crate::position::Offset;
use crate::tags::{AttachTag, ConstTag, DebugTag, ExpressionTag, HtmlTag, RenderTag};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fragment {
    #[serde(rename = "type")]
    pub kind: FragmentKind,
    pub nodes: Vec<FragmentChild>,
}

impl Fragment {
    pub fn empty() -> Self {
        Self {
            kind: FragmentKind::Fragment,
            nodes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FragmentKind {
    Fragment,
}

/// Children of a Fragment.
///
/// Mirrors `template.d.ts:39` —
/// `Array<Text | Tag | ElementLike | Block | Comment>`.
///
/// Untagged: every variant struct carries its own `type` discriminator field.
/// The deserializer dispatches by matching the variant whose `kind` singleton
/// accepts the wire `type` string. This mirrors the JS data model (every node
/// has `type` on itself) and lets the same structs be used in both tagged-
/// enum contexts and "naked" Vec contexts without losing the discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FragmentChild {
    // Atoms.
    Text(Text),
    Comment(Comment),
    // Tags.
    AttachTag(AttachTag),
    ConstTag(ConstTag),
    DebugTag(DebugTag),
    ExpressionTag(ExpressionTag),
    HtmlTag(HtmlTag),
    RenderTag(RenderTag),
    // Elements.
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
    // Blocks.
    AwaitBlock(AwaitBlock),
    EachBlock(EachBlock),
    IfBlock(IfBlock),
    KeyBlock(KeyBlock),
    SnippetBlock(SnippetBlock),
}

/// Static text (`template.d.ts:111-117`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Text {
    #[serde(rename = "type")]
    pub kind: TextKind,
    pub start: Offset,
    pub end: Offset,
    pub raw: String,
    pub data: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TextKind {
    Text,
}

/// HTML comment (`template.d.ts:143-147`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Comment {
    #[serde(rename = "type")]
    pub kind: CommentKind,
    pub start: Offset,
    pub end: Offset,
    pub data: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CommentKind {
    Comment,
}

/// Attributes that appear on `<script>` and `<style>` elements
/// (`template.d.ts:572` — `attributes: Attribute[]`).
pub type ScriptAttributes = Vec<Attribute>;
