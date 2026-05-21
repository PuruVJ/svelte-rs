//! Typed JS AST.
//!
//! This is the internal IR shared by `svelte_transform_*` (producers) and
//! `svelte_codegen_js` (consumer). It does **not** need to match acorn's
//! wire shape — that's the original mistake we're undoing. Shape is whatever
//! makes the producer + consumer fastest and simplest, optimized for the
//! common cases each visitor hits (`CallExpression`, `MemberExpression`,
//! `Identifier`, `Literal`, etc).
//!
//! Design choices:
//! - **Typed enums, not `serde_json::Value`.** Discriminant is one byte plus
//!   a pointer to the payload — no string-keyed hash lookups in hot loops.
//! - **`Box<T>` per child node.** Not as fast as a single-arena layout, but
//!   no lifetime annotations bleeding through the entire codebase. We can
//!   migrate to bumpalo later behind the same enum API if the profile says
//!   we need to.
//! - **Spans are optional `(u32, u32)`.** Only set on nodes whose positions
//!   matter for sourcemaps; transform-synthesized nodes leave it `None`.
//! - **No `Serialize` impls.** Public AST exposure (if/when added) goes
//!   through a separate `to_estree` function — that's a layer, not a
//!   constraint on the IR.

#![forbid(unsafe_code)]

mod span;

pub use span::Span;

// -------------------------------------------------------------------------
// Top-level
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Program {
    pub source_type: SourceType,
    pub body: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceType {
    Script,
    #[default]
    Module,
}

// -------------------------------------------------------------------------
// Statements
// -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Block(Box<BlockStatement>),
    Break(Box<BreakStatement>),
    Continue(Box<ContinueStatement>),
    Debugger(Span),
    DoWhile(Box<DoWhileStatement>),
    Empty(Span),
    Expression(Box<ExpressionStatement>),
    For(Box<ForStatement>),
    ForIn(Box<ForInStatement>),
    ForOf(Box<ForOfStatement>),
    If(Box<IfStatement>),
    Labeled(Box<LabeledStatement>),
    Return(Box<ReturnStatement>),
    Switch(Box<SwitchStatement>),
    Throw(Box<ThrowStatement>),
    Try(Box<TryStatement>),
    While(Box<WhileStatement>),
    With(Box<WithStatement>),
    // Declarations
    Variable(Box<VariableDeclaration>),
    Function(Box<FunctionDeclaration>),
    Class(Box<ClassDeclaration>),
    // Modules
    Import(Box<ImportDeclaration>),
    ExportNamed(Box<ExportNamedDeclaration>),
    ExportDefault(Box<ExportDefaultDeclaration>),
    ExportAll(Box<ExportAllDeclaration>),
    /// Escape hatch: raw pre-formatted JS to splice into the output. Used by
    /// transforms that have already produced source text and want to embed
    /// it without re-parsing. The string is inserted verbatim. NOT for new
    /// uses — exists during migration only.
    Raw(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockStatement {
    pub body: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionStatement {
    pub expression: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReturnStatement {
    pub argument: Option<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IfStatement {
    pub test: Expression,
    pub consequent: Statement,
    pub alternate: Option<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DoWhileStatement {
    pub body: Statement,
    pub test: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhileStatement {
    pub test: Expression,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForStatement {
    pub init: Option<ForInit>,
    pub test: Option<Expression>,
    pub update: Option<Expression>,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ForInit {
    Declaration(Box<VariableDeclaration>),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForInStatement {
    pub left: ForInit,
    pub right: Expression,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForOfStatement {
    pub left: ForInit,
    pub right: Expression,
    pub body: Statement,
    pub r#await: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BreakStatement {
    pub label: Option<Identifier>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContinueStatement {
    pub label: Option<Identifier>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LabeledStatement {
    pub label: Identifier,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThrowStatement {
    pub argument: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TryStatement {
    pub block: BlockStatement,
    pub handler: Option<CatchClause>,
    pub finalizer: Option<BlockStatement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    pub param: Option<Pattern>,
    pub body: BlockStatement,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchStatement {
    pub discriminant: Expression,
    pub cases: Vec<SwitchCase>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    pub test: Option<Expression>,
    pub consequent: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WithStatement {
    pub object: Expression,
    pub body: Statement,
    pub span: Span,
}

// -------------------------------------------------------------------------
// Declarations
// -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct VariableDeclaration {
    pub kind: VariableKind,
    pub declarations: Vec<VariableDeclarator>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableKind {
    Var,
    Let,
    Const,
}

impl VariableKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            VariableKind::Var => "var",
            VariableKind::Let => "let",
            VariableKind::Const => "const",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariableDeclarator {
    pub id: Pattern,
    pub init: Option<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionDeclaration {
    pub id: Option<Identifier>,
    pub params: Vec<Pattern>,
    pub body: BlockStatement,
    pub generator: bool,
    pub r#async: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassDeclaration {
    pub id: Option<Identifier>,
    pub super_class: Option<Expression>,
    pub body: ClassBody,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassBody {
    pub body: Vec<ClassMember>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClassMember {
    Method(Box<MethodDefinition>),
    Property(Box<PropertyDefinition>),
    StaticBlock(Box<StaticBlock>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MethodDefinition {
    pub key: PropertyKey,
    pub value: FunctionExpression,
    pub kind: MethodKind,
    pub computed: bool,
    pub r#static: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Constructor,
    Method,
    Get,
    Set,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDefinition {
    pub key: PropertyKey,
    pub value: Option<Expression>,
    pub computed: bool,
    pub r#static: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StaticBlock {
    pub body: Vec<Statement>,
    pub span: Span,
}

// -------------------------------------------------------------------------
// Modules
// -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ImportDeclaration {
    pub specifiers: Vec<ImportSpecifierKind>,
    pub source: StringLiteral,
    /// `import type { X } from '…'` — TypeScript type-only import. When
    /// true, individual specifiers don't carry their own `type` keyword.
    pub type_only: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportSpecifierKind {
    /// `import { a as b } from 'x'`
    Named(ImportSpecifier),
    /// `import a from 'x'`
    Default(ImportDefaultSpecifier),
    /// `import * as a from 'x'`
    Namespace(ImportNamespaceSpecifier),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportSpecifier {
    pub imported: ModuleExportName,
    pub local: Identifier,
    /// `import { type X } from '…'` — per-specifier type-only marker.
    /// Independent of the parent ImportDeclaration's `type_only` flag.
    pub type_only: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportDefaultSpecifier {
    pub local: Identifier,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportNamespaceSpecifier {
    pub local: Identifier,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModuleExportName {
    Identifier(Identifier),
    String(StringLiteral),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportNamedDeclaration {
    pub declaration: Option<Statement>,
    pub specifiers: Vec<ExportSpecifier>,
    pub source: Option<StringLiteral>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportSpecifier {
    pub local: ModuleExportName,
    pub exported: ModuleExportName,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportDefaultDeclaration {
    pub declaration: ExportDefault,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExportDefault {
    Function(Box<FunctionDeclaration>),
    Class(Box<ClassDeclaration>),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportAllDeclaration {
    pub source: StringLiteral,
    pub exported: Option<ModuleExportName>,
    pub span: Span,
}

// -------------------------------------------------------------------------
// Expressions
// -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Identifier(Identifier),
    Literal(Box<Literal>),
    Template(Box<TemplateLiteral>),
    Array(Box<ArrayExpression>),
    Object(Box<ObjectExpression>),
    Arrow(Box<ArrowFunctionExpression>),
    Function(Box<FunctionExpression>),
    Class(Box<ClassExpression>),
    Member(Box<MemberExpression>),
    Call(Box<CallExpression>),
    New(Box<NewExpression>),
    Binary(Box<BinaryExpression>),
    Logical(Box<LogicalExpression>),
    Assignment(Box<AssignmentExpression>),
    Update(Box<UpdateExpression>),
    Unary(Box<UnaryExpression>),
    Conditional(Box<ConditionalExpression>),
    Sequence(Box<SequenceExpression>),
    Spread(Box<SpreadElement>),
    This(Span),
    Super(Span),
    Yield(Box<YieldExpression>),
    Await(Box<AwaitExpression>),
    Tagged(Box<TaggedTemplateExpression>),
    Paren(Box<ParenthesizedExpression>),
    Meta(Box<MetaProperty>),
    /// Escape hatch: raw pre-formatted source spliced in at expression
    /// position. Used during migration when an underlying transform has
    /// already produced JS text (e.g. snippet bodies pre-rendered as
    /// strings). Avoid for new code.
    Raw(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    String(StringLiteral),
    Number(NumberLiteral),
    Boolean(BooleanLiteral),
    Null(Span),
    Regex(RegexLiteral),
    BigInt(BigIntLiteral),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteral {
    pub value: String,
    /// Raw form including quotes — preserved so the emitter can replay the
    /// exact characters when known (`'foo'` vs `"foo"` vs `\x41`). When
    /// `None`, the emitter formats canonically with single quotes.
    pub raw: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NumberLiteral {
    pub value: f64,
    /// Optional source representation (e.g. `0xff`, `1e3`). When None,
    /// emitter formats canonically.
    pub raw: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BooleanLiteral {
    pub value: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegexLiteral {
    pub pattern: String,
    pub flags: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigIntLiteral {
    /// Decimal digit string ending with `n`. E.g. `"123n"`.
    pub raw: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TemplateLiteral {
    pub quasis: Vec<TemplateElement>,
    pub expressions: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateElement {
    /// The cooked text (after escape processing).
    pub cooked: String,
    /// The raw text (verbatim from source).
    pub raw: String,
    pub tail: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArrayExpression {
    pub elements: Vec<ArrayElement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrayElement {
    Elision,
    Expression(Expression),
    Spread(Box<SpreadElement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectExpression {
    pub properties: Vec<ObjectMember>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObjectMember {
    Property(Box<Property>),
    Spread(Box<SpreadElement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub key: PropertyKey,
    pub value: Expression,
    pub kind: PropertyKind,
    pub computed: bool,
    pub shorthand: bool,
    pub method: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    Init,
    Get,
    Set,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyKey {
    Identifier(Identifier),
    Private(PrivateIdentifier),
    Literal(Box<Literal>),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateIdentifier {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArrowFunctionExpression {
    pub params: Vec<Pattern>,
    pub body: ArrowBody,
    pub r#async: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrowBody {
    Block(Box<BlockStatement>),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionExpression {
    pub id: Option<Identifier>,
    pub params: Vec<Pattern>,
    pub body: BlockStatement,
    pub generator: bool,
    pub r#async: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassExpression {
    pub id: Option<Identifier>,
    pub super_class: Option<Expression>,
    pub body: ClassBody,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberExpression {
    pub object: Expression,
    pub property: MemberProperty,
    pub computed: bool,
    pub optional: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemberProperty {
    Identifier(Identifier),
    Private(PrivateIdentifier),
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallExpression {
    pub callee: Expression,
    pub arguments: Vec<Argument>,
    pub optional: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewExpression {
    pub callee: Expression,
    pub arguments: Vec<Argument>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Argument {
    Expression(Expression),
    Spread(Box<SpreadElement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BinaryExpression {
    pub left: Expression,
    pub operator: BinaryOperator,
    pub right: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Plus,
    Minus,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    NotEq,
    StrictEq,
    StrictNotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    ShiftL,
    ShiftR,
    UnsignedShiftR,
    BitAnd,
    BitOr,
    BitXor,
    InstanceOf,
    In,
}

impl BinaryOperator {
    pub const fn as_str(self) -> &'static str {
        use BinaryOperator::*;
        match self {
            Plus => "+", Minus => "-", Mul => "*", Div => "/", Mod => "%", Pow => "**",
            Eq => "==", NotEq => "!=", StrictEq => "===", StrictNotEq => "!==",
            Lt => "<", LtEq => "<=", Gt => ">", GtEq => ">=",
            ShiftL => "<<", ShiftR => ">>", UnsignedShiftR => ">>>",
            BitAnd => "&", BitOr => "|", BitXor => "^",
            InstanceOf => "instanceof", In => "in",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogicalExpression {
    pub left: Expression,
    pub operator: LogicalOperator,
    pub right: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOperator {
    And,
    Or,
    Coalesce,
}

impl LogicalOperator {
    pub const fn as_str(self) -> &'static str {
        match self {
            LogicalOperator::And => "&&",
            LogicalOperator::Or => "||",
            LogicalOperator::Coalesce => "??",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignmentExpression {
    pub left: AssignmentTarget,
    pub operator: AssignmentOperator,
    pub right: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssignmentTarget {
    Pattern(Pattern),
    Expression(Expression),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentOperator {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    PowAssign,
    ShiftLAssign,
    ShiftRAssign,
    UnsignedShiftRAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    AndAssign,
    OrAssign,
    CoalesceAssign,
}

impl AssignmentOperator {
    pub const fn as_str(self) -> &'static str {
        use AssignmentOperator::*;
        match self {
            Assign => "=", AddAssign => "+=", SubAssign => "-=",
            MulAssign => "*=", DivAssign => "/=", ModAssign => "%=",
            PowAssign => "**=", ShiftLAssign => "<<=", ShiftRAssign => ">>=",
            UnsignedShiftRAssign => ">>>=", BitAndAssign => "&=",
            BitOrAssign => "|=", BitXorAssign => "^=",
            AndAssign => "&&=", OrAssign => "||=", CoalesceAssign => "??=",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateExpression {
    pub operator: UpdateOperator,
    pub argument: Expression,
    pub prefix: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOperator {
    Increment,
    Decrement,
}

impl UpdateOperator {
    pub const fn as_str(self) -> &'static str {
        match self {
            UpdateOperator::Increment => "++",
            UpdateOperator::Decrement => "--",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnaryExpression {
    pub operator: UnaryOperator,
    pub argument: Expression,
    pub prefix: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Minus,
    Plus,
    Not,
    BitNot,
    TypeOf,
    Void,
    Delete,
}

impl UnaryOperator {
    pub const fn as_str(self) -> &'static str {
        use UnaryOperator::*;
        match self {
            Minus => "-", Plus => "+", Not => "!", BitNot => "~",
            TypeOf => "typeof", Void => "void", Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConditionalExpression {
    pub test: Expression,
    pub consequent: Expression,
    pub alternate: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SequenceExpression {
    pub expressions: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpreadElement {
    pub argument: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct YieldExpression {
    pub argument: Option<Expression>,
    pub delegate: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AwaitExpression {
    pub argument: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaggedTemplateExpression {
    pub tag: Expression,
    pub quasi: TemplateLiteral,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParenthesizedExpression {
    pub expression: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetaProperty {
    pub meta: Identifier,
    pub property: Identifier,
    pub span: Span,
}

// -------------------------------------------------------------------------
// Patterns (for function params, destructuring, etc.)
// -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Identifier(Identifier),
    Array(Box<ArrayPattern>),
    Object(Box<ObjectPattern>),
    Rest(Box<RestElement>),
    Assignment(Box<AssignmentPattern>),
    Member(Box<MemberExpression>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArrayPattern {
    pub elements: Vec<Option<Pattern>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPattern {
    pub properties: Vec<ObjectPatternMember>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObjectPatternMember {
    Property(Box<ObjectPatternProperty>),
    Rest(Box<RestElement>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPatternProperty {
    pub key: PropertyKey,
    pub value: Pattern,
    pub computed: bool,
    pub shorthand: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestElement {
    pub argument: Pattern,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignmentPattern {
    pub left: Pattern,
    pub right: Expression,
    pub span: Span,
}
