//! Element-like nodes.
//!
//! Ported from `packages/svelte/src/compiler/types/template.d.ts:316-448`.
//! All elements share the `BaseElement` shape — `name, name_loc, attributes,
//! fragment` — plus a `type` discriminator and per-variant extras.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attributes::ElementAttribute;
use crate::fragment::Fragment;
use crate::position::{Offset, SourceLocation};

/// `<MyComponent ... />` — invocation of a component identifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Component {
    #[serde(rename = "type")]
    pub kind: ComponentKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ComponentKind {
    Component,
}

/// `<div>`, `<span>`, etc. The generic HTML/SVG/MathML element.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegularElement {
    #[serde(rename = "type")]
    pub kind: RegularElementKind,
    pub start: Offset,
    pub end: Offset,
    pub name: String,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RegularElementKind {
    RegularElement,
}

/// `<slot ...>` (legacy slot syntax).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlotElement {
    #[serde(rename = "type")]
    pub kind: SlotElementKind,
    pub start: Offset,
    pub end: Offset,
    pub name: SlotName,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SlotElementKind {
    SlotElement,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SlotName {
    Slot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TitleElement {
    #[serde(rename = "type")]
    pub kind: TitleElementKind,
    pub start: Offset,
    pub end: Offset,
    pub name: TitleName,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TitleElementKind {
    TitleElement,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TitleName {
    Title,
}

macro_rules! special_element {
    ($struct:ident, $kind_enum:ident, $name_enum:ident, $name_literal:literal) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
        pub struct $struct {
            #[serde(rename = "type")]
            pub kind: $kind_enum,
            pub start: Offset,
            pub end: Offset,
            pub name: $name_enum,
            pub name_loc: SourceLocation,
            pub attributes: Vec<ElementAttribute>,
            pub fragment: Fragment,
        }

        #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
        pub enum $kind_enum {
            $struct,
        }

        #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
        pub enum $name_enum {
            #[serde(rename = $name_literal)]
            Value,
        }
    };
}

special_element!(SvelteBody, SvelteBodyKind, SvelteBodyName, "svelte:body");
special_element!(
    SvelteBoundary,
    SvelteBoundaryKind,
    SvelteBoundaryName,
    "svelte:boundary"
);
special_element!(
    SvelteDocument,
    SvelteDocumentKind,
    SvelteDocumentName,
    "svelte:document"
);
special_element!(
    SvelteFragment,
    SvelteFragmentKind,
    SvelteFragmentName,
    "svelte:fragment"
);
special_element!(SvelteHead, SvelteHeadKind, SvelteHeadName, "svelte:head");
special_element!(
    SvelteWindow,
    SvelteWindowKind,
    SvelteWindowName,
    "svelte:window"
);

/// `<svelte:options>` — note that `type` is `"SvelteOptions"` (no `Raw` in the
/// wire format) but we keep the `Raw` suffix on the Rust type to disambiguate
/// from the hoisted `Root.options` (a different shape).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SvelteOptionsRaw {
    #[serde(rename = "type")]
    pub kind: SvelteOptionsRawKind,
    pub start: Offset,
    pub end: Offset,
    pub name: SvelteOptionsRawName,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteOptionsRawKind {
    SvelteOptions,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteOptionsRawName {
    #[serde(rename = "svelte:options")]
    Value,
}

/// `<svelte:component this={Expr}>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SvelteComponent {
    #[serde(rename = "type")]
    pub kind: SvelteComponentKind,
    pub start: Offset,
    pub end: Offset,
    pub name: SvelteComponentName,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
    pub expression: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteComponentKind {
    SvelteComponent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteComponentName {
    #[serde(rename = "svelte:component")]
    Value,
}

/// `<svelte:element this={Expr}>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SvelteElement {
    #[serde(rename = "type")]
    pub kind: SvelteElementKind,
    pub start: Offset,
    pub end: Offset,
    pub name: SvelteElementName,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
    pub tag: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteElementKind {
    SvelteElement,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteElementName {
    #[serde(rename = "svelte:element")]
    Value,
}

/// `<svelte:self>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SvelteSelf {
    #[serde(rename = "type")]
    pub kind: SvelteSelfKind,
    pub start: Offset,
    pub end: Offset,
    pub name: SvelteSelfName,
    pub name_loc: SourceLocation,
    pub attributes: Vec<ElementAttribute>,
    pub fragment: Fragment,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteSelfKind {
    SvelteSelf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SvelteSelfName {
    #[serde(rename = "svelte:self")]
    Value,
}
