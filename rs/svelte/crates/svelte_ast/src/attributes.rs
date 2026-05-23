//! Attributes + directives that appear inside element opening tags.

use bumpalo::collections::Vec as BumpVec;

use svelte_js_ast::Expression;

use crate::fragment::{Text, };
use crate::position::{Offset, SourceLocation};
use crate::tags::{AttachTag, ExpressionTag};

#[derive(Debug, PartialEq)]
pub enum ElementAttribute<'a> {
    Attribute(Attribute<'a>),
    SpreadAttribute(SpreadAttribute),
    AnimateDirective(AnimateDirective<'a>),
    BindDirective(BindDirective<'a>),
    ClassDirective(ClassDirective<'a>),
    LetDirective(LetDirective<'a>),
    OnDirective(OnDirective<'a>),
    StyleDirective(StyleDirective<'a>),
    TransitionDirective(TransitionDirective<'a>),
    UseDirective(UseDirective<'a>),
    AttachTag(AttachTag),
}

#[derive(Debug, PartialEq)]
pub struct Attribute<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub value: AttributeValue<'a>,
}

#[derive(Debug, PartialEq)]
pub enum AttributeValue<'a> {
    Empty,
    Single(ExpressionTag),
    Many(BumpVec<'a, AttributeValuePart<'a>>),
}

#[derive(Debug, PartialEq)]
pub enum AttributeValuePart<'a> {
    Text(Text<'a>),
    ExpressionTag(ExpressionTag),
}

impl AttributeValuePart<'_> {
    pub fn start_pos(&self) -> Offset {
        match self {
            AttributeValuePart::Text(t) => t.start,
            AttributeValuePart::ExpressionTag(e) => e.start,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct SpreadAttribute {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
}

#[derive(Debug, PartialEq)]
pub struct AnimateDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: BumpVec<'a, &'a str>,
}

#[derive(Debug, PartialEq)]
pub struct BindDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub expression: Expression,
    pub modifiers: BumpVec<'a, &'a str>,
}

#[derive(Debug, PartialEq)]
pub struct ClassDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub expression: Expression,
    pub modifiers: BumpVec<'a, &'a str>,
}

#[derive(Debug, PartialEq)]
pub struct LetDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: BumpVec<'a, &'a str>,
}

#[derive(Debug, PartialEq)]
pub struct OnDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: BumpVec<'a, &'a str>,
}

#[derive(Debug, PartialEq)]
pub struct StyleDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub value: AttributeValue<'a>,
    pub modifiers: BumpVec<'a, &'a str>,
}

#[derive(Debug, PartialEq)]
pub struct TransitionDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: BumpVec<'a, &'a str>,
    pub intro: bool,
    pub outro: bool,
}

#[derive(Debug, PartialEq)]
pub struct UseDirective<'a> {
    pub start: Offset,
    pub end: Offset,
    pub name: &'a str,
    pub name_loc: Option<SourceLocation>,
    pub expression: Option<Expression>,
    pub modifiers: BumpVec<'a, &'a str>,
}
