//! `{#each}`, `{#if}`, `{#await}`, `{#key}`, `{#snippet}` blocks.
//!
//! Ported from `packages/svelte/src/compiler/types/template.d.ts:451-537`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fragment::Fragment;
use crate::position::Offset;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EachBlock {
    #[serde(rename = "type")]
    pub kind: EachBlockKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
    pub context: Option<Value>,
    pub body: Fragment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fragment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EachBlockKind {
    EachBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IfBlock {
    #[serde(rename = "type")]
    pub kind: IfBlockKind,
    pub start: Offset,
    pub end: Offset,
    pub elseif: bool,
    pub test: Value,
    pub consequent: Fragment,
    pub alternate: Option<Fragment>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum IfBlockKind {
    IfBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AwaitBlock {
    #[serde(rename = "type")]
    pub kind: AwaitBlockKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
    pub value: Option<Value>,
    pub error: Option<Value>,
    pub pending: Option<Fragment>,
    pub then: Option<Fragment>,
    #[serde(rename = "catch")]
    pub catch_: Option<Fragment>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AwaitBlockKind {
    AwaitBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyBlock {
    #[serde(rename = "type")]
    pub kind: KeyBlockKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum KeyBlockKind {
    KeyBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnippetBlock {
    #[serde(rename = "type")]
    pub kind: SnippetBlockKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
    pub parameters: Vec<Value>,
    #[serde(rename = "typeParams", skip_serializing_if = "Option::is_none")]
    pub type_params: Option<String>,
    pub body: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnippetBlockKind {
    SnippetBlock,
}
