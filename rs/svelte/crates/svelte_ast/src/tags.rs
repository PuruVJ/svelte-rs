//! `{...}` template tags.

use svelte_js_ast::{Expression, Identifier, VariableDeclaration};

use crate::position::Offset;

#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionTag {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HtmlTag {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConstTag {
    pub start: Offset,
    pub end: Offset,
    pub declaration: VariableDeclaration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DebugTag {
    pub start: Offset,
    pub end: Offset,
    pub identifiers: Vec<Identifier>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderTag {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttachTag {
    pub start: Offset,
    pub end: Offset,
    pub expression: Expression,
}
