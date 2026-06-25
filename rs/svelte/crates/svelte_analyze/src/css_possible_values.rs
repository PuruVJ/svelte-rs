//! `gather_possible_values` / `get_possible_values` port.
//!
//! Ported from `packages/svelte/src/compiler/phases/2-analyze/css/utils.js:11-96`.
//!
//! Given an expression node (in the acorn JSON shape we produce from
//! `<script>` and mustache parses), enumerate the string values it might
//! evaluate to. Used by the prune pass to decide whether `class="foo"`
//! plus `class={cond ? 'bar' : 'baz'}` could yield a value matching
//! `.bar` (yes) or `.qux` (no).
//!
//! Returns `None` when the expression isn't statically analysable —
//! callers must conservatively assume any value is possible.

use std::collections::HashSet;

use serde_json::Value;

/// Output of `gather_possible_values`. Either an enumerable set of
/// concrete string values, or `None` ("unknown / could be anything").
#[derive(Debug, Default, Clone)]
pub struct PossibleValues {
    pub set: HashSet<String>,
    pub unknown: bool,
}

impl PossibleValues {
    pub fn add(&mut self, v: impl Into<String>) {
        self.set.insert(v.into());
    }
    pub fn mark_unknown(&mut self) {
        self.unknown = true;
    }
    pub fn into_option(self) -> Option<Vec<String>> {
        if self.unknown {
            None
        } else {
            Some(self.set.into_iter().collect())
        }
    }
}

/// Inspect `node` and add to `out` every string value it could yield.
/// `is_class` is true if the expression backs a `class={...}` attribute —
/// in that case arrays / objects are treated as clsx-style class lists.
pub fn gather_possible_values(node: &Value, is_class: bool, out: &mut PossibleValues) {
    gather(node, is_class, out, false);
}

fn gather(node: &Value, is_class: bool, out: &mut PossibleValues, is_nested: bool) {
    if out.unknown {
        return;
    }

    let t = node.get("type").and_then(|v| v.as_str());
    match t {
        Some("Literal") => {
            if let Some(s) = node.get("value").and_then(|v| v.as_str()) {
                out.add(s.to_string());
            } else if let Some(b) = node.get("value").and_then(|v| v.as_bool()) {
                out.add(b.to_string());
            } else if let Some(n) = node.get("value").and_then(|v| v.as_f64()) {
                out.add(format_number(n));
            } else if let Some(()) = node.get("value").and_then(|v| if v.is_null() { Some(()) } else { None }) {
                // `null` literal — treat as unknown (matches upstream's behavior of falling through).
                out.mark_unknown();
            } else {
                out.mark_unknown();
            }
        }
        Some("ConditionalExpression") => {
            if let Some(c) = node.get("consequent") {
                gather(c, is_class, out, is_nested);
            }
            if let Some(a) = node.get("alternate") {
                gather(a, is_class, out, is_nested);
            }
        }
        Some("LogicalExpression") => {
            let op = node.get("operator").and_then(|v| v.as_str()).unwrap_or("");
            if op == "&&" {
                // `&&` is special: the left side is only included if it's
                // falsy. We gather left into a sub-set and inspect.
                let mut left = PossibleValues::default();
                if let Some(l) = node.get("left") {
                    gather(l, is_class, &mut left, is_nested);
                }
                if left.unknown {
                    // Conservative: any non-nullish falsy values are
                    // possible (unless this is a `class` attribute being
                    // processed by clsx).
                    if !is_class || !is_nested {
                        out.add("");
                        // `false`, `NaN`, `0` — stringify
                        out.add("false");
                        out.add("NaN");
                        out.add("0");
                    }
                } else {
                    // For known left-side values, only keep the falsy ones.
                    for v in &left.set {
                        if is_falsy(v) && (!is_class || !is_nested) {
                            out.add(v.clone());
                        }
                    }
                }
                if let Some(r) = node.get("right") {
                    gather(r, is_class, out, is_nested);
                }
            } else {
                if let Some(l) = node.get("left") {
                    gather(l, is_class, out, is_nested);
                }
                if let Some(r) = node.get("right") {
                    gather(r, is_class, out, is_nested);
                }
            }
        }
        Some("ArrayExpression") if is_class => {
            if let Some(elements) = node.get("elements").and_then(|v| v.as_array()) {
                for entry in elements {
                    if !entry.is_null() {
                        gather(entry, is_class, out, true);
                    }
                }
            }
        }
        Some("ObjectExpression") if is_class => {
            if let Some(properties) = node.get("properties").and_then(|v| v.as_array()) {
                for p in properties {
                    if p.get("type").and_then(|v| v.as_str()) != Some("Property") {
                        out.mark_unknown();
                        continue;
                    }
                    if p.get("computed").and_then(|v| v.as_bool()) == Some(true) {
                        out.mark_unknown();
                        continue;
                    }
                    let Some(key) = p.get("key") else {
                        out.mark_unknown();
                        continue;
                    };
                    let key_type = key.get("type").and_then(|v| v.as_str());
                    match key_type {
                        Some("Identifier") => {
                            if let Some(n) = key.get("name").and_then(|v| v.as_str()) {
                                out.add(n.to_string());
                            } else {
                                out.mark_unknown();
                            }
                        }
                        Some("Literal") => {
                            if let Some(s) = key.get("value").and_then(|v| v.as_str()) {
                                out.add(s.to_string());
                            } else if let Some(n) = key.get("value").and_then(|v| v.as_f64()) {
                                out.add(format_number(n));
                            } else {
                                out.mark_unknown();
                            }
                        }
                        _ => out.mark_unknown(),
                    }
                }
            }
        }
        _ => out.mark_unknown(),
    }
}

/// Stringify a JS-style number the way `String(n)` would. Integers print
/// without a decimal point; otherwise use the default float formatter.
fn format_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

fn is_falsy(s: &str) -> bool {
    s.is_empty() || s == "0" || s == "false" || s == "NaN"
}

/// Convenience entry point for `get_possible_values(chunk, is_class)` —
/// chunk is either a `Text` or `ExpressionTag` from an attribute value.
/// Returns `None` if any expression in the chunk is non-analysable.
pub fn get_possible_values(chunk: &Value, is_class: bool) -> Option<Vec<String>> {
    let t = chunk.get("type").and_then(|v| v.as_str());
    let mut out = PossibleValues::default();
    match t {
        Some("Text") => {
            if let Some(d) = chunk.get("data").and_then(|v| v.as_str()) {
                out.add(d.to_string());
            }
        }
        Some("ExpressionTag") => {
            if let Some(expr) = chunk.get("expression") {
                gather_possible_values(expr, is_class, &mut out);
            }
        }
        _ => out.mark_unknown(),
    }
    out.into_option()
}
