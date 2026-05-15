//! `{...}` template tags.
//!
//! Ported from `packages/svelte/src/compiler/types/template.d.ts:120-194`.
//! Tag metadata (`expression: ExpressionMetadata`, `is_controlled`,
//! `arguments`, `path`, `snippets`, etc.) is `@internal` and stripped by
//! `to_public_ast` before serialization, so it does not appear here.
//!
//! Every struct carries its own `type` discriminator field so it can be
//! serialized in untagged-enum contexts (such as `AttributeValue::Single`)
//! and still round-trip with the JS wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::position::Offset;

/// `{expression}` — a possibly-reactive template expression.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExpressionTag {
    #[serde(rename = "type")]
    pub kind: ExpressionTagKind,
    pub start: Offset,
    pub end: Offset,
    /// ESTree `Expression`. Modelled as opaque JSON until the OXC adapter
    /// produces estree-shaped output.
    pub expression: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExpressionTagKind {
    ExpressionTag,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HtmlTag {
    #[serde(rename = "type")]
    pub kind: HtmlTagKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum HtmlTagKind {
    HtmlTag,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConstTag {
    #[serde(rename = "type")]
    pub kind: ConstTagKind,
    pub start: Offset,
    pub end: Offset,
    /// ESTree `VariableDeclaration`.
    pub declaration: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConstTagKind {
    ConstTag,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DebugTag {
    #[serde(rename = "type")]
    pub kind: DebugTagKind,
    pub start: Offset,
    pub end: Offset,
    pub identifiers: Vec<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DebugTagKind {
    DebugTag,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderTag {
    #[serde(rename = "type")]
    pub kind: RenderTagKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RenderTagKind {
    RenderTag,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachTag {
    #[serde(rename = "type")]
    pub kind: AttachTagKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttachTagKind {
    AttachTag,
}
