//! Port of `node_modules/esrap@2.2.4/src/context.js`.

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::Value;

/// One element of the command stream emitted by visitors. Mirrors esrap's
/// `Command` union: a number (one of the layout constants below), a string
/// literal to write, a `{ type: 'Location', line, column }` sourcemap pin,
/// or a nested array of commands. See `context.js:1-7` for the constants.
#[derive(Debug, Clone)]
pub enum Command {
    /// Pop indentation after the next newline.
    Margin,
    /// Insert a newline before the next non-layout command.
    Newline,
    /// Increase indentation (effective on subsequent newlines).
    Indent,
    /// Decrease indentation.
    Dedent,
    /// Insert a single space before the next non-layout command.
    Space,
    /// Raw text to emit verbatim.
    Str(String),
    /// Sourcemap mapping pin: `(line, column)` are 1-indexed line / 0-indexed
    /// column, mirroring acorn's `loc` shape. Stored 0-indexed line here to
    /// match esrap's segment output (it subtracts 1 at emission time).
    Location { line: u32, column: u32 },
    /// Nested run of commands (e.g. a sub-visitor's output appended in one
    /// shot, see `Context.append` in `context.js:50-56`).
    Group(Vec<Command>),
}

/// Options forwarded from `print(node, visitors, opts)` — see `index.js:54`.
#[derive(Debug, Clone, Default)]
pub struct PrintOptions {
    /// Indent string. Defaults to a single tab to match esrap upstream
    /// (`index.js:90`).
    pub indent: Option<String>,
    /// `sources[0]` in the emitted v3 sourcemap.
    pub source_map_source: Option<String>,
    /// `sourcesContent[0]` in the emitted v3 sourcemap.
    pub source_map_content: Option<String>,
    /// When `false`, return the mappings as a `[number, number, number, number][][]`
    /// instead of the VLQ string. Defaults to `true` (encoded) for parity with
    /// esrap (`index.js:29`).
    pub source_map_encode_mappings: Option<bool>,
    /// Comment nodes to interleave with output, sorted by start position.
    /// Mirrors `options.comments` consumed by the TS visitor factory in
    /// `ts/index.js:104-106`.
    pub comments: Vec<Value>,
}

/// Shared comment cursor — needs interior mutability since multiple `Context`s
/// (parent + spawned child via `fresh()`) read/write a single rolling index
/// during a single `print()` call. Mirrors esrap's closure-captured
/// `comment_index` in `ts/index.js:106`.
#[derive(Debug)]
pub struct CommentState {
    pub comments: Vec<Value>,
    pub index: usize,
}

impl CommentState {
    pub fn new(comments: Vec<Value>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self { comments, index: 0 }))
    }
}

/// Visitor table: keyed on `node.type`, value is the function that emits the
/// node into a [`Context`]. Mirrors `Visitors` in esrap.
pub type VisitorTable = std::collections::HashMap<&'static str, VisitorFn>;

pub type VisitorFn = fn(&Value, &mut Context);

/// The transient state a visitor uses while emitting one node — the rolling
/// command buffer plus `has_newline`/`multiline` tracking. Mirrors `class Context`.
pub struct Context<'v> {
    visitors: &'v VisitorTable,
    comments: Rc<RefCell<CommentState>>,
    pub(crate) commands: Vec<Command>,
    has_newline: bool,
    /// Mirrors esrap's `multiline` flag — set when this context emitted (or
    /// appended) anything that forces multi-line layout. Visitors read it via
    /// [`Context::is_multiline`] to choose between single- and multi-line forms.
    pub multiline: bool,
}

impl<'v> Context<'v> {
    pub fn new(visitors: &'v VisitorTable) -> Self {
        Self {
            visitors,
            comments: CommentState::new(Vec::new()),
            commands: Vec::new(),
            has_newline: false,
            multiline: false,
        }
    }

    pub fn with_comments(visitors: &'v VisitorTable, comments: Rc<RefCell<CommentState>>) -> Self {
        Self {
            visitors,
            comments,
            commands: Vec::new(),
            has_newline: false,
            multiline: false,
        }
    }

    pub fn indent(&mut self) {
        self.commands.push(Command::Indent);
    }

    pub fn dedent(&mut self) {
        self.commands.push(Command::Dedent);
    }

    pub fn margin(&mut self) {
        self.commands.push(Command::Margin);
    }

    pub fn newline(&mut self) {
        self.has_newline = true;
        self.commands.push(Command::Newline);
    }

    pub fn space(&mut self) {
        self.commands.push(Command::Space);
    }

    /// Append another context's command stream. Mirrors `Context.append` —
    /// propagates `multiline` if either side is multi-line. See `context.js:50-56`.
    pub fn append(&mut self, other: Context<'v>) {
        self.commands.push(Command::Group(other.commands));
        if self.has_newline || other.multiline {
            self.multiline = true;
        }
    }

    /// Write a string, optionally wrapping it in start/end `Location` pins from
    /// the node's `loc`. Mirrors `Context.write` — see `context.js:63-75`.
    pub fn write(&mut self, content: &str, node: Option<&Value>) {
        if let Some(loc) = node.and_then(|n| n.get("loc")) {
            if let (Some(start), Some(end)) = (loc.get("start"), loc.get("end")) {
                if let (Some(sl), Some(sc)) = (
                    start.get("line").and_then(|v| v.as_u64()),
                    start.get("column").and_then(|v| v.as_u64()),
                ) {
                    self.commands.push(Command::Location {
                        line: sl as u32,
                        column: sc as u32,
                    });
                }
                self.commands.push(Command::Str(content.to_string()));
                if let (Some(el), Some(ec)) = (
                    end.get("line").and_then(|v| v.as_u64()),
                    end.get("column").and_then(|v| v.as_u64()),
                ) {
                    self.commands.push(Command::Location {
                        line: el as u32,
                        column: ec as u32,
                    });
                }
                if self.has_newline {
                    self.multiline = true;
                }
                return;
            }
        }
        self.commands.push(Command::Str(content.to_string()));
        if self.has_newline {
            self.multiline = true;
        }
    }

    /// Emit a bare `Location` pin without a string payload. Mirrors `Context.location`.
    pub fn location(&mut self, line: u32, column: u32) {
        self.commands.push(Command::Location { line, column });
    }

    /// Dispatch a node through the visitor table. Mirrors `Context.visit` —
    /// `context.js:89-113`.
    pub fn visit(&mut self, node: &Value) {
        let ty = node
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("<no type>");
        if let Some(visitor) = self.visitors.get(ty) {
            visitor(node, self);
        } else {
            panic!("svelte_codegen_js: not implemented: {ty}");
        }
    }

    /// `true` iff this context has produced nothing but layout markers (no
    /// string content). Mirrors `Context.empty` — `context.js:115-117`.
    pub fn empty(&self) -> bool {
        !self.commands.iter().any(has_content)
    }

    /// Total length of string content in this context's command stream.
    /// Used by some visitors to choose layout. Mirrors `Context.measure`.
    pub fn measure(&self) -> usize {
        measure_commands(&self.commands)
    }

    /// Spawn a fresh `Context` sharing this one's visitor table. Mirrors `Context.new`.
    pub fn fresh(&self) -> Context<'v> {
        Context::with_comments(self.visitors, self.comments.clone())
    }

    pub(crate) fn comment_state(&self) -> Rc<RefCell<CommentState>> {
        self.comments.clone()
    }

    pub fn is_multiline(&self) -> bool {
        self.multiline
    }
}

fn has_content(cmd: &Command) -> bool {
    match cmd {
        Command::Str(s) => !s.is_empty(),
        Command::Group(cs) => cs.iter().any(has_content),
        _ => false,
    }
}

fn measure_commands(cmds: &[Command]) -> usize {
    let mut total = 0;
    for c in cmds {
        match c {
            Command::Str(s) => total += s.len(),
            Command::Group(g) => total += measure_commands(g),
            _ => {}
        }
    }
    total
}
