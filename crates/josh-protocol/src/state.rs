use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::payload::{
    AgentAskParams, AgentMessageParams, AgentTranscriptParams, ExecutionEventParams,
    ModelRequestParams, PermissionRequestParams, PermissionRevokeParams, SubAgentAskParams,
    SubAgentCreateParams, SubAgentMessageParams, SubAgentRunParams, UserAskParams, Validate,
};
use crate::{
    ExecutionMode, InitializeParams, WireErrorCode, WireMessage, notification_params,
    request_params,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerRole {
    Host,
    Runtime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    New,
    Initialized,
    Projected,
    CatalogFrozen,
    ProgramLoaded,
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiveAction {
    RequestAccepted,
    ResponseAccepted { method: String },
    NotificationAccepted,
    CancelObserved { active: bool, first: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestStateError {
    pub code: WireErrorCode,
    pub fatal: bool,
    pub message: String,
}

impl RequestStateError {
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            code: WireErrorCode::ProtocolViolation,
            fatal: true,
            message: message.into(),
        }
    }

    fn request(code: WireErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            fatal: false,
            message: message.into(),
        }
    }
}

impl fmt::Display for RequestStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RequestStateError {}

/// Tracks direction-scoped active IDs and the negotiated JOSH connection state.
pub struct ProtocolTracker {
    role: PeerRole,
    state: ConnectionState,
    max_active_requests: usize,
    incoming: BTreeMap<String, String>,
    outgoing: BTreeMap<String, String>,
    cancelled_incoming: BTreeSet<String>,
    late_outgoing: BTreeSet<String>,
    late_order: VecDeque<String>,
    late_limit: usize,
    active_execution: bool,
    active_execution_id: Option<String>,
    execution_mode: Option<ExecutionMode>,
    invoking_session_id: Option<String>,
}

impl ProtocolTracker {
    #[must_use]
    pub fn new(role: PeerRole, max_active_requests: usize) -> Self {
        Self {
            role,
            state: ConnectionState::New,
            max_active_requests,
            incoming: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            cancelled_incoming: BTreeSet::new(),
            late_outgoing: BTreeSet::new(),
            late_order: VecDeque::new(),
            late_limit: max_active_requests.max(1),
            active_execution: false,
            active_execution_id: None,
            execution_mode: None,
            invoking_session_id: None,
        }
    }

    #[must_use]
    pub const fn state(&self) -> ConnectionState {
        self.state
    }

    #[must_use]
    pub fn active_incoming(&self) -> usize {
        self.incoming.len()
    }

    #[must_use]
    pub fn active_outgoing(&self) -> usize {
        self.outgoing.len()
    }

    #[must_use]
    pub const fn execution_mode(&self) -> Option<ExecutionMode> {
        self.execution_mode
    }

    #[must_use]
    pub fn invoking_session_id(&self) -> Option<&str> {
        self.invoking_session_id.as_deref()
    }

    pub fn set_max_active_requests(&mut self, limit: usize) {
        self.max_active_requests = limit;
        self.late_limit = limit.max(1);
    }

    /// Registers a locally created request before it is written.
    ///
    /// # Errors
    ///
    /// Returns a wire-state error for an invalid method, state, duplicate ID, or limit.
    pub fn register_outgoing_request(
        &mut self,
        id: &str,
        method: &str,
    ) -> Result<(), RequestStateError> {
        if is_bound_request(method) {
            return Err(RequestStateError::request(
                WireErrorCode::RequestInvalid,
                "bound requests must be registered with their complete message",
            ));
        }
        self.check_method(method, false)?;
        self.insert_outgoing(id, method)
    }

    fn insert_outgoing(&mut self, id: &str, method: &str) -> Result<(), RequestStateError> {
        if self.outgoing.contains_key(id) {
            return Err(RequestStateError::fatal(
                "duplicate active outgoing request ID",
            ));
        }
        if self.outgoing.len() >= self.max_active_requests {
            return Err(RequestStateError::request(
                WireErrorCode::RequestLimit,
                "outgoing active request limit reached",
            ));
        }
        self.outgoing.insert(id.to_owned(), method.to_owned());
        Ok(())
    }

    /// Registers a locally created request and validates its identity binding.
    ///
    /// # Errors
    ///
    /// Returns a wire-state error for an invalid envelope, payload, binding, or state.
    pub fn register_outgoing_message(
        &mut self,
        message: &WireMessage,
    ) -> Result<(), RequestStateError> {
        let WireMessage::Request { id, method, .. } = message else {
            return Err(RequestStateError::request(
                WireErrorCode::RequestInvalid,
                "outgoing message is not a request",
            ));
        };
        self.check_method(method, false)?;
        self.validate_request_binding(message)?;
        self.insert_outgoing(id, method)
    }

    /// Applies one validated incoming message to direction-scoped request state.
    ///
    /// # Errors
    ///
    /// Returns a fatal or request-scoped error for an invalid direction or state.
    pub fn receive(&mut self, message: &WireMessage) -> Result<ReceiveAction, RequestStateError> {
        match message {
            WireMessage::Request { .. } => self.receive_request(message),
            WireMessage::Response { id, .. } => self.receive_response(id),
            WireMessage::Notification { method, .. } => {
                self.check_notification(method, true)?;
                self.validate_notification_binding(message)?;
                Ok(ReceiveAction::NotificationAccepted)
            }
            WireMessage::Cancel { id, .. } => {
                let active = self.incoming.contains_key(id);
                let first = active && self.cancelled_incoming.insert(id.clone());
                Ok(ReceiveAction::CancelObserved { active, first })
            }
        }
    }

    /// Validates one locally created notification against direction, state, and binding.
    ///
    /// # Errors
    ///
    /// Returns a fatal protocol error when the notification cannot be sent on
    /// this connection.
    pub fn validate_outgoing_notification(
        &self,
        message: &WireMessage,
    ) -> Result<(), RequestStateError> {
        let WireMessage::Notification { method, .. } = message else {
            return Err(RequestStateError::fatal(
                "outgoing message is not a notification",
            ));
        };
        self.check_notification(method, false)?;
        self.validate_notification_binding(message)
    }

    /// Commits the one terminal response for an incoming request.
    ///
    /// # Errors
    ///
    /// Returns a fatal error when `id` is not active in the incoming direction.
    pub fn commit_response(&mut self, id: &str) -> Result<String, RequestStateError> {
        let method = self
            .incoming
            .remove(id)
            .ok_or_else(|| RequestStateError::fatal("response commits an unknown incoming ID"))?;
        self.cancelled_incoming.remove(id);
        Ok(method)
    }

    #[must_use]
    pub fn is_cancelled(&self, id: &str) -> bool {
        self.cancelled_incoming.contains(id)
    }

    pub fn cancel_outgoing(&mut self, id: &str) -> bool {
        if self.outgoing.remove(id).is_none() {
            return false;
        }
        if self.late_outgoing.insert(id.to_owned()) {
            self.late_order.push_back(id.to_owned());
        }
        while self.late_order.len() > self.late_limit {
            if let Some(expired) = self.late_order.pop_front() {
                self.late_outgoing.remove(&expired);
            }
        }
        true
    }

    /// Records successful initialization.
    ///
    /// # Errors
    ///
    /// Returns a fatal error when the connection is not new.
    pub fn initialize_succeeded(&mut self) -> Result<(), RequestStateError> {
        self.initialize_binding(ExecutionMode::Unattended, None)
    }

    /// Records successful initialization and freezes the offered minor and session.
    ///
    /// # Errors
    ///
    /// Returns an error when params are invalid or the connection is not new.
    pub fn initialize_succeeded_with(
        &mut self,
        params: &InitializeParams,
    ) -> Result<(), RequestStateError> {
        params.validate().map_err(|error| {
            RequestStateError::request(WireErrorCode::RequestInvalid, error.to_string())
        })?;
        self.initialize_binding(
            params.execution_mode,
            params.bound_session_id().map(str::to_owned),
        )
    }

    fn initialize_binding(
        &mut self,
        mode: ExecutionMode,
        invoking_session_id: Option<String>,
    ) -> Result<(), RequestStateError> {
        if self.state != ConnectionState::New {
            return Err(RequestStateError::fatal(
                "initialize state transition is invalid",
            ));
        }
        self.execution_mode = Some(mode);
        self.invoking_session_id = invoking_session_id;
        self.state = ConnectionState::Initialized;
        Ok(())
    }

    /// Records a successfully frozen catalog.
    ///
    /// # Errors
    ///
    /// Returns a fatal error when the connection is not initialized.
    pub fn catalog_succeeded(&mut self) -> Result<(), RequestStateError> {
        if self.state != ConnectionState::Projected {
            return Err(RequestStateError::fatal(
                "catalog state transition is invalid",
            ));
        }
        self.state = ConnectionState::CatalogFrozen;
        Ok(())
    }

    /// Records a successfully frozen host projection.
    ///
    /// # Errors
    ///
    /// Returns a fatal error when the connection is not initialized.
    pub fn projection_succeeded(&mut self) -> Result<(), RequestStateError> {
        if self.state != ConnectionState::Initialized {
            return Err(RequestStateError::fatal(
                "host projection state transition is invalid",
            ));
        }
        self.state = ConnectionState::Projected;
        Ok(())
    }

    /// Records a successful program load.
    ///
    /// # Errors
    ///
    /// Returns a fatal error when no catalog is frozen.
    pub fn program_loaded(&mut self) -> Result<(), RequestStateError> {
        if !matches!(
            self.state,
            ConnectionState::CatalogFrozen | ConnectionState::ProgramLoaded
        ) {
            return Err(RequestStateError::fatal(
                "program state transition is invalid",
            ));
        }
        self.state = ConnectionState::ProgramLoaded;
        Ok(())
    }

    /// Records one accepted active execution.
    ///
    /// # Errors
    ///
    /// Returns a request-state error when no program is loaded or one execution is active.
    pub fn execution_started(&mut self) -> Result<(), RequestStateError> {
        self.execution_started_binding(None)
    }

    /// Records an accepted execution and freezes its host-selected ID.
    ///
    /// # Errors
    ///
    /// Returns an error when no program is loaded, another execution is active,
    /// or the execution ID is invalid.
    pub fn execution_started_with(&mut self, execution_id: &str) -> Result<(), RequestStateError> {
        if execution_id.is_empty()
            || execution_id.len() > 128
            || execution_id.chars().any(char::is_control)
        {
            return Err(RequestStateError::request(
                WireErrorCode::RequestInvalid,
                "execution ID is invalid",
            ));
        }
        self.execution_started_binding(Some(execution_id.to_owned()))
    }

    fn execution_started_binding(
        &mut self,
        execution_id: Option<String>,
    ) -> Result<(), RequestStateError> {
        if self.state != ConnectionState::ProgramLoaded || self.active_execution {
            return Err(RequestStateError::request(
                WireErrorCode::RequestInvalidState,
                "execution cannot start in the current state",
            ));
        }
        self.active_execution = true;
        self.active_execution_id = execution_id;
        Ok(())
    }

    pub fn execution_finished(&mut self) {
        self.active_execution = false;
        self.active_execution_id = None;
    }

    pub fn disconnect(&mut self) {
        self.state = ConnectionState::Disconnected;
        self.incoming.clear();
        self.outgoing.clear();
        self.cancelled_incoming.clear();
        self.late_outgoing.clear();
        self.late_order.clear();
        self.active_execution = false;
        self.active_execution_id = None;
        self.execution_mode = None;
        self.invoking_session_id = None;
    }

    fn receive_request(
        &mut self,
        message: &WireMessage,
    ) -> Result<ReceiveAction, RequestStateError> {
        let WireMessage::Request { id, method, .. } = message else {
            unreachable!("receive_request is called only for requests");
        };
        if self.incoming.contains_key(id) {
            return Err(RequestStateError::fatal(
                "duplicate active incoming request ID",
            ));
        }
        self.check_method(method, true)?;
        self.validate_request_binding(message)?;
        if matches!(
            method.as_str(),
            "initialize" | "host/project" | "catalog/set"
        ) && self
            .incoming
            .values()
            .chain(self.outgoing.values())
            .any(|active| active == method)
        {
            return Err(RequestStateError::request(
                WireErrorCode::RequestInvalidState,
                "singleton request method is already active",
            ));
        }
        if self.incoming.len() >= self.max_active_requests {
            return Err(RequestStateError::request(
                WireErrorCode::RequestLimit,
                "incoming active request limit reached",
            ));
        }
        self.incoming.insert(id.to_owned(), method.to_owned());
        Ok(ReceiveAction::RequestAccepted)
    }

    fn receive_response(&mut self, id: &str) -> Result<ReceiveAction, RequestStateError> {
        if let Some(method) = self.outgoing.remove(id) {
            return Ok(ReceiveAction::ResponseAccepted { method });
        }
        if self.late_outgoing.remove(id) {
            self.late_order.retain(|candidate| candidate != id);
            return Err(RequestStateError::fatal(
                "response arrived after its outgoing request was cancelled",
            ));
        }
        Err(RequestStateError::fatal(
            "response has an unknown or terminal request ID",
        ))
    }

    fn validate_request_binding(&self, message: &WireMessage) -> Result<(), RequestStateError> {
        let WireMessage::Request { method, .. } = message else {
            return Err(RequestStateError::request(
                WireErrorCode::RequestInvalid,
                "expected request message",
            ));
        };
        let binding = match method.as_str() {
            "agent/message" => {
                let params: AgentMessageParams = parse_request(message, method)?;
                Some((params.execution_id, params.session_id))
            }
            "agent/ask" => {
                let params: AgentAskParams = parse_request(message, method)?;
                Some((
                    params.execution_id().to_owned(),
                    params.session_id().to_owned(),
                ))
            }
            "agent/transcript" => {
                let params: AgentTranscriptParams = parse_request(message, method)?;
                Some((params.execution_id, params.session_id))
            }
            "permission/request" => {
                let params: PermissionRequestParams = parse_request(message, method)?;
                Some((params.execution_id, params.session_id))
            }
            "model/request" => {
                let params: ModelRequestParams = parse_request(message, method)?;
                self.ensure_active_execution(&params.execution_id)?;
                None
            }
            "user/ask" => {
                let params: UserAskParams = parse_request(message, method)?;
                self.ensure_active_execution(&params.execution_id)?;
                None
            }
            "sub_agent/create" => {
                let params: SubAgentCreateParams = parse_request(message, method)?;
                self.ensure_active_execution(&params.execution_id)?;
                None
            }
            "sub_agent/run" => {
                let params: SubAgentRunParams = parse_request(message, method)?;
                self.ensure_active_execution(&params.execution_id)?;
                None
            }
            "sub_agent/message" => {
                let params: SubAgentMessageParams = parse_request(message, method)?;
                self.ensure_active_execution(&params.execution_id)?;
                None
            }
            "sub_agent/ask" => {
                let params: SubAgentAskParams = parse_request(message, method)?;
                self.ensure_active_execution(&params.execution_id)?;
                None
            }
            _ => None,
        };
        if let Some((execution_id, session_id)) = binding {
            self.ensure_bound_identities(&execution_id, &session_id)?;
        }
        Ok(())
    }

    fn validate_notification_binding(
        &self,
        message: &WireMessage,
    ) -> Result<(), RequestStateError> {
        let WireMessage::Notification { method, .. } = message else {
            return Err(RequestStateError::fatal("expected notification message"));
        };
        match method.as_str() {
            "permission/revoke" => {
                let params: PermissionRevokeParams =
                    notification_params(message, method).map_err(|error| {
                        RequestStateError::fatal(format!(
                            "permission revocation is malformed: {error}"
                        ))
                    })?;
                self.ensure_bound_identities(&params.execution_id, &params.session_id)
            }
            "execution/event" => {
                let params: ExecutionEventParams =
                    notification_params(message, method).map_err(|error| {
                        RequestStateError::fatal(format!("execution event is malformed: {error}"))
                    })?;
                if self
                    .active_execution_id
                    .as_deref()
                    .is_some_and(|execution_id| execution_id != params.execution_id)
                {
                    return Err(RequestStateError::fatal(
                        "execution event does not match the active execution",
                    ));
                }
                if params.kind == crate::payload::EventKind::PermissionDecision
                    && self.active_execution_id.is_none()
                {
                    return Err(RequestStateError::fatal(
                        "permission decision event requires an active execution",
                    ));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn ensure_bound_identities(
        &self,
        execution_id: &str,
        session_id: &str,
    ) -> Result<(), RequestStateError> {
        if self.active_execution_id.as_deref() != Some(execution_id)
            || self.invoking_session_id.as_deref() != Some(session_id)
        {
            return Err(RequestStateError::fatal(
                "message identities do not match the active attached binding",
            ));
        }
        Ok(())
    }

    fn ensure_active_execution(&self, execution_id: &str) -> Result<(), RequestStateError> {
        if self.active_execution_id.as_deref() != Some(execution_id) {
            return Err(RequestStateError::fatal(
                "message execution identity does not match the active execution",
            ));
        }
        Ok(())
    }

    fn check_method(&self, method: &str, incoming: bool) -> Result<(), RequestStateError> {
        let expected_sender = match (self.role, incoming) {
            (PeerRole::Host, true) | (PeerRole::Runtime, false) => PeerRole::Runtime,
            (PeerRole::Host, false) | (PeerRole::Runtime, true) => PeerRole::Host,
        };
        let defined_sender = match method {
            "initialize" | "host/project" | "catalog/set" | "program/load" | "execution/start" => {
                PeerRole::Host
            }
            "tool/invoke" | "agent/message" | "agent/ask" | "agent/transcript"
            | "permission/request" | "model/request" | "user/ask" | "sub_agent/create"
            | "sub_agent/run" | "sub_agent/message" | "sub_agent/ask" => PeerRole::Runtime,
            _ => {
                return Err(RequestStateError::request(
                    WireErrorCode::RequestMethodNotFound,
                    "request method is unknown",
                ));
            }
        };
        if defined_sender != expected_sender {
            return Err(RequestStateError::request(
                WireErrorCode::RequestMethodNotFound,
                "request method has the wrong direction",
            ));
        }
        let valid_state = match method {
            "initialize" => self.state == ConnectionState::New,
            "host/project" => self.state == ConnectionState::Initialized,
            "catalog/set" => self.state == ConnectionState::Projected,
            "program/load" => matches!(
                self.state,
                ConnectionState::CatalogFrozen | ConnectionState::ProgramLoaded
            ),
            "execution/start" => {
                self.state == ConnectionState::ProgramLoaded && !self.active_execution
            }
            "tool/invoke" => self.state == ConnectionState::ProgramLoaded && self.active_execution,
            "agent/message" | "agent/ask" | "agent/transcript" | "permission/request" => {
                self.state == ConnectionState::ProgramLoaded
                    && self.active_execution
                    && self.execution_mode == Some(ExecutionMode::Attached)
                    && self.invoking_session_id.is_some()
                    && self.active_execution_id.is_some()
            }
            "model/request" | "user/ask" | "sub_agent/create" | "sub_agent/run"
            | "sub_agent/message" | "sub_agent/ask" => {
                self.state == ConnectionState::ProgramLoaded
                    && self.active_execution
                    && self.active_execution_id.is_some()
            }
            _ => false,
        };
        if !valid_state {
            return Err(RequestStateError::request(
                WireErrorCode::RequestInvalidState,
                "request method is invalid in the current state",
            ));
        }
        Ok(())
    }

    fn check_notification(&self, method: &str, incoming: bool) -> Result<(), RequestStateError> {
        let sender = match (self.role, incoming) {
            (PeerRole::Host, true) | (PeerRole::Runtime, false) => PeerRole::Runtime,
            (PeerRole::Host, false) | (PeerRole::Runtime, true) => PeerRole::Host,
        };
        match method {
            "runtime/ready"
                if sender == PeerRole::Runtime && self.state == ConnectionState::New =>
            {
                Ok(())
            }
            "execution/event"
                if sender == PeerRole::Runtime
                    && self.state == ConnectionState::ProgramLoaded
                    && self.active_execution =>
            {
                Ok(())
            }
            "permission/revoke"
                if sender == PeerRole::Host
                    && self.state == ConnectionState::ProgramLoaded
                    && self.active_execution
                    && self.execution_mode == Some(ExecutionMode::Attached)
                    && self.invoking_session_id.is_some()
                    && self.active_execution_id.is_some() =>
            {
                Ok(())
            }
            "runtime/ready" | "execution/event" | "permission/revoke" => {
                Err(RequestStateError::fatal(
                    "notification is invalid in the current state or direction",
                ))
            }
            _ => Err(RequestStateError::fatal("notification method is unknown")),
        }
    }
}

fn parse_request<T>(message: &WireMessage, method: &str) -> Result<T, RequestStateError>
where
    T: serde::de::DeserializeOwned + Validate,
{
    request_params(message, method).map_err(|error| {
        RequestStateError::request(
            WireErrorCode::RequestInvalid,
            format!("{method} params are invalid: {error}"),
        )
    })
}

fn is_bound_request(method: &str) -> bool {
    matches!(
        method,
        "agent/message"
            | "agent/ask"
            | "agent/transcript"
            | "permission/request"
            | "model/request"
            | "user/ask"
            | "sub_agent/create"
            | "sub_agent/run"
            | "sub_agent/message"
            | "sub_agent/ask"
    )
}
