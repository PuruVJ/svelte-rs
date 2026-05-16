//! Port of esrap's comment-handling helpers from
//! `node_modules/esrap@2.2.4/src/languages/ts/index.js`:
//! `reset_comment_index`, `flush_trailing_comments`, `flush_comments_until`,
//! `write_comment`, `before`.

use serde_json::Value;

use crate::context::Context;

/// `(line, column)` pair. Acorn `loc` uses 1-indexed line and 0-indexed column.
pub type Pos = (u32, u32);

/// Mirror of `function before(a, b)` — `ts/index.js:2251-2255`.
pub fn before(a: Pos, b: Pos) -> bool {
    if a.0 < b.0 {
        return true;
    }
    if a.0 > b.0 {
        return false;
    }
    a.1 < b.1
}

fn loc(node: &Value, key: &str) -> Option<Pos> {
    let loc = node.get("loc")?;
    let p = loc.get(key)?;
    Some((
        p.get("line")?.as_u64()? as u32,
        p.get("column")?.as_u64()? as u32,
    ))
}

fn comment_start(c: &Value) -> Option<Pos> {
    loc(c, "start")
}
fn comment_end(c: &Value) -> Option<Pos> {
    loc(c, "end")
}

/// `write_comment` — emit a single `Line` or `Block` comment into the context's
/// command stream. Multi-line `/* ... */` block comments are split on `\n` so
/// each line ends up on its own output line. Mirrors `ts/index.js:80-95`.
pub fn write_comment(comment: &Value, ctx: &mut Context) {
    let kind = comment.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let value = comment.get("value").and_then(|v| v.as_str()).unwrap_or("");
    if kind == "Line" {
        ctx.write(&format!("//{value}"), None);
    } else {
        ctx.write("/*", None);
        let lines: Vec<&str> = value.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                ctx.newline();
            }
            ctx.write(line, None);
        }
        ctx.write("*/", None);
        if lines.len() > 1 {
            ctx.newline();
        }
    }
}

/// Reset the rolling comment index to point at the first comment whose start
/// is at or after `node.loc.start`. Mirrors `ts/index.js:144-167`.
pub fn reset_comment_index(node: &Value, ctx: &Context) {
    let state = ctx.comment_state();
    let mut state = state.borrow_mut();

    // Nodes without `loc` are synthetic (built by transforms, no source
    // location). Don't advance the cursor for them — leaving the comment
    // index in place lets subsequent loc-bearing nodes pick up the comments.
    let Some(node_start) = loc(node, "start") else {
        return;
    };

    // fast path: cursor already correct
    let current_ok = state
        .comments
        .get(state.index)
        .and_then(comment_start)
        .map(|cs| !before(cs, node_start))
        .unwrap_or(false);
    let prev_ok = if state.index == 0 {
        true
    } else {
        state
            .comments
            .get(state.index - 1)
            .and_then(comment_start)
            .map(|ps| before(ps, node_start))
            .unwrap_or(false)
    };
    if current_ok && prev_ok {
        return;
    }

    // linear scan (upstream's TODO is binary search)
    let new_idx = state
        .comments
        .iter()
        .position(|c| comment_start(c).map(|cs| !before(cs, node_start)).unwrap_or(false))
        .unwrap_or(state.comments.len());
    state.index = new_idx;
}

/// Drain trailing comments that sit on the same line as `prev` (the just-emitted
/// node's end) and before `next` (the next node's start, or `None` for end of
/// region). Each emitted Line comment forces a newline; Block comments stay
/// inline. Mirrors `ts/index.js:174-198`.
pub fn flush_trailing_comments(ctx: &mut Context, prev: Option<Pos>, next: Option<Pos>) {
    let state = ctx.comment_state();
    loop {
        let comment_opt = {
            let s = state.borrow();
            s.comments.get(s.index).cloned()
        };
        let Some(comment) = comment_opt else { break };
        let Some(cs) = comment_start(&comment) else { break };
        let Some(ce) = comment_end(&comment) else { break };
        let Some(prev_pos) = prev else { break };
        let same_line = cs.0 == prev_pos.0;
        let before_next = match next {
            Some(n) => before(ce, n),
            None => true,
        };
        if !(same_line && before_next) {
            break;
        }
        ctx.write(" ", None);
        write_comment(&comment, ctx);
        {
            let mut s = state.borrow_mut();
            s.index += 1;
        }
        let kind = comment.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "Line" {
            ctx.newline();
        } else {
            continue;
        }
    }
}

/// Drain comments that fall in `[from, to)`. Emits a leading margin if the first
/// comment is on a later line than `from`. When `pad` is true, single-line block
/// comments are followed by a space; otherwise they're left adjacent.
/// Mirrors `ts/index.js:206-233`.
pub fn flush_comments_until(ctx: &mut Context, from: Option<Pos>, to: Option<Pos>, pad: bool) {
    let Some(to) = to else { return };
    let state = ctx.comment_state();
    let mut first = true;
    loop {
        let comment_opt = {
            let s = state.borrow();
            s.comments.get(s.index).cloned()
        };
        let Some(comment) = comment_opt else { break };
        let Some(cs) = comment_start(&comment) else { break };
        let Some(ce) = comment_end(&comment) else { break };
        if !before(cs, to) {
            break;
        }
        if first {
            if let Some(f) = from {
                if cs.0 > f.0 {
                    ctx.margin();
                    ctx.newline();
                }
            }
        }
        first = false;
        write_comment(&comment, ctx);
        if ce.0 < to.0 {
            ctx.newline();
        } else if pad {
            ctx.write(" ", None);
        }
        {
            let mut s = state.borrow_mut();
            s.index += 1;
        }
    }
}
