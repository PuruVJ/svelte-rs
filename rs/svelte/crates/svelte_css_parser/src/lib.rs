//! CSS parser for Svelte's `<style>` blocks.
//!
//! Hand-port of `packages/svelte/src/compiler/phases/1-parse/read/style.js`.
//!
//! Returns Svelte's `CSS.StyleSheet` AST shape. The parser is recursive-
//! descent and self-contained (no dependency on `svelte_parse`'s `Parser`).
//! Callers tell us where the `<style>` body starts in the full template;
//! we advance an internal cursor and return the StyleSheet plus the byte
//! offset where the close tag (`</style`) was found.

#![forbid(unsafe_code)]

use svelte_ast::css::*;
use svelte_ast::{Comment, ElementAttribute};
use svelte_diagnostics::{errors, CompileDiagnostic};

/// Read a `<style>...</style>` block. Caller positions `content_start` at the
/// first byte of the body (just after the opening `>`), and `start` at the
/// position of the leading `<` of `<style>`. Caller already parsed the
/// attributes.
///
/// Returns the parsed `StyleSheet` and the byte offset immediately AFTER the
/// closing `</style>` (i.e. where the next sibling starts).
pub fn read_style(
    template: &str,
    start: u32,
    content_start: usize,
    attributes: Vec<ElementAttribute>,
    preceding_comment: Option<Comment>,
) -> Result<(StyleSheet, usize), CompileDiagnostic> {
    let mut p = CssParser::new(template, content_start);
    let children = p.read_body(|p| {
        p.template[p.index..].starts_with("</style") || p.index >= p.template.len()
    })?;
    let content_end = p.index;

    p.eat("</style")?;
    // Consume any whitespace then `>`. The upstream uses `parser.read(/\s*>/y)`
    // which always succeeds (matches zero or more whitespace then `>`).
    p.allow_whitespace();
    if !p.try_eat(">") {
        return Err(errors::expected_token(
            Some((p.index as u32, p.index as u32)),
            ">",
        ));
    }
    let end = p.index;

    let styles = template[content_start..content_end].to_string();
    Ok((
        StyleSheet {
            kind: StyleSheetKind::StyleSheet,
            start,
            end: end as u32,
            attributes,
            children,
            content: StyleSheetContent {
                start: content_start as u32,
                end: content_end as u32,
                styles,
                comment: preceding_comment,
            },
        },
        end,
    ))
}

struct CssParser<'src> {
    template: &'src str,
    index: usize,
}

impl<'src> CssParser<'src> {
    fn new(template: &'src str, index: usize) -> Self {
        Self { template, index }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.template.as_bytes().get(self.index).copied()
    }

    fn matches(&self, s: &str) -> bool {
        self.template[self.index..].starts_with(s)
    }

    fn try_eat(&mut self, s: &str) -> bool {
        if self.matches(s) {
            self.index += s.len();
            true
        } else {
            false
        }
    }

    fn eat(&mut self, s: &str) -> Result<(), CompileDiagnostic> {
        if self.try_eat(s) {
            Ok(())
        } else {
            Err(errors::expected_token(
                Some((self.index as u32, self.index as u32)),
                s,
            ))
        }
    }

    fn allow_whitespace(&mut self) {
        let bytes = self.template.as_bytes();
        while self.index < bytes.len() {
            let b = bytes[self.index];
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                self.index += 1;
            } else {
                break;
            }
        }
    }

    /// Skip whitespace + `/* */` + `<!-- -->` comments. Mirrors
    /// `allow_comment_or_whitespace` in style.js:628-643.
    fn allow_comment_or_whitespace(&mut self) {
        loop {
            self.allow_whitespace();
            if self.matches("/*") {
                self.index += 2;
                while self.index < self.template.len() {
                    if self.matches("*/") {
                        self.index += 2;
                        break;
                    }
                    self.index += 1;
                }
                continue;
            }
            if self.matches("<!--") {
                self.index += 4;
                while self.index < self.template.len() {
                    if self.matches("-->") {
                        self.index += 3;
                        break;
                    }
                    self.index += 1;
                }
                continue;
            }
            break;
        }
    }

    fn read_body(
        &mut self,
        finished: impl Fn(&CssParser<'_>) -> bool,
    ) -> Result<Vec<StyleSheetChild>, CompileDiagnostic> {
        let mut children = Vec::new();
        loop {
            self.allow_comment_or_whitespace();
            if finished(self) {
                break;
            }
            if self.matches("@") {
                let a = self.read_at_rule()?;
                children.push(StyleSheetChild::Atrule(a));
            } else {
                let r = self.read_rule()?;
                children.push(StyleSheetChild::Rule(r));
            }
        }
        Ok(children)
    }

    fn read_at_rule(&mut self) -> Result<Atrule, CompileDiagnostic> {
        let start = self.index as u32;
        self.eat("@")?;
        let name = self.read_identifier()?;
        let prelude = self.read_value()?;

        let block = if self.matches("{") {
            Some(self.read_block()?)
        } else {
            self.eat(";")?;
            None
        };

        Ok(Atrule {
            kind: AtruleKind::Atrule,
            start,
            end: self.index as u32,
            name,
            prelude,
            block,
        })
    }

    fn read_rule(&mut self) -> Result<Rule, CompileDiagnostic> {
        let start = self.index as u32;
        let prelude = self.read_selector_list(false)?;
        let block = self.read_block()?;
        Ok(Rule {
            kind: RuleKind::Rule,
            start,
            end: self.index as u32,
            prelude,
            block,
        })
    }

    fn read_selector_list(
        &mut self,
        inside_pseudo_class: bool,
    ) -> Result<SelectorList, CompileDiagnostic> {
        let mut children = Vec::new();
        self.allow_comment_or_whitespace();
        let start = self.index as u32;
        loop {
            if self.index >= self.template.len() {
                return Err(errors::unexpected_eof(Some((
                    self.index as u32,
                    self.index as u32,
                ))));
            }
            children.push(self.read_selector(inside_pseudo_class)?);
            let end = self.index;
            self.allow_comment_or_whitespace();
            let terminator = if inside_pseudo_class { ")" } else { "{" };
            if self.matches(terminator) {
                return Ok(SelectorList {
                    kind: SelectorListKind::SelectorList,
                    start,
                    end: end as u32,
                    children,
                });
            }
            self.eat(",")?;
            self.allow_comment_or_whitespace();
        }
    }

    fn read_selector(
        &mut self,
        inside_pseudo_class: bool,
    ) -> Result<ComplexSelector, CompileDiagnostic> {
        let list_start = self.index as u32;
        let mut children: Vec<RelativeSelector> = Vec::new();
        let mut relative_selector = RelativeSelector {
            kind: RelativeSelectorKind::RelativeSelector,
            start: self.index as u32,
            end: 0,
            combinator: None,
            selectors: Vec::new(),
        };

        loop {
            if self.index >= self.template.len() {
                return Err(errors::unexpected_eof(Some((
                    self.index as u32,
                    self.index as u32,
                ))));
            }
            let start = self.index as u32;

            if self.try_eat("&") {
                relative_selector.selectors.push(SimpleSelector::NestingSelector(NestingSelector {
                    kind: NestingSelectorKind::NestingSelector,
                    start,
                    end: self.index as u32,
                    name: NestingSelectorName::Ampersand,
                }));
            } else if self.try_eat("*") {
                let mut name = "*".to_string();
                if self.try_eat("|") {
                    name = self.read_identifier()?;
                }
                relative_selector.selectors.push(SimpleSelector::TypeSelector(TypeSelector {
                    kind: TypeSelectorKind::TypeSelector,
                    start,
                    end: self.index as u32,
                    name,
                }));
            } else if self.try_eat("#") {
                let name = self.read_identifier()?;
                relative_selector.selectors.push(SimpleSelector::IdSelector(IdSelector {
                    kind: IdSelectorKind::IdSelector,
                    start,
                    end: self.index as u32,
                    name,
                }));
            } else if self.try_eat(".") {
                let name = self.read_identifier()?;
                relative_selector.selectors.push(SimpleSelector::ClassSelector(ClassSelector {
                    kind: ClassSelectorKind::ClassSelector,
                    start,
                    end: self.index as u32,
                    name,
                }));
            } else if self.try_eat("::") {
                let name = self.read_identifier()?;
                relative_selector.selectors.push(SimpleSelector::PseudoElementSelector(
                    PseudoElementSelector {
                        kind: PseudoElementSelectorKind::PseudoElementSelector,
                        start,
                        end: self.index as u32,
                        name,
                    },
                ));
                // Inside `::foo(...)` — we read the inner selector list for
                // validation but discard it (upstream does the same).
                if self.try_eat("(") {
                    let _ = self.read_selector_list(true)?;
                    self.eat(")")?;
                }
            } else if self.try_eat(":") {
                let name = self.read_identifier()?;
                let mut args = None;
                if self.try_eat("(") {
                    args = Some(self.read_selector_list(true)?);
                    self.eat(")")?;
                }
                relative_selector.selectors.push(SimpleSelector::PseudoClassSelector(
                    PseudoClassSelector {
                        kind: PseudoClassSelectorKind::PseudoClassSelector,
                        start,
                        end: self.index as u32,
                        name,
                        args,
                    },
                ));
            } else if self.try_eat("[") {
                self.allow_whitespace();
                let name = self.read_identifier()?;
                self.allow_whitespace();
                let matcher = self.read_attribute_matcher();
                let value = if matcher.is_some() {
                    self.allow_whitespace();
                    Some(self.read_attribute_value()?)
                } else {
                    None
                };
                self.allow_whitespace();
                let flags = self.read_attribute_flags();
                self.allow_whitespace();
                self.eat("]")?;
                relative_selector.selectors.push(SimpleSelector::AttributeSelector(
                    AttributeSelector {
                        kind: AttributeSelectorKind::AttributeSelector,
                        start,
                        end: self.index as u32,
                        name,
                        matcher,
                        value,
                        flags,
                    },
                ));
            } else if inside_pseudo_class && self.match_nth_of() {
                let value = self.read_nth_of();
                relative_selector.selectors.push(SimpleSelector::Nth(Nth {
                    kind: NthKind::Nth,
                    start,
                    end: self.index as u32,
                    value,
                }));
            } else if let Some(percentage) = self.try_read_percentage() {
                relative_selector.selectors.push(SimpleSelector::Percentage(Percentage {
                    kind: PercentageKind::Percentage,
                    start,
                    end: self.index as u32,
                    value: percentage,
                }));
            } else if !self.match_combinator() {
                let mut name = self.read_identifier()?;
                if self.try_eat("|") {
                    name = self.read_identifier()?;
                }
                relative_selector.selectors.push(SimpleSelector::TypeSelector(TypeSelector {
                    kind: TypeSelectorKind::TypeSelector,
                    start,
                    end: self.index as u32,
                    name,
                }));
            }

            let index = self.index;
            self.allow_comment_or_whitespace();

            let terminator = if inside_pseudo_class { ")" } else { "{" };
            if self.matches(",") || self.matches(terminator) {
                // Rewind whitespace, finalize current relative selector.
                self.index = index;
                relative_selector.end = index as u32;
                children.push(relative_selector);
                return Ok(ComplexSelector {
                    kind: ComplexSelectorKind::ComplexSelector,
                    start: list_start,
                    end: index as u32,
                    children,
                });
            }

            self.index = index;
            let combinator = self.read_combinator();
            if let Some(combinator) = combinator {
                if !relative_selector.selectors.is_empty() {
                    relative_selector.end = index as u32;
                    children.push(relative_selector);
                }
                relative_selector = RelativeSelector {
                    kind: RelativeSelectorKind::RelativeSelector,
                    start: combinator.start,
                    end: 0,
                    combinator: Some(combinator),
                    selectors: Vec::new(),
                };
                self.allow_whitespace();
                if self.matches(",") || self.matches(terminator) {
                    return Err(errors::css_selector_invalid(Some((
                        self.index as u32,
                        self.index as u32,
                    ))));
                }
            }
        }
    }

    fn read_combinator(&mut self) -> Option<Combinator> {
        let start = self.index;
        self.allow_whitespace();
        let index = self.index;
        // Match combinator chars: `+`, `~`, `>`, `||`.
        let bytes = self.template.as_bytes();
        let name = if index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    self.index += 1;
                    Some("+".to_string())
                }
                b'~' => {
                    self.index += 1;
                    Some("~".to_string())
                }
                b'>' => {
                    self.index += 1;
                    Some(">".to_string())
                }
                b'|' if bytes.get(index + 1) == Some(&b'|') => {
                    self.index += 2;
                    Some("||".to_string())
                }
                _ => None,
            }
        } else {
            None
        };

        if let Some(name) = name {
            let end = self.index;
            self.allow_whitespace();
            return Some(Combinator {
                kind: CombinatorKind::Combinator,
                name,
                start: index as u32,
                end: end as u32,
            });
        }

        if self.index != start {
            return Some(Combinator {
                kind: CombinatorKind::Combinator,
                name: " ".to_string(),
                start: start as u32,
                end: self.index as u32,
            });
        }

        None
    }

    fn read_block(&mut self) -> Result<Block, CompileDiagnostic> {
        let start = self.index as u32;
        self.eat("{")?;
        let mut children: Vec<BlockChild> = Vec::new();
        while self.index < self.template.len() {
            self.allow_comment_or_whitespace();
            if self.matches("}") {
                break;
            }
            children.push(self.read_block_item()?);
        }
        self.eat("}")?;
        Ok(Block {
            kind: BlockKind::Block,
            start,
            end: self.index as u32,
            children,
        })
    }

    fn read_block_item(&mut self) -> Result<BlockChild, CompileDiagnostic> {
        if self.matches("@") {
            return Ok(BlockChild::Atrule(self.read_at_rule()?));
        }
        // Look-ahead: read a value, peek the next char. `{` → nested rule;
        // anything else → declaration. Then rewind to start.
        let start = self.index;
        let _ = self.read_value()?;
        let after = self.peek_byte();
        self.index = start;
        if after == Some(b'{') {
            Ok(BlockChild::Rule(self.read_rule()?))
        } else {
            Ok(BlockChild::Declaration(self.read_declaration()?))
        }
    }

    fn read_declaration(&mut self) -> Result<Declaration, CompileDiagnostic> {
        let start = self.index;
        let property = self.read_until_whitespace_or_colon();
        self.allow_whitespace();
        self.eat(":")?;
        let index = self.index;
        self.allow_whitespace();
        let value = self.read_value()?;
        if value.is_empty() && !property.starts_with("--") {
            return Err(errors::css_empty_declaration(Some((
                start as u32,
                index as u32,
            ))));
        }
        let end = self.index as u32;
        if !self.matches("}") {
            self.eat(";")?;
        }
        Ok(Declaration {
            kind: DeclarationKind::Declaration,
            start: start as u32,
            end,
            property,
            value,
        })
    }

    /// Read a CSS value up to `;`, `{`, or `}` (whichever comes first), but
    /// respect strings, `url(...)`, and `/* */` comments. Mirrors
    /// `read_value` in style.js:497-550.
    fn read_value(&mut self) -> Result<String, CompileDiagnostic> {
        let mut value = String::new();
        let mut escaped = false;
        let mut in_url = false;
        let mut quote_mark: Option<u8> = None;

        while self.index < self.template.len() {
            let ch = self.template[self.index..].chars().next().unwrap();
            let b = ch as u32;

            if escaped {
                value.push('\\');
                value.push(ch);
                escaped = false;
                self.index += ch.len_utf8();
                continue;
            } else if ch == '\\' {
                escaped = true;
                self.index += 1;
                continue;
            } else if quote_mark.is_some_and(|q| b == q as u32) {
                quote_mark = None;
            } else if ch == ')' {
                in_url = false;
            } else if quote_mark.is_none() && (ch == '"' || ch == '\'') {
                quote_mark = Some(ch as u8);
            } else if ch == '(' && value.ends_with("url") {
                in_url = true;
            } else if (ch == ';' || ch == '{' || ch == '}') && !in_url && quote_mark.is_none() {
                return Ok(value.trim().to_string());
            } else if ch == '/'
                && !in_url
                && quote_mark.is_none()
                && self.template.as_bytes().get(self.index + 1) == Some(&b'*')
            {
                self.index += 2;
                while self.index < self.template.len() {
                    if self.matches("*/") {
                        self.index += 2;
                        break;
                    }
                    self.index += 1;
                }
                continue;
            }

            value.push(ch);
            self.index += ch.len_utf8();
        }

        Err(errors::unexpected_eof(Some((
            self.template.len() as u32,
            self.template.len() as u32,
        ))))
    }

    fn read_attribute_value(&mut self) -> Result<String, CompileDiagnostic> {
        let mut value = String::new();
        let mut escaped = false;
        let quote_mark: Option<u8> = if self.try_eat("\"") {
            Some(b'"')
        } else if self.try_eat("'") {
            Some(b'\'')
        } else {
            None
        };
        while self.index < self.template.len() {
            let ch = self.template[self.index..].chars().next().unwrap();
            if escaped {
                value.push('\\');
                value.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if quote_mark
                .map(|q| ch as u32 == q as u32)
                .unwrap_or_else(|| matches!(ch, ' ' | '\t' | '\n' | '\r' | ']'))
            {
                if let Some(q) = quote_mark {
                    self.eat(if q == b'"' { "\"" } else { "'" })?;
                }
                return Ok(value.trim().to_string());
            } else {
                value.push(ch);
            }
            self.index += ch.len_utf8();
        }
        Err(errors::unexpected_eof(Some((
            self.template.len() as u32,
            self.template.len() as u32,
        ))))
    }

    /// Match `[~^$*|]?=` and return the matched matcher string, or None.
    fn read_attribute_matcher(&mut self) -> Option<String> {
        let bytes = self.template.as_bytes();
        let i = self.index;
        if i >= bytes.len() {
            return None;
        }
        let len = match bytes[i] {
            b'~' | b'^' | b'$' | b'*' | b'|' if bytes.get(i + 1) == Some(&b'=') => 2,
            b'=' => 1,
            _ => return None,
        };
        let s = self.template[i..i + len].to_string();
        self.index += len;
        Some(s)
    }

    /// Match `[a-zA-Z]+` and return the matched string, or None.
    fn read_attribute_flags(&mut self) -> Option<String> {
        let bytes = self.template.as_bytes();
        let start = self.index;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
            end += 1;
        }
        if end == start {
            None
        } else {
            self.index = end;
            Some(self.template[start..end].to_string())
        }
    }

    /// Match nth-of patterns. Mirrors REGEX_NTH_OF in style.js:11:
    /// `(even|odd|\+?(\d+|\d*n(\s*[+-]\s*\d+)?)|-\d*n(\s*\+\s*\d+))((?=\s*[,)])|\s+of\s+)`.
    /// Returns true if a valid pattern is present.
    fn match_nth_of(&self) -> bool {
        self.scan_nth_of().is_some()
    }

    fn read_nth_of(&mut self) -> String {
        let end = self.scan_nth_of().unwrap_or(self.index);
        let s = self.template[self.index..end].to_string();
        self.index = end;
        s
    }

    /// Returns the end byte offset if `template[self.index..]` matches the
    /// nth-of regex; otherwise None.
    fn scan_nth_of(&self) -> Option<usize> {
        let bytes = self.template.as_bytes();
        let start = self.index;
        let mut i = start;

        // Match `even`, `odd`, or one of the numeric forms.
        let n_end = if self.template[i..].starts_with("even") {
            i + 4
        } else if self.template[i..].starts_with("odd") {
            i + 3
        } else {
            // `\+?(\d+|\d*n(\s*[+-]\s*\d+)?)|-\d*n(\s*\+\s*\d+)`
            let mut j = i;
            // Optional leading `+` or `-`.
            let has_leading_minus = bytes.get(j) == Some(&b'-');
            if bytes.get(j) == Some(&b'+') || has_leading_minus {
                j += 1;
            }
            let digit_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let had_digits = j > digit_start;
            let has_n = bytes.get(j) == Some(&b'n');
            if has_n {
                j += 1;
                // Optional `\s*[+-]\s*\d+` (or `\s*+\s*\d+` for the `-d*n` case).
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if matches!(bytes.get(k), Some(b'+') | Some(b'-')) {
                    k += 1;
                    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    let d = k;
                    while k < bytes.len() && bytes[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k > d {
                        j = k;
                    }
                }
            } else if !had_digits {
                return None;
            }
            j
        };

        i = n_end;

        // Trailing alternative: `(?=\s*[,)])` or `\s+of\s+`.
        // Try lookahead first.
        let mut k = i;
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if matches!(bytes.get(k), Some(b',') | Some(b')')) {
            return Some(i);
        }
        // `\s+of\s+`
        let mut k = i;
        let ws_start = k;
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if k == ws_start {
            return None;
        }
        if !self.template[k..].starts_with("of") {
            return None;
        }
        let after_of = k + 2;
        let mut m = after_of;
        let ws_start = m;
        while m < bytes.len() && bytes[m].is_ascii_whitespace() {
            m += 1;
        }
        if m == ws_start {
            return None;
        }
        Some(m)
    }

    /// Match `\d+(\.\d+)?%`.
    fn try_read_percentage(&mut self) -> Option<String> {
        let bytes = self.template.as_bytes();
        let start = self.index;
        let mut i = start;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        if bytes.get(i) == Some(&b'.') {
            let after_dot = i + 1;
            let mut j = after_dot;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > after_dot {
                i = j;
            }
        }
        if bytes.get(i) != Some(&b'%') {
            return None;
        }
        i += 1;
        self.index = i;
        Some(self.template[start..i].to_string())
    }

    /// Peek: does cursor start with a combinator (`+`, `~`, `>`, or `||`)?
    fn match_combinator(&self) -> bool {
        let bytes = self.template.as_bytes();
        match bytes.get(self.index) {
            Some(b'+') | Some(b'~') | Some(b'>') => true,
            Some(b'|') => bytes.get(self.index + 1) == Some(&b'|'),
            _ => false,
        }
    }

    /// Read until `\s` or `:`. Used for declaration property names.
    fn read_until_whitespace_or_colon(&mut self) -> String {
        let start = self.index;
        let bytes = self.template.as_bytes();
        while self.index < bytes.len() {
            let b = bytes[self.index];
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b':') {
                break;
            }
            self.index += 1;
        }
        self.template[start..self.index].to_string()
    }

    /// CSS identifier: leading `\d` or `-\d` rejected; chars are
    /// `[a-zA-Z0-9_-]` or non-ASCII (>= U+00A0) or escape sequences.
    fn read_identifier(&mut self) -> Result<String, CompileDiagnostic> {
        let start = self.index;
        // Forbid leading hyphen-digit or digit. Match `-?\d` at start.
        let bytes = self.template.as_bytes();
        let leading_bad = match bytes.get(start) {
            Some(b'-') => matches!(bytes.get(start + 1), Some(b) if b.is_ascii_digit()),
            Some(b) => b.is_ascii_digit(),
            None => false,
        };
        if leading_bad {
            return Err(errors::css_expected_identifier(Some((
                start as u32,
                start as u32,
            ))));
        }

        let mut identifier = String::new();
        while self.index < self.template.len() {
            let ch = self.template[self.index..].chars().next().unwrap();
            if ch == '\\' {
                // Unicode escape `\HHHHHH(\r\n|\s)?` or `\<char>` fallback.
                let after = self.index + 1;
                let bytes = self.template.as_bytes();
                let mut hex_end = after;
                while hex_end < bytes.len()
                    && hex_end - after < 6
                    && bytes[hex_end].is_ascii_hexdigit()
                {
                    hex_end += 1;
                }
                if hex_end > after {
                    let code = u32::from_str_radix(&self.template[after..hex_end], 16)
                        .unwrap_or(0);
                    if let Some(c) = char::from_u32(code) {
                        identifier.push(c);
                    }
                    self.index = hex_end;
                    // Optional trailing whitespace (CR, LF, or single space).
                    if let Some(&b) = bytes.get(self.index) {
                        if b == b'\r' && bytes.get(self.index + 1) == Some(&b'\n') {
                            self.index += 2;
                        } else if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c) {
                            self.index += 1;
                        }
                    }
                } else if let Some(c2) = self.template[after..].chars().next() {
                    identifier.push('\\');
                    identifier.push(c2);
                    self.index = after + c2.len_utf8();
                } else {
                    break;
                }
            } else if (ch as u32) >= 160 || ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                identifier.push(ch);
                self.index += ch.len_utf8();
            } else {
                break;
            }
        }

        if identifier.is_empty() {
            return Err(errors::css_expected_identifier(Some((
                start as u32,
                start as u32,
            ))));
        }
        Ok(identifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_rule() {
        let src = "div { color: red; }</style>";
        let (sheet, _) = read_style(src, 0, 0, vec![], None).unwrap();
        assert_eq!(sheet.children.len(), 1);
        let rule = match &sheet.children[0] {
            StyleSheetChild::Rule(r) => r,
            _ => panic!("expected rule"),
        };
        assert_eq!(rule.block.children.len(), 1);
        match &rule.block.children[0] {
            BlockChild::Declaration(d) => {
                assert_eq!(d.property, "color");
                assert_eq!(d.value, "red");
            }
            _ => panic!("expected declaration"),
        }
    }

    #[test]
    fn parses_atrule() {
        let src = "@media (min-width: 600px) { div { color: red; } }</style>";
        let (sheet, _) = read_style(src, 0, 0, vec![], None).unwrap();
        assert_eq!(sheet.children.len(), 1);
        match &sheet.children[0] {
            StyleSheetChild::Atrule(a) => {
                assert_eq!(a.name, "media");
                assert!(a.block.is_some());
            }
            _ => panic!("expected atrule"),
        }
    }
}
