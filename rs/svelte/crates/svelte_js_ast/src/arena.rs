//! Bump arena helpers for typed JS AST nodes.
//!
//! JS programs produced by parse and transform allocate into the compile bump
//! alongside template AST data so a single drop frees the whole compile.

use std::borrow::Cow;

use bumpalo::Bump;

/// Per-parse / per-compile bump allocator for typed JS AST nodes.
pub struct JsArena<'a> {
    pub bump: &'a Bump,
}

impl<'a> JsArena<'a> {
    pub fn new(bump: &'a Bump) -> Self {
        Self { bump }
    }

    pub fn vec<T>(&self) -> bumpalo::collections::Vec<'a, T> {
        bumpalo::collections::Vec::new_in(self.bump)
    }

    pub fn boxed<T>(&self, value: T) -> bumpalo::boxed::Box<'a, T> {
        bumpalo::boxed::Box::new_in(value, self.bump)
    }

    pub fn alloc_str(&self, s: &str) -> &'a str {
        self.bump.alloc_str(s)
    }

    pub fn alloc_string(&self, s: &str) -> bumpalo::collections::String<'a> {
        bumpalo::collections::String::from_str_in(s, self.bump)
    }

    pub fn cow_str(&self, s: &str) -> Cow<'a, str> {
        Cow::Borrowed(self.alloc_str(s))
    }

    pub fn cow_static(&self, s: &'static str) -> Cow<'a, str> {
        Cow::Borrowed(s)
    }
}
