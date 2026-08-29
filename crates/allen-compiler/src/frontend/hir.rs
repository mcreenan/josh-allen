//! Public high-level IR data model emitted after semantic checking.

use super::Span;
use allen_bytecode::{
    CapabilityOperation, CheckedIntOperation, CollectionOperation, EffectSetId, FsOperation,
    ListCombinator, SafeCollectionOperation, StandardOperation, StringOperation, ValueType,
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
    pub constants: Vec<HirConstant>,
    pub functions: Vec<HirFunction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HirConstant {
    pub symbol: SymbolId,
    pub name: String,
    pub value_type: TypeId,
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
pub enum HirListItem {
    Element(HirExpr),
    Spread(HirExpr),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HirMapItem {
    Entry { key: HirExpr, value: HirExpr },
    Spread(HirExpr),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HirExprKind {
    Unit,
    Int(i64),
    Float(u64),
    Bool(bool),
    String(String),
    Template(Vec<HirTemplatePart>),
    TemplateRender {
        template: u32,
        arguments: Vec<HirExpr>,
    },
    Bytes(Vec<u8>),
    Variable,
    List(Vec<HirExpr>),
    ListWithSpread(Vec<HirListItem>),
    Length(Box<HirExpr>),
    StringOperation {
        operation: StringOperation,
        arguments: Vec<HirExpr>,
    },
    StandardOperation {
        operation: StandardOperation,
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
    CollectionOperation {
        operation: CollectionOperation,
        arguments: Vec<HirExpr>,
    },
    ListFold {
        values: Box<HirExpr>,
        initial: Box<HirExpr>,
        callback: Box<HirExpr>,
    },
    ListCombinator {
        operation: ListCombinator,
        values: Box<HirExpr>,
        initial: Option<Box<HirExpr>>,
        callback: Box<HirExpr>,
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
    MapWithSpread(Vec<HirMapItem>),
    Record(Vec<HirExpr>),
    Prompt(Vec<HirExpr>),
    Enum,
    NewtypeWrap(Box<HirExpr>),
    NewtypeUnwrap(Box<HirExpr>),
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
    Decode(Box<HirExpr>),
    Tuple(Vec<HirExpr>),
    Unary(Box<HirExpr>),
    Binary(Vec<HirExpr>),
    Index {
        collection: Box<HirExpr>,
        index: Box<HirExpr>,
    },
    Range {
        start: Box<HirExpr>,
        end: Box<HirExpr>,
        inclusive: bool,
    },
    Slice {
        collection: Box<HirExpr>,
        range: Box<HirExpr>,
    },
    SequenceFromList(Box<HirExpr>),
    SequenceMap {
        sequence: Box<HirExpr>,
        callback: Box<HirExpr>,
    },
    SequenceFilter {
        sequence: Box<HirExpr>,
        callback: Box<HirExpr>,
    },
    SequenceTake {
        sequence: Box<HirExpr>,
        count: Box<HirExpr>,
    },
    SequenceFind {
        sequence: Box<HirExpr>,
        callback: Box<HirExpr>,
    },
    SequenceAny {
        sequence: Box<HirExpr>,
        callback: Box<HirExpr>,
    },
    SequenceAll {
        sequence: Box<HirExpr>,
        callback: Box<HirExpr>,
    },
    SequenceFold {
        sequence: Box<HirExpr>,
        initial: Box<HirExpr>,
        callback: Box<HirExpr>,
    },
    SequenceToList(Box<HirExpr>),
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
    Fail(Box<HirExpr>),
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
}
