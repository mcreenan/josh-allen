//! Public high-level IR data model emitted after semantic checking.

use super::Span;
use allen_bytecode::{
    CapabilityOperation, CheckedIntOperation, EffectSetId, FsOperation, SafeCollectionOperation,
    StringOperation, ValueType,
};

pub type SymbolId = u32;
pub type TypeId = u32;
pub type SpanId = u32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HirBundle {
    pub types: Vec<ValueType>,
    pub spans: Vec<SourceSpan>,
    pub modules: Vec<HirModule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSpan {
    pub module: String,
    pub span: Span,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HirModule {
    pub path: String,
    pub functions: Vec<HirFunction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HirFunction {
    pub symbol: SymbolId,
    pub name: String,
    pub is_async: bool,
    pub parameters: Vec<TypeId>,
    pub return_type: TypeId,
    pub effects: EffectSetId,
    pub body: HirExpr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HirExpr {
    pub kind: HirExprKind,
    pub symbol: Option<SymbolId>,
    pub ty: TypeId,
    pub effects: EffectSetId,
    pub span: SpanId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HirExprKind {
    Unit,
    Int(i64),
    Float(u64),
    Bool(bool),
    String(String),
    Template(Vec<HirTemplatePart>),
    Bytes(Vec<u8>),
    Variable,
    List(Vec<HirExpr>),
    Length(Box<HirExpr>),
    StringOperation {
        operation: StringOperation,
        arguments: Vec<HirExpr>,
    },
    CapabilityInspect {
        operation: CapabilityOperation,
        arguments: Vec<HirExpr>,
    },
    SafeCollectionOperation {
        operation: SafeCollectionOperation,
        arguments: Vec<HirExpr>,
    },
    CheckedIntOperation {
        operation: CheckedIntOperation,
        arguments: Vec<HirExpr>,
    },
    ListAppend {
        values: Box<HirExpr>,
        value: Box<HirExpr>,
    },
    ListSet {
        values: Box<HirExpr>,
        index: Box<HirExpr>,
        value: Box<HirExpr>,
    },
    Map(Vec<(HirExpr, HirExpr)>),
    Record(Vec<HirExpr>),
    Prompt(Vec<HirExpr>),
    Enum,
    FieldGet(Box<HirExpr>),
    Match {
        source: Box<HirExpr>,
        arms: Vec<HirExpr>,
    },
    If {
        condition: Box<HirExpr>,
        then_branch: Box<HirExpr>,
        else_branch: Option<Box<HirExpr>>,
    },
    While {
        condition: Box<HirExpr>,
        body: Box<HirExpr>,
    },
    Loop {
        body: Box<HirExpr>,
    },
    For {
        binding: HirLoopBinding,
        source: HirForSource,
        body: Box<HirExpr>,
    },
    Break,
    Continue,
    Try(Box<HirExpr>),
    ToUnknown(Box<HirExpr>),
    Narrow(Box<HirExpr>),
    Tuple(Vec<HirExpr>),
    Unary(Box<HirExpr>),
    Binary(Vec<HirExpr>),
    Index {
        collection: Box<HirExpr>,
        index: Box<HirExpr>,
    },
    Convert(Box<HirExpr>),
    Assignment(Box<HirExpr>),
    DirectCall(Vec<HirExpr>),
    AsyncCall(Vec<HirExpr>),
    Spawn {
        future: Box<HirExpr>,
        scope: u32,
    },
    TaskSnapshot(Box<HirExpr>),
    WorkspaceGet,
    EffectCall {
        operation: FsOperation,
        arguments: Vec<HirExpr>,
    },
    ToolCall {
        tool: u32,
        input: Box<HirExpr>,
    },
    Await(Box<HirExpr>),
    AwaitBlock {
        scope: u32,
        body: Box<HirExpr>,
    },
    Stop(Box<HirExpr>),
    Closure {
        captures: Vec<SymbolId>,
        body: Box<HirExpr>,
    },
    ClosureCall(Vec<HirExpr>),
    Block(Vec<HirExpr>),
    Return(Box<HirExpr>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HirTemplatePart {
    Literal { value: String, span: SpanId },
    Interpolation(HirExpr),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HirLoopBinding {
    pub elements: Vec<HirLoopBindingElement>,
    pub tuple: bool,
    pub span: SpanId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HirLoopBindingElement {
    pub symbol: Option<SymbolId>,
    pub ty: TypeId,
    pub span: SpanId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HirForSource {
    Iterable(Box<HirExpr>),
    Range {
        start: Box<HirExpr>,
        end: Box<HirExpr>,
    },
}
