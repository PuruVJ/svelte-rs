//! Attributes + directives that appear inside element opening tags.

use svelte_js_ast::Expression;

use crate::fragment::Text;
use crate::position::{Offset, SourceLocation};
use crate::tags::{AttachTag, ExpressionTag};

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub value: AttributeValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    /// Bare attribute (e.g. `disabled`).
    Empty,
    Single(ExpressionTag),
    Many(Vec<AttributeValuePart>),
}

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct SpreadAttribute {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnimateDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BindDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Expression,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Expression,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LetDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OnDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StyleDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub value: AttributeValue,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransitionDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: Vec<String>,
    pub intro: bool,
    pub outro: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UseDirective {
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: Vec<String>,
}
