//! Diagnostic effect origins and program debug tables.
//!
//! Every runtime-to-host provider request may carry an `origin` that says
//! where in the program the effect started: its exact source site, the
//! enclosing control flow, loop iterations, call frames, and task. Origins are
//! self-describing so a host can render them without the static tables that
//! `program/load` also returns. Neither is part of any request digest or
//! replay record.

use serde::{Deserialize, Serialize};

use crate::ProtocolError;
use crate::message::invalid;

/// Maximum number of `scope` entries in one origin. When a deeper chain is
/// cut, the innermost entries are kept and `truncated` is set.
pub const MAX_ORIGIN_SCOPE: usize = 64;

const MAX_ORIGIN_TEXT: usize = 4_096;

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Where one provider request came from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectOriginPayload {
    /// The task that owns the effect. The root task is 0; a spawned effect
    /// future is its own task.
    pub task: u64,
    /// The task that spawned `task`; absent for the root task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task: Option<u64>,
    /// The instruction that started the request.
    pub site: EffectSitePayload,
    /// Enclosing control flow, call frames, and spawns, outermost first.
    pub scope: Vec<EffectScopePayload>,
    /// Whether outer `scope` entries were dropped to respect the bound.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl EffectOriginPayload {
    /// Validate the origin's bounded, kind-consistent shape.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized or inconsistent origin.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.parent_task.is_some() == (self.task == 0) {
            return Err(invalid("origin parent task does not match its task"));
        }
        self.site.validate()?;
        if self.scope.len() > MAX_ORIGIN_SCOPE {
            return Err(invalid("origin scope exceeds its bound"));
        }
        for entry in &self.scope {
            entry.validate()?;
        }
        Ok(())
    }
}

/// A source span with optional 1-based line and column positions.
///
/// Wire payloads carry these fields inline. `start` and `end` are UTF-8 byte
/// offsets. Lines and columns are present when the runtime has the source
/// text; columns count Unicode scalar values, and `end_line`/`end_column`
/// locate the exclusive end offset.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SourceSpan {
    pub source: String,
    pub start: u32,
    pub end: u32,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

macro_rules! span_accessors {
    ($($payload:ty),+) => {$(
        impl $payload {
            /// The inline source span.
            #[must_use]
            pub fn span(&self) -> SourceSpan {
                SourceSpan {
                    source: self.source.clone(),
                    start: self.start,
                    end: self.end,
                    line: self.line,
                    column: self.column,
                    end_line: self.end_line,
                    end_column: self.end_column,
                }
            }

            /// Replace the inline source span.
            pub fn set_span(&mut self, span: SourceSpan) {
                self.source = span.source;
                self.start = span.start;
                self.end = span.end;
                self.line = span.line;
                self.column = span.column;
                self.end_line = span.end_line;
                self.end_column = span.end_column;
            }
        }
    )+};
}

span_accessors!(
    EffectSitePayload,
    EffectScopePayload,
    ProgramConstructPayload,
    ProgramRegionPayload,
    ProgramEffectSitePayload
);

impl SourceSpan {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_text(&self.source, "origin source")?;
        if self.start > self.end {
            return Err(invalid("origin span start exceeds its end"));
        }
        let positions = [self.line, self.column, self.end_line, self.end_column];
        if positions.iter().any(Option::is_some) && positions.iter().any(Option::is_none) {
            return Err(invalid("origin line and column positions are incomplete"));
        }
        if positions.contains(&Some(0)) {
            return Err(invalid("origin lines and columns are 1-based"));
        }
        if (self.line, self.column) > (self.end_line, self.end_column) {
            return Err(invalid("origin start position follows its end"));
        }
        Ok(())
    }
}

/// The instruction that started one request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectSitePayload {
    /// The static effect site from `program/load`, when the request was
    /// started by an `await` or `spawn` of a future created in the same
    /// function by one effect call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    pub function: String,
    pub instruction: u32,
    pub source: String,
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

impl EffectSitePayload {
    fn validate(&self) -> Result<(), ProtocolError> {
        validate_text(&self.function, "origin function")?;
        self.span().validate()
    }
}

/// One entry of an origin's `scope`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    For,
    While,
    Loop,
    If,
    Match,
    AwaitBlock,
    /// A call frame boundary: the frame below was called from this span.
    Call,
    /// A task boundary: the task below was spawned from this span.
    Spawn,
}

impl ScopeKind {
    /// Whether this kind is a source control-flow construct.
    #[must_use]
    pub const fn is_construct(self) -> bool {
        !matches!(self, Self::Call | Self::Spawn)
    }

    /// Whether this kind is a loop.
    #[must_use]
    pub const fn is_loop(self) -> bool {
        matches!(self, Self::For | Self::While | Self::Loop)
    }

    const fn allows_region(self, region: ScopeRegion) -> bool {
        matches!(
            (self, region),
            (Self::For | Self::Loop | Self::AwaitBlock, ScopeRegion::Body)
                | (Self::While, ScopeRegion::Condition | ScopeRegion::Body)
                | (
                    Self::If,
                    ScopeRegion::Condition | ScopeRegion::Then | ScopeRegion::Else
                )
                | (Self::Match, ScopeRegion::Scrutinee | ScopeRegion::Arm)
        )
    }
}

/// The part of a construct that contains the site or call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeRegion {
    Condition,
    Body,
    Then,
    Else,
    Scrutinee,
    Arm,
}

/// The taken branch of an `if`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IfBranch {
    Then,
    Else,
}

/// One enclosing construct, call frame boundary, or task boundary.
///
/// Construct entries (`for`, `while`, `loop`, `if`, `match`, `await_block`)
/// carry `construct` and `region`. Loops add `instance` and the 1-based
/// `iteration`; `if` adds `branch` inside a branch; `match` adds the 0-based
/// `arm` inside an arm. `call` entries name the called `function` and the
/// calling `instruction`; `spawn` entries also name the spawned `task`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectScopePayload {
    pub kind: ScopeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub construct: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<u64>,
    pub source: String,
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<ScopeRegion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<IfBranch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u64>,
}

impl EffectScopePayload {
    fn validate(&self) -> Result<(), ProtocolError> {
        self.span().validate()?;
        if let Some(function) = &self.function {
            validate_text(function, "origin function")?;
        }
        let construct = self.kind.is_construct();
        let consistent = self.construct.is_some() == construct
            && self.function.is_some() != construct
            && self.instruction.is_some() != construct
            && self.task.is_some() == (self.kind == ScopeKind::Spawn)
            && self
                .region
                .is_none_or(|region| construct && self.kind.allows_region(region))
            && self.branch.is_some()
                == (self.kind == ScopeKind::If
                    && matches!(self.region, Some(ScopeRegion::Then | ScopeRegion::Else)))
            && self.branch.is_none_or(|branch| {
                matches!(
                    (branch, self.region),
                    (IfBranch::Then, Some(ScopeRegion::Then))
                        | (IfBranch::Else, Some(ScopeRegion::Else))
                )
            })
            && self.arm.is_some() == (self.region == Some(ScopeRegion::Arm))
            && self.instance.is_some() == self.kind.is_loop()
            && self.iteration.is_some() == self.kind.is_loop()
            && self.instance != Some(0)
            && self.iteration != Some(0);
        if !consistent {
            return Err(invalid("origin scope entry does not match its kind"));
        }
        Ok(())
    }
}

/// Static control-flow and effect-site tables for one loaded program.
///
/// Present when the artifact has debug information. Construct IDs match the
/// `construct` of origin scope entries; effect-site IDs match `site.id`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramDebugPayload {
    pub constructs: Vec<ProgramConstructPayload>,
    pub effect_sites: Vec<ProgramEffectSitePayload>,
    /// Whether either table was cut to respect the response frame budget.
    pub truncated: bool,
}

impl ProgramDebugPayload {
    /// Validate the tables' identifiers and spans.
    ///
    /// # Errors
    ///
    /// Returns an error for unordered identifiers or invalid spans.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        let mut previous = None;
        for construct in &self.constructs {
            if previous.is_some_and(|previous| previous >= construct.id)
                || construct
                    .parent
                    .is_some_and(|parent| parent >= construct.id)
            {
                return Err(invalid("program construct identifiers are invalid"));
            }
            previous = Some(construct.id);
            validate_text(&construct.function, "program construct function")?;
            construct.span().validate()?;
            for region in &construct.regions {
                region.span().validate()?;
                if !construct.kind.allows_region(region.region)
                    || region.arm.is_some() != (region.region == ScopeRegion::Arm)
                {
                    return Err(invalid("program construct region is invalid"));
                }
            }
        }
        let mut previous = None;
        for site in &self.effect_sites {
            if previous.is_some_and(|previous| previous >= site.id) {
                return Err(invalid("program effect-site identifiers are invalid"));
            }
            previous = Some(site.id);
            validate_text(&site.function, "program effect-site function")?;
            validate_text(&site.operation, "program effect-site operation")?;
            site.span().validate()?;
        }
        Ok(())
    }
}

/// One source control-flow construct.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramConstructPayload {
    pub id: u32,
    pub kind: ScopeKind,
    pub function: String,
    pub parent: Option<u32>,
    pub source: String,
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    pub regions: Vec<ProgramRegionPayload>,
}

/// One named region of a construct.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramRegionPayload {
    pub region: ScopeRegion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<u32>,
    pub source: String,
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

/// The effect family of a static effect site.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectSiteKind {
    Tool,
    Model,
    User,
    Agent,
    SubAgent,
    Permission,
    Fs,
    Http,
    Exec,
}

/// One effect call in the program: the instruction that creates the effect
/// future, and the `await` or `spawn` that starts it when that happens in the
/// same function.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramEffectSitePayload {
    pub id: u32,
    pub kind: EffectSiteKind,
    /// The source operation, such as `model.request` or `tools.workspace.read.call`.
    pub operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub function: String,
    pub instruction: u32,
    pub start_instruction: Option<u32>,
    pub source: String,
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    /// Enclosing constructs, outermost first.
    pub constructs: Vec<u32>,
}

fn validate_text(value: &str, name: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty() || value.len() > MAX_ORIGIN_TEXT || value.chars().any(char::is_control) {
        return Err(invalid(name));
    }
    Ok(())
}
