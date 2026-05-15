//! Position + span types.
//!
//! `Position` shape mirrors the union of acorn's `loc` (which has
//! `{ line, column }`) and locate-character's output (which adds
//! `character`). Both shapes appear in the wire output; `character` is
//! emitted only where the upstream parser populates it (e.g. `name_loc`).

use serde::{Deserialize, Serialize};

/// Character offset into the source string.
pub type Offset = u32;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceLocation {
    pub start: Position,
    pub end: Position,
}
