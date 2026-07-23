//! String hash used for scoped CSS class names and `$.head` keys.
//!
//! Mirrors `packages/svelte/src/utils.js` `hash()`.

/// DJB2-variant (XOR) base-36 hash — upstream `hash(str)`.
pub fn svelte_str_hash(s: &str) -> String {
    let s: String = s.chars().filter(|c| *c != '\r').collect();
    let mut h: i64 = 5381;
    for c in s.chars().rev() {
        h = ((h << 5) - h) ^ (c as i64);
        h &= 0xFFFFFFFF;
    }
    let mut n = h as u32;
    if n == 0 {
        return "0".into();
    }
    let chars: Vec<char> = "0123456789abcdefghijklmnopqrstuvwxyz".chars().collect();
    let mut out = String::new();
    while n > 0 {
        out.insert(0, chars[(n % 36) as usize]);
        n /= 36;
    }
    out
}

/// Default `cssHash` when no callback is supplied — upstream
/// `validate-options.js` default.
pub fn default_css_class_hash(filename: &str, css_styles: &str) -> String {
    let basis = if filename == "(unknown)" {
        css_styles
    } else {
        filename
    };
    format!("svelte-{}", svelte_str_hash(basis))
}
