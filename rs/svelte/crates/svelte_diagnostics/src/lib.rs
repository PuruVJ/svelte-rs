//! Diagnostic codes for the Svelte compiler.
//!
//! The bodies of `errors` and `warnings` are generated at build time from
//! `packages/svelte/messages/{compile,shared}-{errors,warnings}/*.md` —
//! the same source files consumed by
//! `packages/svelte/scripts/process-messages/index.js`. Both message text and
//! the trailing `https://svelte.dev/e/<code>` URL line are produced
//! byte-for-byte identical to the upstream JS output.

#![forbid(unsafe_code)]

use serde::Serialize;

/// Inclusive start, exclusive end offsets into the source string.
/// Matches the `[start, end]` pair the JS `CompileDiagnostic` carries.
pub type Span = (u32, u32);

/// A single error or warning emitted by the compiler.
///
/// Field shape matches the JS `CompileDiagnostic` class
/// (`packages/svelte/src/compiler/utils/compile_diagnostic.js`):
/// `{ code, message, position: [start, end] | undefined }`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompileDiagnostic {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Span>,
}

impl std::fmt::Display for CompileDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompileDiagnostic {}

pub mod errors {
    include!(concat!(env!("OUT_DIR"), "/errors.rs"));
}

pub mod warnings {
    include!(concat!(env!("OUT_DIR"), "/warnings.rs"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden: this string must match what
    /// `packages/svelte/src/compiler/errors.js`'s
    /// `options_invalid_value(null, "foo")` produces.
    #[test]
    fn options_invalid_value_message_matches_js() {
        let d = errors::options_invalid_value(None, "foo");
        assert_eq!(d.code, "options_invalid_value");
        assert_eq!(
            d.message,
            "Invalid compiler option: foo\nhttps://svelte.dev/e/options_invalid_value"
        );
        assert_eq!(d.position, None);
    }

    #[test]
    fn span_is_carried_through() {
        let d = errors::options_invalid_value(Some((10, 20)), "bar");
        assert_eq!(d.position, Some((10, 20)));
    }
}
