//! Static data tables for CSS prune.
//!
//! Ported from
//! `packages/svelte/src/compiler/phases/2-analyze/css/css-prune.js:22-67`.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// HTML attributes whose enumerated values are case-insensitive per the
/// HTML spec. CSS attribute selectors match these case-insensitively in
/// HTML documents unless the selector explicitly opts in via the `s` flag.
pub fn case_insensitive_attributes() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "accept-charset",
            "autocapitalize",
            "autocomplete",
            "behavior",
            "charset",
            "crossorigin",
            "decoding",
            "dir",
            "direction",
            "draggable",
            "enctype",
            "enterkeyhint",
            "fetchpriority",
            "formenctype",
            "formmethod",
            "formtarget",
            "hidden",
            "http-equiv",
            "inputmode",
            "kind",
            "loading",
            "method",
            "preload",
            "referrerpolicy",
            "rel",
            "rev",
            "role",
            "rules",
            "scope",
            "shape",
            "spellcheck",
            "target",
            "translate",
            "type",
            "valign",
            "wrap",
        ]
        .into_iter()
        .collect()
    })
}

/// Elements whose state is exposed via plain attribute selectors. E.g.
/// `details[open]` works because `open` is reflected back from the
/// boolean DOM property. Mirrors `whitelist_attribute_selector` in
/// `css-prune.js`.
pub fn whitelist_attribute_selector() -> &'static HashMap<&'static str, &'static [&'static str]>
{
    static MAP: OnceLock<HashMap<&'static str, &'static [&'static str]>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert("details", &["open"][..]);
        m.insert("dialog", &["open"][..]);
        m
    })
}

/// Existence of an element in a particular traversal context. Mirrors
/// `NODE_PROBABLY_EXISTS` / `NODE_DEFINITELY_EXISTS` in css-prune.js:14-17.
///
/// - `Definite`: the element is guaranteed to render (e.g. a `<div>`
///   directly in a fragment).
/// - `Probable`: the element MIGHT render depending on runtime state
///   (e.g. inside an `{#if cond}` branch, or inside `{#each items}` with
///   `items` potentially empty).
///
/// The matcher uses this to decide when warnings can fire (only when a
/// selector matches nothing definite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Existence {
    Probable,
    #[default]
    Definite,
}

impl Existence {
    /// Combine two existence values: take the WEAKER (probable wins).
    pub fn min(a: Existence, b: Existence) -> Existence {
        match (a, b) {
            (Existence::Definite, Existence::Definite) => Existence::Definite,
            _ => Existence::Probable,
        }
    }
    /// Combine two existence values: take the STRONGER (definite wins).
    /// Mirrors `higher_existence` in css-prune.js:1190-1199.
    pub fn max(a: Existence, b: Existence) -> Existence {
        match (a, b) {
            (Existence::Definite, _) | (_, Existence::Definite) => Existence::Definite,
            _ => Existence::Probable,
        }
    }
}
