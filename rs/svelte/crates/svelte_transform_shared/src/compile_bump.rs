//! Per-`compile()` bump arena for scratch buffers (HTML slabs, direct JS emission).
//!
//! Full arena-owned AST is a follow-up; this scopes allocation to one compile call
//! and avoids repeated heap growth for large template strings.

use bumpalo::Bump;

/// Bump allocator scoped to a single `compile()` invocation.
pub struct CompileBump {
    pub bump: Bump,
}

impl CompileBump {
    pub fn new() -> Self {
        Self {
            bump: Bump::new(),
        }
    }

    /// Growable string backed by the arena (finalized with [`BumpString::into_owned`]).
    pub fn string(&self) -> BumpString<'_> {
        BumpString::new_in(&self.bump)
    }
}

/// Arena-backed string builder; copies out once at the end.
pub struct BumpString<'a> {
    inner: bumpalo::collections::String<'a>,
}

impl<'a> BumpString<'a> {
    pub fn new_in(bump: &'a Bump) -> Self {
        Self {
            inner: bumpalo::collections::String::new_in(bump),
        }
    }

    pub fn push_str(&mut self, s: &str) {
        self.inner.push_str(s);
    }

    pub fn push(&mut self, c: char) {
        self.inner.push(c);
    }

    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    pub fn into_owned(self) -> String {
        self.inner.to_string()
    }
}
