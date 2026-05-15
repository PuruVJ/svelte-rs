//! CSS AST.
//!
//! Ported from `packages/svelte/src/compiler/types/css.d.ts`.
//!
//! Field names match upstream's wire format. Container enums are
//! `#[serde(untagged)]` because every variant struct already carries its own
//! `type` discriminator via a `kind` field renamed to `"type"`.

use serde::{Deserialize, Serialize};

use crate::fragment::Comment;
use crate::position::Offset;

/// Top-level `<style>` block. Mirrors `CSS.StyleSheet` in css.d.ts:17-27.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleSheet {
    #[serde(rename = "type")]
    pub kind: StyleSheetKind,
    pub start: Offset,
    pub end: Offset,
    pub attributes: Vec<crate::ElementAttribute>,
    pub children: Vec<StyleSheetChild>,
    pub content: StyleSheetContent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleSheetKind {
    StyleSheet,
}

/// `content` field on `StyleSheet`. Carries the raw style text for source-
/// map purposes and a possible HTML comment that immediately precedes the
/// `<style>` element.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleSheetContent {
    pub start: Offset,
    pub end: Offset,
    pub styles: String,
    pub comment: Option<Comment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum StyleSheetChild {
    Rule(Rule),
    Atrule(Atrule),
}

/// `@media`, `@supports`, `@import`, etc. Mirrors `CSS.Atrule` (css.d.ts:29-34).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Atrule {
    #[serde(rename = "type")]
    pub kind: AtruleKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub prelude: String,
    pub block: Option<Block>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AtruleKind {
    Atrule,
}

/// A CSS rule, e.g. `.foo { color: red; }`. Mirrors `CSS.Rule`
/// (css.d.ts:36-53).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    #[serde(rename = "type")]
    pub kind: RuleKind,
    pub start: Offset,
    pub end: Offset,
    pub prelude: SelectorList,
    pub block: Block,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RuleKind {
    Rule,
}

/// `a, b, c` — comma-separated selectors. Mirrors `CSS.SelectorList`
/// (css.d.ts:58-64).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelectorList {
    #[serde(rename = "type")]
    pub kind: SelectorListKind,
    pub start: Offset,
    pub end: Offset,
    pub children: Vec<ComplexSelector>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SelectorListKind {
    SelectorList,
}

/// `a b c` — descendant chain. Mirrors `CSS.ComplexSelector` (css.d.ts:69-82).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComplexSelector {
    #[serde(rename = "type")]
    pub kind: ComplexSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub children: Vec<RelativeSelector>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ComplexSelectorKind {
    ComplexSelector,
}

/// A relative selector — combinator + simple-selector list.
/// Mirrors `CSS.RelativeSelector` (css.d.ts:87-110).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelativeSelector {
    #[serde(rename = "type")]
    pub kind: RelativeSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub combinator: Option<Combinator>,
    pub selectors: Vec<SimpleSelector>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RelativeSelectorKind {
    RelativeSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SimpleSelector {
    TypeSelector(TypeSelector),
    IdSelector(IdSelector),
    ClassSelector(ClassSelector),
    AttributeSelector(AttributeSelector),
    PseudoElementSelector(PseudoElementSelector),
    PseudoClassSelector(PseudoClassSelector),
    Percentage(Percentage),
    Nth(Nth),
    NestingSelector(NestingSelector),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypeSelector {
    #[serde(rename = "type")]
    pub kind: TypeSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TypeSelectorKind {
    TypeSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IdSelector {
    #[serde(rename = "type")]
    pub kind: IdSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdSelectorKind {
    IdSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClassSelector {
    #[serde(rename = "type")]
    pub kind: ClassSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClassSelectorKind {
    ClassSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttributeSelector {
    #[serde(rename = "type")]
    pub kind: AttributeSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub matcher: Option<String>,
    pub value: Option<String>,
    pub flags: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttributeSelectorKind {
    AttributeSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PseudoElementSelector {
    #[serde(rename = "type")]
    pub kind: PseudoElementSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PseudoElementSelectorKind {
    PseudoElementSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PseudoClassSelector {
    #[serde(rename = "type")]
    pub kind: PseudoClassSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub args: Option<SelectorList>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PseudoClassSelectorKind {
    PseudoClassSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Percentage {
    #[serde(rename = "type")]
    pub kind: PercentageKind,
    pub start: Offset,
    pub end: Offset,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PercentageKind {
    Percentage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Nth {
    #[serde(rename = "type")]
    pub kind: NthKind,
    pub start: Offset,
    pub end: Offset,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum NthKind {
    Nth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NestingSelector {
    #[serde(rename = "type")]
    pub kind: NestingSelectorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: NestingSelectorName,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum NestingSelectorKind {
    NestingSelector,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum NestingSelectorName {
    #[serde(rename = "&")]
    Ampersand,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Combinator {
    #[serde(rename = "type")]
    pub kind: CombinatorKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CombinatorKind {
    Combinator,
}

/// `{ ... }` — rule body. Mirrors `CSS.Block` (css.d.ts:177-180).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Block {
    #[serde(rename = "type")]
    pub kind: BlockKind,
    pub start: Offset,
    pub end: Offset,
    pub children: Vec<BlockChild>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BlockKind {
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum BlockChild {
    Declaration(Declaration),
    Rule(Rule),
    Atrule(Atrule),
}

/// `property: value` — single declaration. Mirrors `CSS.Declaration`
/// (css.d.ts:182-186).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Declaration {
    #[serde(rename = "type")]
    pub kind: DeclarationKind,
    pub start: Offset,
    pub end: Offset,
    pub property: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeclarationKind {
    Declaration,
}
