//! `{#each}`, `{#if}`, `{#await}`, `{#key}`, `{#snippet}` blocks.

use bumpalo::collections::Vec as BumpVec;

use svelte_js_ast::{Expression, Identifier, Pattern};

use crate::fragment::Fragment;
use crate::position::Offset;

#[derive(Debug, PartialEq)]
pub struct EachBlock<'a> {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
    pub context: Option<Pattern>,
    pub body: Fragment<'a>,
    pub fallback: Option<Fragment<'a>>,
    pub index: Option<&'a str>,
    pub key: Option<Expression>,
}

#[derive(Debug, PartialEq)]
pub struct IfBlock<'a> {
    pub start: Offset,
    pub end: Offset,
    pub elseif: bool,
    pub test: Expression,
    pub consequent: Fragment<'a>,
    pub alternate: Option<Fragment<'a>>,
}

#[derive(Debug, PartialEq)]
pub struct AwaitBlock<'a> {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
    pub value: Option<Pattern>,
    pub error: Option<Pattern>,
    pub pending: Option<Fragment<'a>>,
    pub then: Option<Fragment<'a>>,
    pub catch_: Option<Fragment<'a>>,
}

#[derive(Debug, PartialEq)]
pub struct KeyBlock<'a> {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
    pub fragment: Fragment<'a>,
}

#[derive(Debug, PartialEq)]
pub struct SnippetBlock<'a> {
    pub start: Offset,
    pub end: Offset,
    pub expression: Identifier,
    pub parameters: BumpVec<'a, Pattern>,
    pub type_params: Option<&'a str>,
    pub body: Fragment<'a>,
}
