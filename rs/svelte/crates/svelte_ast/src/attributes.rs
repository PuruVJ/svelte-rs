//! Attribute and directive shapes that appear inside element opening tags.
//!
//! Ported from `packages/svelte/src/compiler/types/template.d.ts:196-557`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fragment::Text;
use crate::position::{Offset, SourceLocation};
use crate::tags::{AttachTag, ExpressionTag};

/// Anything that can appear inside an opening tag — the union of attributes,
/// spread attributes, all directive kinds, and `{@attach}` tags
/// (`template.d.ts:319`). Untagged because every variant struct already
/// carries its own `type` discriminator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ElementAttribute {
    Attribute(Attribute),
    SpreadAttribute(SpreadAttribute),
    AnimateDirective(AnimateDirective),
    BindDirective(BindDirective),
    ClassDirective(ClassDirective),
    LetDirective(LetDirective),
    OnDirective(OnDirective),
    StyleDirective(StyleDirective),
    TransitionDirective(TransitionDirective),
    UseDirective(UseDirective),
    AttachTag(AttachTag),
}

/// `name="value"` or `name={expr}` or `name`. (`template.d.ts:544-557`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attribute {
    #[serde(rename = "type")]
    pub kind: AttributeKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub value: AttributeValue,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttributeKind {
    Attribute,
}

/// `template.d.ts:549`: `value: true | ExpressionTag | Array<Text | ExpressionTag>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AttributeValue {
    /// Bare attribute (e.g. `disabled`) — always serializes as `true`.
    Empty(bool),
    /// Single `{expr}` interpolation (e.g. `name={x}`).
    Single(ExpressionTag),
    /// Mixed string + interpolations (e.g. `class="a {b} c"`).
    Many(Vec<AttributeValuePart>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AttributeValuePart {
    Text(Text),
    ExpressionTag(ExpressionTag),
}

impl AttributeValuePart {
    pub fn start_pos(&self) -> Offset {
        match self {
            AttributeValuePart::Text(t) => t.start,
            AttributeValuePart::ExpressionTag(e) => e.start,
        }
    }
}

/// `{...rest}` (`template.d.ts:559-566`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpreadAttribute {
    #[serde(rename = "type")]
    pub kind: SpreadAttributeKind,
    pub start: Offset,
    pub end: Offset,
    pub expression: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SpreadAttributeKind {
    SpreadAttribute,
}

// --- Directives ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnimateDirective {
    #[serde(rename = "type")]
    pub kind: AnimateDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Value>,
    /// The parser unconditionally writes `directive.modifiers = modifiers` for
    /// every non-style directive (`phases/1-parse/state/element.js`), so the
    /// wire format always carries the field even when it is empty.
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnimateDirectiveKind {
    AnimateDirective,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BindDirective {
    #[serde(rename = "type")]
    pub kind: BindDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Value,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BindDirectiveKind {
    BindDirective,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClassDirective {
    #[serde(rename = "type")]
    pub kind: ClassDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    /// Despite `template.d.ts:228` typing this as the literal `"class"`,
    /// the parser emits the part AFTER the colon (e.g. `"foo"` for
    /// `class:foo={isFoo}`).
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Value,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClassDirectiveKind {
    ClassDirective,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LetDirective {
    #[serde(rename = "type")]
    pub kind: LetDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Value>,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LetDirectiveKind {
    LetDirective,
}

/// `on:eventName|modifier1|modifier2` — legacy event directive.
///
/// `modifiers` is `Vec<String>` (not a typed enum) because
/// `phases/1-parse/state/element.js` writes `directive.modifiers =
/// modifiers` from a `.split('|')` and the modifier strings are validated
/// later in phase 2; the wire format may contain any string here, not just
/// the canonical set (`capture`, `passive`, `once`, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OnDirective {
    #[serde(rename = "type")]
    pub kind: OnDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Value>,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OnDirectiveKind {
    OnDirective,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StyleDirective {
    #[serde(rename = "type")]
    pub kind: StyleDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    /// `template.d.ts:276`: `value: true | ExpressionTag | Array<ExpressionTag | Text>`.
    pub value: AttributeValue,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StyleDirectiveKind {
    StyleDirective,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransitionDirective {
    #[serde(rename = "type")]
    pub kind: TransitionDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Value>,
    pub modifiers: Vec<String>,
    pub intro: bool,
    pub outro: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TransitionDirectiveKind {
    TransitionDirective,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UseDirective {
    #[serde(rename = "type")]
    pub kind: UseDirectiveKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Value>,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum UseDirectiveKind {
    UseDirective,
}
