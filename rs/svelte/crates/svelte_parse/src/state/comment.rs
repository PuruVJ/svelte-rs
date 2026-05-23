//! HTML comment reader.
//!
//! Comments are parsed inside `state/element.js` in upstream — that file
//! detects `<!--` and consumes through `-->`. We split it out here for
//! Phase 2 scaffolding clarity; once `state/element.js` is fully ported it
//! can call into this helper.

use svelte_ast::Comment;
use svelte_diagnostics::{errors, CompileDiagnostic};

use crate::parser::Parser;

/// Reads `<!-- ... -->`. Assumes the cursor sits on the leading `<!--`.
pub fn read_comment<'a, 'src>(
    parser: &mut Parser<'a, 'src>,
) -> Result<Comment<'a>, CompileDiagnostic> {
    let start = parser.index as u32;

    // Consume `<!--`.
    debug_assert!(parser.match_str("<!--"));
    parser.index += 4;

    let data_start = parser.index;
    // Find the closing `-->`.
    let rest = &parser.template[parser.index..];
    let end_rel = match rest.find("-->") {
        Some(i) => i,
        None => {
            return Err(errors::expected_token(
                Some((parser.index as u32, parser.index as u32)),
                "-->",
            ));
        }
    };

    parser.index += end_rel;
    let data = parser.alloc_str(&parser.template[data_start..parser.index]);
    parser.index += 3; // skip `-->`

    Ok(Comment {
        start,
        end: parser.index as u32,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bumpalo::Bump;

    #[test]
    fn reads_simple_comment() {
        let bump = Bump::new();
        let mut p = Parser::new(&bump, "<!-- hello -->after", false);
        let c = read_comment(&mut p).unwrap();
        assert_eq!(c.start, 0);
        assert_eq!(c.end, 14);
        assert_eq!(c.data, " hello ");
        assert_eq!(p.index, 14);
    }

    #[test]
    fn empty_comment() {
        let bump = Bump::new();
        let mut p = Parser::new(&bump, "<!---->", false);
        let c = read_comment(&mut p).unwrap();
        assert_eq!(c.data, "");
        assert_eq!(c.end, 7);
    }

    #[test]
    fn unterminated_comment_errors() {
        let bump = Bump::new();
        let mut p = Parser::new(&bump, "<!-- never closes", false);
        let err = read_comment(&mut p).expect_err("must error");
        assert_eq!(err.code, "expected_token");
    }
}
