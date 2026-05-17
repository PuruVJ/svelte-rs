//! `{#each}`, `{#if}`, `{#await}`, `{#key}`, `{#snippet}` blocks.

use svelte_js_ast::{Expression, Identifier, Pattern};

use crate::fragment::Fragment;
use crate::position::Offset;

#[derive(Debug, Clone, PartialEq)]
pub struct EachBlock {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
    pub context: Option<Pattern>,
    pub body: Fragment,
    pub fallback: Option<Fragment>,
    pub index: Option<String>,
    pub key: Option<Expression>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IfBlock {
    pub start: Offset,
    pub end: Offset,
    pub elseif: bool,
    pub test: Expression,
    pub consequent: Fragment,
    pub alternate: Option<Fragment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AwaitBlock {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
    pub value: Option<Pattern>,
    pub error: Option<Pattern>,
    pub pending: Option<Fragment>,
    pub then: Option<Fragment>,
    pub catch_: Option<Fragment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyBlock {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnippetBlock {
    pub start: Offset,
    pub end: Offset,
    pub expression: Identifier,
    pub parameters: Vec<Pattern>,
    pub type_params: Option<String>,
    pub body: Fragment,
}
