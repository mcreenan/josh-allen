use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use allen_runtime::{
    AgentAskCall, AgentCancellationSignal, AgentHostError, AgentMessageCall, AgentProviderPoll,
    ExternalGrantDecision, ExternalGrantDecisionProvider, ExternalGrantId, ExternalGrantPoll,
    ExternalGrantRequest, GrantDuration as RuntimeGrantDuration, InvokingAgentProvider,
    PromptPayload, ResponseHostError, ResponseProvider, ResponseProviderPoll, RuntimeProviders,
    SubAgentAskCall, SubAgentCreateCall, SubAgentHostError, SubAgentId, SubAgentMessageCall,
    SubAgentProjection, SubAgentProvider, SubAgentProviderPoll, SubAgentRunCall,
    ToolCancellationSignal, ToolHostError, ToolInvocation, ToolOutcome, ToolProvider,
    ToolProviderPoll, TranscriptMessage as RuntimeTranscriptMessage,
    TranscriptPart as RuntimeTranscriptPart, TranscriptQuery,
    TranscriptRole as RuntimeTranscriptRole, TranscriptSnapshot as RuntimeTranscriptSnapshot,
};
use allen_sandbox_fs::{ExternalTargetKind, Rights};
use allen_schema::ToolName;
use allen_vm::{
    BudgetWarning, Checkpoint, CheckpointObserver, PendingEffectId, TaskEvent, TaskEventKind,
    VmError,
};
use josh_protocol::{
    AgentAskParams, AgentMessageParams, AgentMessageResult, AgentTranscriptParams,
    AgentTranscriptResult, CatalogSetParams, EventKind, ExecutionEventParams, ExecutionResult,
    ExecutionStartParams, FrameReader, GrantDuration, InitializeParams, ModelRequestParams,
    PeerInfo, PermissionRequestParams, PermissionRequestResult, PermissionRevokeParams,
    PermissionRight, PermissionTargetKind, ProgramLoadParams, PromptPolicy, PromptSegmentPayload,
    ProtocolError, ProtocolTracker, ReceiveAction, ResponseSchemaPayload, RuntimeReadyParams,
    StructuredPromptPayload, SubAgentAskParams, SubAgentCreateParams, SubAgentCreateResult,
    SubAgentMessageParams, SubAgentProjectionPayload, SubAgentRunParams, ToolInvokeParams,
    ToolInvokeResult, TranscriptPart, TranscriptRole, TypedResponseResult, UserAskParams, Validate,
    ValidationIssuePayload, WireError, WireErrorCode, WireMessage, encode_frame,
    notification_params, request_params, response_result,
};
use serde_json::Value;

use crate::events::{EventClock, SystemEventClock, execution_event};
use crate::grants::GrantRegistry;
use crate::{HostError, PreparedExecution, Session};

/// Runs one permanent JOSH connection over full-duplex byte streams.
///
/// # Errors
///
/// Returns a safe protocol error after cleanup on framing, state, or I/O failure.
#[allow(clippy::too_many_lines)]
pub fn run_connection<R, W>(input: R, output: W) -> Result<(), HostError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let disconnected = Arc::new(AtomicBool::new(false));
    let session = Arc::new(Mutex::new(Session::new()));
    let tracker = Arc::new(Mutex::new(ProtocolTracker::new(
        josh_protocol::PeerRole::Runtime,
        64,
    )));
    let commit_gate = Arc::new(Mutex::new(()));
    let pending = Arc::new(PendingRegistry::default());
    let grants = Arc::new(GrantRegistry::default());
    let writer = Arc::new(ConnectionWriter::new(
        output,
        josh_protocol::DEFAULT_MAX_FRAME_BYTES,
        Arc::clone(&disconnected),
        Arc::clone(&session),
        Arc::clone(&tracker),
        Arc::clone(&pending),
    ));
    let (reader, reader_commands) = spawn_reader(input);
    let outgoing_ids = Arc::new(AtomicU64::new(1));
    let mut executions = Vec::new();

    let result = (|| -> Result<(), HostError> {
        let ready = RuntimeReadyParams {
            runtime: PeerInfo {
                name: "allen-reference".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        };
        write_notification(&writer, "runtime/ready", &ready)?;

        loop {
            let message = match receive_input(&reader, &disconnected)? {
                Ok(Some(message)) => message,
                Ok(None) => {
                    disconnected.store(true, Ordering::Release);
                    break;
                }
                Err(_) => {
                    disconnected.store(true, Ordering::Release);
                    cancel_active(&session, "");
                    return Err(HostError {
                        code: WireErrorCode::ProtocolViolation,
                        message: "protocol frame is invalid",
                    });
                }
            };
            let action = {
                let mut tracker = lock_tracker(&tracker)?;
                tracker.receive(&message)
            };
            let action = match action {
                Ok(action) => action,
                Err(error) if error.fatal => {
                    disconnected.store(true, Ordering::Release);
                    cancel_active(&session, "");
                    return Err(HostError {
                        code: WireErrorCode::ProtocolViolation,
                        message: "protocol state is invalid",
                    });
                }
                Err(error) => {
                    if let WireMessage::Request { id, .. } = &message {
                        write_wire_error(
                            &writer,
                            id,
                            WireError {
                                code: error.code,
                                message: error.message,
                                data: None,
                            },
                        )?;
                        reader_commands
                            .send(ReaderCommand::Continue(None))
                            .map_err(|_| protocol_write_error())?;
                        continue;
                    }
                    return Err(HostError {
                        code: WireErrorCode::ProtocolViolation,
                        message: "protocol state is invalid",
                    });
                }
            };
            match (&message, action) {
                (
                    WireMessage::Request {
                        id,
                        method,
                        params: _,
                    },
                    ReceiveAction::RequestAccepted,
                ) => match method.as_str() {
                    "initialize" => {
                        let result = request_params::<InitializeParams>(&message, "initialize")
                            .map_err(invalid_request)
                            .and_then(|params| {
                                let result = lock_session(&session)?.initialize(&params)?;
                                Ok((result, params))
                            });
                        match result {
                            Ok((result, params)) => {
                                write_result(&writer, id, &result)?;
                                let limits = lock_session(&session)?.effective_limits();
                                let reader_limit =
                                    usize::try_from(limits.max_frame_bytes).unwrap_or(usize::MAX);
                                writer.set_max_frame_bytes(
                                    usize::try_from(limits.max_frame_bytes).unwrap_or(usize::MAX),
                                );
                                let mut state = lock_tracker(&tracker)?;
                                state.commit_response(id).map_err(state_error)?;
                                state
                                    .initialize_succeeded_with(&params)
                                    .map_err(state_error)?;
                                state.set_max_active_requests(
                                    usize::try_from(limits.max_active_requests)
                                        .unwrap_or(usize::MAX),
                                );
                                reader_commands
                                    .send(ReaderCommand::Continue(Some(reader_limit)))
                                    .map_err(|_| protocol_write_error())?;
                                continue;
                            }
                            Err(error) => finish_failed_request(&writer, &tracker, id, error)?,
                        }
                    }
                    "catalog/set" => {
                        let result = request_params::<CatalogSetParams>(&message, "catalog/set")
                            .map_err(invalid_request)
                            .and_then(|params| lock_session(&session)?.set_catalog(&params));
                        match result {
                            Ok(result) => {
                                write_result(&writer, id, &result)?;
                                let mut state = lock_tracker(&tracker)?;
                                state.commit_response(id).map_err(state_error)?;
                                state.catalog_succeeded().map_err(state_error)?;
                            }
                            Err(error) => finish_failed_request(&writer, &tracker, id, error)?,
                        }
                    }
                    "program/load" => {
                        let result = request_params::<ProgramLoadParams>(&message, "program/load")
                            .map_err(invalid_request)
                            .and_then(|params| lock_session(&session)?.load_program(&params));
                        match result {
                            Ok(result) => {
                                write_result(&writer, id, &result)?;
                                let mut state = lock_tracker(&tracker)?;
                                state.commit_response(id).map_err(state_error)?;
                                state.program_loaded().map_err(state_error)?;
                            }
                            Err(error) => finish_failed_request(&writer, &tracker, id, error)?,
                        }
                    }
                    "execution/start" => {
                        let prepared =
                            request_params::<ExecutionStartParams>(&message, "execution/start")
                                .map_err(invalid_request)
                                .and_then(|params| {
                                    lock_session(&session)?.prepare_execution(id.clone(), params)
                                });
                        match prepared {
                            Ok(prepared) => {
                                lock_tracker(&tracker)?
                                    .execution_started_with(prepared.execution_id())
                                    .map_err(state_error)?;
                                let catalog_digest = lock_session(&session)?
                                    .catalog_digest()
                                    .unwrap_or_default()
                                    .to_owned();
                                if let Err(error) = write_event(
                                    &writer,
                                    &execution_event(
                                        prepared.execution_id(),
                                        1,
                                        0,
                                        EventKind::Accepted,
                                        false,
                                        BTreeMap::from([
                                            (
                                                "artifact_digest".to_owned(),
                                                Value::String(
                                                    prepared.artifact_digest().to_owned(),
                                                ),
                                            ),
                                            (
                                                "catalog_digest".to_owned(),
                                                Value::String(catalog_digest),
                                            ),
                                            (
                                                "entry".to_owned(),
                                                Value::String(prepared.entry().to_owned()),
                                            ),
                                            (
                                                "program_id".to_owned(),
                                                Value::String(prepared.program_id().to_owned()),
                                            ),
                                        ]),
                                    ),
                                ) {
                                    lock_session(&session)?.finish_execution(id);
                                    lock_tracker(&tracker)?.execution_finished();
                                    return Err(error);
                                }
                                executions.push(spawn_execution(
                                    prepared,
                                    Arc::clone(&writer),
                                    Arc::clone(&session),
                                    Arc::clone(&tracker),
                                    Arc::clone(&commit_gate),
                                    Arc::clone(&pending),
                                    Arc::clone(&disconnected),
                                    Arc::clone(&outgoing_ids),
                                    Arc::clone(&grants),
                                ));
                            }
                            Err(error) => finish_failed_request(&writer, &tracker, id, error)?,
                        }
                    }
                    _ => {
                        finish_failed_request(
                            &writer,
                            &tracker,
                            id,
                            HostError {
                                code: WireErrorCode::RequestMethodNotFound,
                                message: "request method is unknown",
                            },
                        )?;
                    }
                },
                (
                    WireMessage::Cancel { id, .. },
                    ReceiveAction::CancelObserved { active, first },
                ) => {
                    if active && first {
                        let _gate = commit_gate.lock().map_err(|_| poisoned())?;
                        cancel_active(&session, id);
                    }
                }
                (WireMessage::Response { id, .. }, ReceiveAction::ResponseAccepted { method })
                    if matches!(
                        method.as_str(),
                        "tool/invoke"
                            | "agent/message"
                            | "agent/ask"
                            | "agent/transcript"
                            | "model/request"
                            | "permission/request"
                            | "user/ask"
                            | "sub_agent/create"
                            | "sub_agent/run"
                            | "sub_agent/message"
                            | "sub_agent/ask"
                    ) =>
                {
                    pending.complete(id, message.clone());
                }
                (WireMessage::Notification { method, .. }, ReceiveAction::NotificationAccepted)
                    if method == "permission/revoke" =>
                {
                    let params = notification_params::<PermissionRevokeParams>(
                        &message,
                        "permission/revoke",
                    )
                    .map_err(invalid_request)?;
                    grants.revoke(&params.grant_id);
                }
                _ => {
                    return Err(HostError {
                        code: WireErrorCode::ProtocolViolation,
                        message: "protocol message is invalid",
                    });
                }
            }
            reader_commands
                .send(ReaderCommand::Continue(None))
                .map_err(|_| protocol_write_error())?;
        }
        Ok(())
    })();
    disconnected.store(true, Ordering::Release);
    let _ = reader_commands.try_send(ReaderCommand::Stop);
    cancel_active(&session, "");
    join_executions(executions);
    grants.clear();
    if let Ok(mut state) = tracker.lock() {
        state.disconnect();
    }
    result
}

enum ReaderCommand {
    Continue(Option<usize>),
    Stop,
}

fn spawn_reader<R: Read + Send + 'static>(
    input: R,
) -> (
    Receiver<Result<Option<WireMessage>, ProtocolError>>,
    SyncSender<ReaderCommand>,
) {
    let (messages, received) = sync_channel(1);
    let (commands, next) = sync_channel(0);
    std::thread::spawn(move || {
        let mut reader = FrameReader::new(input, josh_protocol::DEFAULT_MAX_FRAME_BYTES);
        loop {
            let message = reader.read_message();
            let terminal = !matches!(message, Ok(Some(_)));
            if messages.send(message).is_err() || terminal {
                return;
            }
            match next.recv() {
                Ok(ReaderCommand::Continue(limit)) => {
                    if let Some(limit) = limit {
                        reader.set_max_frame_bytes(limit);
                    }
                }
                Ok(ReaderCommand::Stop) | Err(_) => return,
            }
        }
    });
    (received, commands)
}

fn receive_input(
    reader: &Receiver<Result<Option<WireMessage>, ProtocolError>>,
    disconnected: &AtomicBool,
) -> Result<Result<Option<WireMessage>, ProtocolError>, HostError> {
    loop {
        if disconnected.load(Ordering::Acquire) {
            return Err(protocol_write_error());
        }
        match reader.recv_timeout(Duration::from_millis(10)) {
            Ok(message) => return Ok(message),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(protocol_write_error()),
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn spawn_execution(
    prepared: PreparedExecution,
    writer: Arc<ConnectionWriter>,
    session: Arc<Mutex<Session>>,
    tracker: Arc<Mutex<ProtocolTracker>>,
    commit_gate: Arc<Mutex<()>>,
    pending: Arc<PendingRegistry>,
    disconnected: Arc<AtomicBool>,
    outgoing_ids: Arc<AtomicU64>,
    grants: Arc<GrantRegistry>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let started = Instant::now();
        let clock: Arc<dyn EventClock> = Arc::new(SystemEventClock::new(started));
        let event_sequence = Arc::new(AtomicU64::new(3));
        let replayed = Arc::new(AtomicBool::new(false));
        let tool_protocol_violation = Arc::new(AtomicBool::new(false));
        let cancelled = prepared.cancellation_flag();
        let mut tools = WireToolProvider::new(
            prepared.execution_id().to_owned(),
            Arc::clone(prepared.catalog()),
            Arc::clone(&writer),
            Arc::clone(&tracker),
            Arc::clone(&pending),
            Arc::clone(&disconnected),
            Arc::clone(&event_sequence),
            Arc::clone(&clock),
            Arc::clone(&outgoing_ids),
            Arc::clone(&replayed),
            Arc::clone(&cancelled),
            Arc::clone(&tool_protocol_violation),
        );
        let mut invoking_agent = prepared.invoking_session_id().map(|session_id| {
            WireInvokingAgentProvider::new(
                prepared.execution_id().to_owned(),
                session_id.to_owned(),
                Arc::clone(&writer),
                Arc::clone(&tracker),
                Arc::clone(&pending),
                Arc::clone(&outgoing_ids),
                Arc::clone(&disconnected),
            )
        });
        let mut model = Some(WireResponseProvider::new(
            prepared.execution_id().to_owned(),
            ResponseProviderKind::Model,
            Arc::clone(&writer),
            Arc::clone(&tracker),
            Arc::clone(&pending),
            Arc::clone(&outgoing_ids),
            Arc::clone(&disconnected),
        ));
        let mut user = Some(WireResponseProvider::new(
            prepared.execution_id().to_owned(),
            ResponseProviderKind::User,
            Arc::clone(&writer),
            Arc::clone(&tracker),
            Arc::clone(&pending),
            Arc::clone(&outgoing_ids),
            Arc::clone(&disconnected),
        ));
        let mut sub_agent = Some(WireSubAgentProvider::new(
            prepared.execution_id().to_owned(),
            Arc::clone(&writer),
            Arc::clone(&tracker),
            Arc::clone(&pending),
            Arc::clone(&outgoing_ids),
            Arc::clone(&disconnected),
        ));
        let mut external_grants = prepared.invoking_session_id().map(|session_id| {
            WireExternalGrantProvider::new(
                prepared.execution_id().to_owned(),
                session_id.to_owned(),
                Arc::clone(&writer),
                Arc::clone(&tracker),
                Arc::clone(&pending),
                Arc::clone(&outgoing_ids),
                Arc::clone(&disconnected),
                Arc::clone(&cancelled),
                started
                    .checked_add(prepared.wall_time())
                    .unwrap_or_else(Instant::now),
                Arc::clone(&grants),
                Arc::clone(&event_sequence),
                Arc::clone(&clock),
                Arc::clone(&replayed),
            )
        });
        let mut observer = WireTaskObserver {
            execution_id: prepared.execution_id().to_owned(),
            writer: Arc::clone(&writer),
            sequence: Arc::clone(&event_sequence),
            clock: Arc::clone(&clock),
            cancelled,
            disconnected: Arc::clone(&disconnected),
            deadline: started
                .checked_add(prepared.wall_time())
                .unwrap_or_else(Instant::now),
            replayed: Arc::clone(&replayed),
            started: false,
        };
        let mut providers = RuntimeProviders {
            tools: Some(&mut tools),
            invoking_agent: invoking_agent
                .as_mut()
                .map(|provider| provider as &mut dyn InvokingAgentProvider),
            model: model
                .as_mut()
                .map(|provider| provider as &mut dyn ResponseProvider),
            user: user
                .as_mut()
                .map(|provider| provider as &mut dyn ResponseProvider),
            sub_agent: sub_agent
                .as_mut()
                .map(|provider| provider as &mut dyn SubAgentProvider),
            external_grants: external_grants
                .as_mut()
                .map(|provider| provider as &mut dyn ExternalGrantDecisionProvider),
            ..RuntimeProviders::default()
        };
        let computed = prepared.run_with_observer(&mut providers, &mut observer);
        cancel_pending_outbound(
            &pending,
            &tracker,
            &writer,
            Instant::now() + CONTROL_ENQUEUE_TIMEOUT,
        );
        grants.clear();
        let Ok(_gate) = commit_gate.lock() else {
            return;
        };
        if disconnected.load(Ordering::Acquire) {
            if let Ok(mut state) = tracker.lock() {
                state.execution_finished();
            }
            if let Ok(mut session) = session.lock() {
                session.finish_execution(prepared.request_id());
            }
            return;
        }
        let mut result = if tool_protocol_violation.load(Ordering::Acquire) {
            provider_protocol_violation_result()
        } else if prepared.is_cancelled() {
            ExecutionResult::Cancelled { reason: None }
        } else {
            computed
        };
        if result.validate().is_err()
            || validate_result_frame(&writer, prepared.request_id(), &result).is_err()
        {
            result = frame_limit_result();
        }
        if validate_result_frame(&writer, prepared.request_id(), &result).is_err() {
            if let Ok(mut state) = tracker.lock() {
                state.execution_finished();
            }
            if let Ok(mut session) = session.lock() {
                session.finish_execution(prepared.request_id());
            }
            return;
        }
        let kind = match &result {
            ExecutionResult::Completed { .. } => EventKind::Completed,
            ExecutionResult::Stopped { .. } => EventKind::Stopped,
            ExecutionResult::Failed { .. } => EventKind::Failed,
            ExecutionResult::Cancelled { .. } => EventKind::Cancelled,
        };
        let elapsed = clock.elapsed_ms();
        let terminal_sequence = event_sequence.load(Ordering::Relaxed);
        if write_event(
            &writer,
            &execution_event(
                prepared.execution_id(),
                terminal_sequence,
                elapsed,
                kind,
                replayed.load(Ordering::Acquire),
                BTreeMap::new(),
            ),
        )
        .is_ok()
        {
            event_sequence.fetch_add(1, Ordering::Relaxed);
            if write_result(&writer, prepared.request_id(), &result).is_err() {
                if let Ok(mut state) = tracker.lock() {
                    state.execution_finished();
                }
                if let Ok(mut session) = session.lock() {
                    session.finish_execution(prepared.request_id());
                }
                return;
            }
            if let Ok(mut state) = tracker.lock() {
                let _ = state.commit_response(prepared.request_id());
                state.execution_finished();
            }
            if let Ok(mut state) = session.lock() {
                state.finish_execution(prepared.request_id());
            }
        } else {
            if let Ok(mut state) = tracker.lock() {
                state.execution_finished();
            }
            if let Ok(mut session) = session.lock() {
                session.finish_execution(prepared.request_id());
            }
        }
    })
}

const OUTBOUND_QUEUE_DEPTH: usize = 8;
const CONTROL_ENQUEUE_TIMEOUT: Duration = Duration::from_secs(1);

struct ConnectionWriter {
    sender: SyncSender<Vec<u8>>,
    max_frame_bytes: AtomicUsize,
    failed: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    session: Arc<Mutex<Session>>,
    tracker: Arc<Mutex<ProtocolTracker>>,
    pending: Arc<PendingRegistry>,
}

impl ConnectionWriter {
    fn new<W: Write + Send + 'static>(
        mut output: W,
        max_frame_bytes: usize,
        disconnected: Arc<AtomicBool>,
        session: Arc<Mutex<Session>>,
        tracker: Arc<Mutex<ProtocolTracker>>,
        pending: Arc<PendingRegistry>,
    ) -> Self {
        let (sender, receiver) = sync_channel::<Vec<u8>>(OUTBOUND_QUEUE_DEPTH);
        let failed = Arc::new(AtomicBool::new(false));
        let writer_failed = Arc::clone(&failed);
        let writer_disconnected = Arc::clone(&disconnected);
        let writer_session = Arc::clone(&session);
        let writer_tracker = Arc::clone(&tracker);
        let writer_pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            while let Ok(frame) = receiver.recv() {
                if output
                    .write_all(&frame)
                    .and_then(|()| output.flush())
                    .is_err()
                {
                    writer_failed.store(true, Ordering::Release);
                    writer_disconnected.store(true, Ordering::Release);
                    cancel_active(&writer_session, "");
                    writer_pending.fail_all();
                    if let Ok(mut state) = writer_tracker.lock() {
                        state.disconnect();
                    }
                    break;
                }
            }
        });
        Self {
            sender,
            max_frame_bytes: AtomicUsize::new(max_frame_bytes),
            failed,
            disconnected,
            session,
            tracker,
            pending,
        }
    }

    fn set_max_frame_bytes(&self, max_frame_bytes: usize) {
        self.max_frame_bytes
            .store(max_frame_bytes, Ordering::Release);
    }

    fn write_message(&self, message: &WireMessage) -> Result<(), HostError> {
        self.write_message_until(message, Instant::now() + CONTROL_ENQUEUE_TIMEOUT)
    }

    fn write_message_until(
        &self,
        message: &WireMessage,
        deadline: Instant,
    ) -> Result<(), HostError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(protocol_write_error());
        }
        let frame = encode_frame(message, self.max_frame_bytes.load(Ordering::Acquire))
            .map_err(protocol_error)?;
        let deadline = deadline.min(Instant::now() + CONTROL_ENQUEUE_TIMEOUT);
        let mut pending = frame;
        loop {
            match self.sender.try_send(pending) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    self.fail_connection();
                    return Err(protocol_write_error());
                }
                Err(TrySendError::Full(frame)) => {
                    pending = frame;
                    if Instant::now() >= deadline || self.failed.load(Ordering::Acquire) {
                        self.fail_connection();
                        return Err(protocol_write_error());
                    }
                    std::thread::yield_now();
                }
            }
        }
    }

    fn fail_connection(&self) {
        self.failed.store(true, Ordering::Release);
        self.disconnected.store(true, Ordering::Release);
        cancel_active(&self.session, "");
        self.pending.fail_all();
        if let Ok(mut state) = self.tracker.lock() {
            state.disconnect();
        }
    }
}

fn finish_failed_request(
    writer: &ConnectionWriter,
    tracker: &Arc<Mutex<ProtocolTracker>>,
    id: &str,
    error: HostError,
) -> Result<(), HostError> {
    write_wire_error(writer, id, error.wire())?;
    lock_tracker(tracker)?
        .commit_response(id)
        .map_err(state_error)?;
    Ok(())
}

fn write_result<T: serde::Serialize>(
    writer: &ConnectionWriter,
    id: &str,
    result: &T,
) -> Result<(), HostError> {
    let value = serde_json::to_value(result).map_err(|_| poisoned())?;
    write_message(
        writer,
        &WireMessage::Response {
            id: id.to_owned(),
            result: Some(value),
            error: None,
        },
    )
}

fn validate_result_frame<T: serde::Serialize>(
    writer: &ConnectionWriter,
    id: &str,
    result: &T,
) -> Result<(), HostError> {
    let value = serde_json::to_value(result).map_err(|_| poisoned())?;
    encode_frame(
        &WireMessage::Response {
            id: id.to_owned(),
            result: Some(value),
            error: None,
        },
        writer.max_frame_bytes.load(Ordering::Acquire),
    )
    .map(|_| ())
    .map_err(protocol_error)
}

fn frame_limit_result() -> ExecutionResult {
    ExecutionResult::Failed {
        error: josh_protocol::ProtocolRuntimeError {
            code: "resource.limit".to_owned(),
            message: "output exceeds protocol frame limit".to_owned(),
            category: "runtime".to_owned(),
            retryable: false,
            span: None,
            operation_id: None,
            metadata: BTreeMap::new(),
            causes: Vec::new(),
        },
    }
}

fn provider_protocol_violation_result() -> ExecutionResult {
    ExecutionResult::Failed {
        error: josh_protocol::ProtocolRuntimeError {
            code: "protocol.violation".to_owned(),
            message: "runtime protocol violation".to_owned(),
            category: "runtime".to_owned(),
            retryable: false,
            span: None,
            operation_id: None,
            metadata: BTreeMap::new(),
            causes: Vec::new(),
        },
    }
}

fn write_wire_error(
    writer: &ConnectionWriter,
    id: &str,
    error: WireError,
) -> Result<(), HostError> {
    write_message(
        writer,
        &WireMessage::Response {
            id: id.to_owned(),
            result: None,
            error: Some(error),
        },
    )
}

fn write_notification<T: serde::Serialize + Validate>(
    writer: &ConnectionWriter,
    method: &str,
    params: &T,
) -> Result<(), HostError> {
    params.validate().map_err(|_| poisoned())?;
    let value = serde_json::to_value(params).map_err(|_| poisoned())?;
    write_message(
        writer,
        &WireMessage::Notification {
            method: method.to_owned(),
            params: value,
        },
    )
}

fn write_event(writer: &ConnectionWriter, event: &ExecutionEventParams) -> Result<(), HostError> {
    write_notification(writer, "execution/event", event)
}

fn write_message(writer: &ConnectionWriter, message: &WireMessage) -> Result<(), HostError> {
    writer.write_message(message)
}

fn lock_session(
    session: &Arc<Mutex<Session>>,
) -> Result<std::sync::MutexGuard<'_, Session>, HostError> {
    session.lock().map_err(|_| poisoned())
}

fn lock_tracker(
    tracker: &Arc<Mutex<ProtocolTracker>>,
) -> Result<std::sync::MutexGuard<'_, ProtocolTracker>, HostError> {
    tracker.lock().map_err(|_| poisoned())
}

fn cancel_active(session: &Arc<Mutex<Session>>, request_id: &str) {
    if let Ok(session) = session.lock() {
        if request_id.is_empty() {
            session.cancel_active();
        } else {
            let _ = session.cancel(request_id);
        }
    }
}

fn join_executions(executions: Vec<JoinHandle<()>>) {
    for execution in executions {
        let _ = execution.join();
    }
}

fn invalid_request(_: ProtocolError) -> HostError {
    HostError {
        code: WireErrorCode::RequestInvalid,
        message: "request parameters are invalid",
    }
}

fn state_error(_: josh_protocol::RequestStateError) -> HostError {
    HostError {
        code: WireErrorCode::ProtocolViolation,
        message: "protocol state is invalid",
    }
}

fn protocol_error(_: ProtocolError) -> HostError {
    protocol_write_error()
}

const fn protocol_write_error() -> HostError {
    HostError {
        code: WireErrorCode::ProtocolViolation,
        message: "protocol write failed",
    }
}

const fn poisoned() -> HostError {
    HostError {
        code: WireErrorCode::ProtocolViolation,
        message: "host state is unavailable",
    }
}

#[derive(Default)]
struct PendingRegistry {
    calls: Mutex<BTreeMap<String, Arc<PendingCall>>>,
}

impl PendingRegistry {
    fn insert(&self, id: String, call: Arc<PendingCall>) -> Result<(), ()> {
        let mut calls = self.calls.lock().map_err(|_| ())?;
        if calls.contains_key(&id) {
            return Err(());
        }
        calls.insert(id, call);
        Ok(())
    }

    fn remove(&self, id: &str) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.remove(id);
        }
    }

    fn complete(&self, id: &str, message: WireMessage) {
        let call = self
            .calls
            .lock()
            .ok()
            .and_then(|mut calls| calls.remove(id));
        if let Some(call) = call {
            call.complete(message);
        }
    }

    fn fail_all(&self) {
        if let Ok(mut calls) = self.calls.lock() {
            for (_, call) in std::mem::take(&mut *calls) {
                call.fail();
            }
        }
    }

    fn fail_all_with_ids(&self) -> Vec<String> {
        let Ok(mut calls) = self.calls.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *calls)
            .into_iter()
            .map(|(id, call)| {
                call.fail();
                id
            })
            .collect()
    }
}

#[derive(Default)]
struct PendingCall {
    response: Mutex<Option<WireMessage>>,
    ready: Condvar,
    failed: AtomicBool,
}

#[derive(Debug, PartialEq)]
enum PendingWait {
    Response(WireMessage),
    Cancelled,
    Deadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundError {
    Cancelled,
    Deadline,
    Transport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderWireError {
    Unavailable,
    Rejected,
    Cancelled,
    ProtocolViolation,
}

fn classify_provider_wire_error(
    code: WireErrorCode,
    unavailable: WireErrorCode,
    denied: Option<WireErrorCode>,
) -> ProviderWireError {
    if code == unavailable {
        ProviderWireError::Unavailable
    } else if denied == Some(code) {
        ProviderWireError::Rejected
    } else if code == WireErrorCode::RequestCancelled {
        ProviderWireError::Cancelled
    } else {
        ProviderWireError::ProtocolViolation
    }
}

impl PendingCall {
    fn complete(&self, message: WireMessage) {
        if let Ok(mut response) = self.response.lock() {
            *response = Some(message);
            self.ready.notify_one();
        }
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.ready.notify_one();
    }

    fn poll(&self) -> Result<Option<WireMessage>, ()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(());
        }
        self.response
            .lock()
            .map(|mut response| response.take())
            .map_err(|_| ())
    }

    fn wait(
        &self,
        mut is_cancelled: impl FnMut() -> bool,
        deadline: std::time::Duration,
    ) -> Result<PendingWait, ()> {
        let started = Instant::now();
        let mut response = self.response.lock().map_err(|_| ())?;
        loop {
            if let Some(response) = response.take() {
                return Ok(PendingWait::Response(response));
            }
            if self.failed.load(Ordering::Acquire) {
                return Err(());
            }
            if is_cancelled() {
                return Ok(PendingWait::Cancelled);
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(PendingWait::Deadline);
            }
            let (next, _) = self
                .ready
                .wait_timeout(
                    response,
                    remaining.min(std::time::Duration::from_millis(10)),
                )
                .map_err(|_| ())?;
            response = next;
        }
    }
}

struct PendingOutbound {
    request_id: String,
    call: Arc<PendingCall>,
    deadline: Instant,
}

#[allow(clippy::too_many_arguments)]
fn start_outbound_request(
    method: &str,
    params: Value,
    writer: &ConnectionWriter,
    tracker: &Arc<Mutex<ProtocolTracker>>,
    pending: &PendingRegistry,
    outgoing_ids: &AtomicU64,
    deadline: Instant,
) -> Result<PendingOutbound, OutboundError> {
    let request_number = outgoing_ids.fetch_add(1, Ordering::Relaxed);
    if request_number == u64::MAX {
        return Err(OutboundError::Transport);
    }
    let request_id = format!("r-{request_number}");
    let message = WireMessage::Request {
        id: request_id.clone(),
        method: method.to_owned(),
        params,
    };
    let call = Arc::new(PendingCall::default());
    pending
        .insert(request_id.clone(), Arc::clone(&call))
        .map_err(|()| OutboundError::Transport)?;
    if tracker
        .lock()
        .map_err(|_| OutboundError::Transport)?
        .register_outgoing_message(&message)
        .is_err()
    {
        pending.remove(&request_id);
        return Err(OutboundError::Transport);
    }
    if writer.write_message_until(&message, deadline).is_err() {
        pending.remove(&request_id);
        let _ = tracker
            .lock()
            .map(|mut tracker| tracker.cancel_outgoing(&request_id));
        return Err(OutboundError::Transport);
    }
    Ok(PendingOutbound {
        request_id,
        call,
        deadline,
    })
}

fn poll_outbound_request(outbound: &PendingOutbound) -> Result<Option<WireMessage>, OutboundError> {
    if Instant::now() >= outbound.deadline {
        return Err(OutboundError::Deadline);
    }
    outbound.call.poll().map_err(|()| OutboundError::Transport)
}

fn cancel_outbound_request(
    outbound: PendingOutbound,
    writer: &ConnectionWriter,
    tracker: &Arc<Mutex<ProtocolTracker>>,
    pending: &PendingRegistry,
    disconnected: &AtomicBool,
) {
    pending.remove(&outbound.request_id);
    let cancelled = tracker
        .lock()
        .is_ok_and(|mut state| state.cancel_outgoing(&outbound.request_id));
    if cancelled && !disconnected.load(Ordering::Acquire) {
        let cancel_deadline = Instant::now()
            .checked_add(CONTROL_ENQUEUE_TIMEOUT)
            .unwrap_or_else(Instant::now);
        let _ = writer.write_message_until(
            &WireMessage::Cancel {
                id: outbound.request_id,
                reason: None,
            },
            cancel_deadline,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn send_outbound_request(
    method: &str,
    params: Value,
    writer: &ConnectionWriter,
    tracker: &Arc<Mutex<ProtocolTracker>>,
    pending: &PendingRegistry,
    outgoing_ids: &AtomicU64,
    disconnected: &AtomicBool,
    deadline: Instant,
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<WireMessage, OutboundError> {
    let request_number = outgoing_ids.fetch_add(1, Ordering::Relaxed);
    if request_number == u64::MAX {
        return Err(OutboundError::Transport);
    }
    let request_id = format!("r-{request_number}");
    let message = WireMessage::Request {
        id: request_id.clone(),
        method: method.to_owned(),
        params,
    };
    let call = Arc::new(PendingCall::default());
    pending
        .insert(request_id.clone(), Arc::clone(&call))
        .map_err(|()| OutboundError::Transport)?;
    let registered = tracker
        .lock()
        .map_err(|_| OutboundError::Transport)?
        .register_outgoing_message(&message);
    if registered.is_err() {
        pending.remove(&request_id);
        return Err(OutboundError::Transport);
    }
    if writer.write_message_until(&message, deadline).is_err() {
        pending.remove(&request_id);
        let _ = tracker
            .lock()
            .map(|mut tracker| tracker.cancel_outgoing(&request_id));
        return Err(OutboundError::Transport);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let wait = call
        .wait(&mut is_cancelled, remaining)
        .map_err(|()| OutboundError::Transport)?;
    match wait {
        PendingWait::Response(response) => Ok(response),
        PendingWait::Cancelled | PendingWait::Deadline => {
            pending.remove(&request_id);
            let cancelled = tracker
                .lock()
                .map_err(|_| OutboundError::Transport)?
                .cancel_outgoing(&request_id);
            if cancelled && !disconnected.load(Ordering::Acquire) {
                let _ = writer.write_message_until(
                    &WireMessage::Cancel {
                        id: request_id,
                        reason: None,
                    },
                    deadline,
                );
            }
            Err(match wait {
                PendingWait::Cancelled => OutboundError::Cancelled,
                PendingWait::Deadline => OutboundError::Deadline,
                PendingWait::Response(_) => unreachable!(),
            })
        }
    }
}

fn cancel_pending_outbound(
    pending: &PendingRegistry,
    tracker: &Arc<Mutex<ProtocolTracker>>,
    writer: &ConnectionWriter,
    deadline: Instant,
) {
    for id in pending.fail_all_with_ids() {
        let cancelled = tracker
            .lock()
            .is_ok_and(|mut state| state.cancel_outgoing(&id));
        if cancelled {
            let _ = writer.write_message_until(&WireMessage::Cancel { id, reason: None }, deadline);
        }
    }
}

struct WireToolProvider {
    execution_id: String,
    catalog: Arc<allen_schema::FrozenCatalog>,
    writer: Arc<ConnectionWriter>,
    tracker: Arc<Mutex<ProtocolTracker>>,
    pending: Arc<PendingRegistry>,
    outgoing_ids: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
    event_sequence: Arc<AtomicU64>,
    clock: Arc<dyn EventClock>,
    replayed: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    protocol_violation: Arc<AtomicBool>,
    active: BTreeMap<u64, PendingToolRequest>,
}

struct PendingToolRequest {
    outbound: PendingOutbound,
    invocation: ToolInvocation,
    definition: allen_schema::ToolDefinition,
}

struct WireInvokingAgentProvider {
    execution_id: String,
    session_id: String,
    writer: Arc<ConnectionWriter>,
    tracker: Arc<Mutex<ProtocolTracker>>,
    pending: Arc<PendingRegistry>,
    outgoing_ids: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
    active: BTreeMap<u64, PendingAgentRequest>,
}

struct PendingAgentRequest {
    outbound: PendingOutbound,
    kind: PendingAgentKind,
}

#[derive(Clone, Copy)]
enum PendingAgentKind {
    Message,
    Ask,
    Transcript { limit: u32 },
}

impl WireInvokingAgentProvider {
    #[allow(clippy::too_many_arguments)]
    fn new(
        execution_id: String,
        session_id: String,
        writer: Arc<ConnectionWriter>,
        tracker: Arc<Mutex<ProtocolTracker>>,
        pending: Arc<PendingRegistry>,
        outgoing_ids: Arc<AtomicU64>,
        disconnected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            execution_id,
            session_id,
            writer,
            tracker,
            pending,
            outgoing_ids,
            disconnected,
            active: BTreeMap::new(),
        }
    }

    fn start_request(
        &mut self,
        pending_id: PendingEffectId,
        method: &str,
        params: Value,
        deadline: Duration,
        kind: PendingAgentKind,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        if self.active.contains_key(&pending_id.0) {
            return Err(AgentHostError::Rejected);
        }
        let deadline_at = Instant::now()
            .checked_add(deadline)
            .unwrap_or_else(Instant::now);
        let outbound = start_outbound_request(
            method,
            params,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            deadline_at,
        )
        .map_err(agent_outbound_error)?;
        self.active
            .insert(pending_id.0, PendingAgentRequest { outbound, kind });
        Ok(AgentProviderPoll::Pending)
    }

    fn finish_response(
        &self,
        response: &WireMessage,
        kind: PendingAgentKind,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        if let Some(error) = Self::response_error(response) {
            return Err(error);
        }
        match kind {
            PendingAgentKind::Message => response_result::<AgentMessageResult>(response)
                .map(|result| AgentProviderPoll::Message(result.accepted))
                .map_err(|_| AgentHostError::InvalidOutcome),
            PendingAgentKind::Ask => response_result::<TypedResponseResult>(response)
                .map(|result| AgentProviderPoll::Ask(result.value))
                .map_err(|_| AgentHostError::InvalidOutcome),
            PendingAgentKind::Transcript { limit } => {
                let result = response_result::<AgentTranscriptResult>(response)
                    .map_err(|_| AgentHostError::InvalidOutcome)?;
                result
                    .validate_for(&self.session_id, limit)
                    .map_err(|_| AgentHostError::InvalidOutcome)?;
                Ok(AgentProviderPoll::Transcript(runtime_transcript(
                    result.snapshot,
                )))
            }
        }
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<WireMessage, AgentHostError> {
        if cancellation.is_cancelled() {
            return Err(AgentHostError::Cancelled);
        }
        let deadline_at = Instant::now()
            .checked_add(deadline)
            .unwrap_or_else(Instant::now);
        send_outbound_request(
            method,
            params,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            &self.disconnected,
            deadline_at,
            || cancellation.is_cancelled(),
        )
        .map_err(|error| match error {
            OutboundError::Cancelled => AgentHostError::Cancelled,
            OutboundError::Deadline => AgentHostError::Timeout,
            OutboundError::Transport => AgentHostError::Transport,
        })
    }

    fn response_error(response: &WireMessage) -> Option<AgentHostError> {
        let WireMessage::Response {
            result: None,
            error: Some(error),
            ..
        } = response
        else {
            return None;
        };
        Some(
            match classify_provider_wire_error(
                error.code,
                WireErrorCode::AgentUnavailable,
                Some(WireErrorCode::AgentDenied),
            ) {
                ProviderWireError::Unavailable => AgentHostError::Unavailable,
                ProviderWireError::Rejected => AgentHostError::Rejected,
                ProviderWireError::Cancelled => AgentHostError::Cancelled,
                ProviderWireError::ProtocolViolation => AgentHostError::InvalidOutcome,
            },
        )
    }
}

fn structured_prompt_payload(prompt: &allen_runtime::StructuredPrompt) -> StructuredPromptPayload {
    StructuredPromptPayload {
        system: prompt.system.clone(),
        context: PromptSegmentPayload::from_option(prompt.context.clone()),
        data: PromptSegmentPayload::from_option(prompt.data.clone()),
        policy: PromptPolicy {
            max_attempts: prompt.max_attempts,
        },
    }
}

fn response_schema_payload(schema: &allen_runtime::ResponseSchema) -> ResponseSchemaPayload {
    ResponseSchemaPayload {
        digest: schema.digest.clone(),
        descriptor: schema.descriptor.clone(),
    }
}

fn validation_issue_payloads(
    issues: &[allen_runtime::ValidationIssue],
) -> Vec<ValidationIssuePayload> {
    issues
        .iter()
        .map(|issue| ValidationIssuePayload {
            path: issue.path.clone(),
            code: issue.code.clone(),
        })
        .collect()
}

fn agent_ask_params(
    execution_id: &str,
    session_id: &str,
    call: &AgentAskCall,
) -> Result<AgentAskParams, AgentHostError> {
    let common_operation = format!("op-{}", call.operation_id);
    let interaction_id = format!("interaction-{}", call.interaction_id);
    let deadline_ms = duration_ms(call.deadline);
    let PromptPayload::Structured(prompt) = &call.prompt else {
        return Err(AgentHostError::Rejected);
    };
    let params = AgentAskParams {
        execution_id: execution_id.to_owned(),
        operation_id: common_operation,
        session_id: session_id.to_owned(),
        interaction_id,
        prompt: structured_prompt_payload(prompt),
        response_schema: response_schema_payload(&call.response_schema),
        attempt: call.attempt,
        validation_issues: validation_issue_payloads(&call.validation_issues),
        deadline_ms,
    };
    params.validate().map_err(|_| AgentHostError::Rejected)?;
    Ok(params)
}

impl InvokingAgentProvider for WireInvokingAgentProvider {
    fn message(
        &mut self,
        call: &AgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<bool, AgentHostError> {
        let params = AgentMessageParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", call.operation_id),
            session_id: self.session_id.clone(),
            message: call.message.clone(),
            deadline_ms: duration_ms(call.deadline),
        };
        params.validate().map_err(|_| AgentHostError::Rejected)?;
        let response = self.request(
            "agent/message",
            serde_json::to_value(params).map_err(|_| AgentHostError::Transport)?,
            call.deadline,
            cancellation,
        )?;
        if let Some(error) = Self::response_error(&response) {
            return Err(error);
        }
        response_result::<AgentMessageResult>(&response)
            .map(|result| result.accepted)
            .map_err(|_| AgentHostError::InvalidOutcome)
    }

    fn ask(
        &mut self,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<Value, AgentHostError> {
        let params = agent_ask_params(&self.execution_id, &self.session_id, call)?;
        let response = self.request(
            "agent/ask",
            serde_json::to_value(params).map_err(|_| AgentHostError::Transport)?,
            call.deadline,
            cancellation,
        )?;
        if let Some(error) = Self::response_error(&response) {
            return Err(error);
        }
        response_result::<TypedResponseResult>(&response)
            .map(|result| result.value)
            .map_err(|_| AgentHostError::InvalidOutcome)
    }

    fn transcript(
        &mut self,
        query: &TranscriptQuery,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<RuntimeTranscriptSnapshot, AgentHostError> {
        let params = AgentTranscriptParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", query.operation_id),
            session_id: self.session_id.clone(),
            limit: u32::from(query.limit),
            deadline_ms: duration_ms(query.deadline),
        };
        params.validate().map_err(|_| AgentHostError::Rejected)?;
        let response = self.request(
            "agent/transcript",
            serde_json::to_value(params).map_err(|_| AgentHostError::Transport)?,
            query.deadline,
            cancellation,
        )?;
        if let Some(error) = Self::response_error(&response) {
            return Err(error);
        }
        let result = response_result::<AgentTranscriptResult>(&response)
            .map_err(|_| AgentHostError::InvalidOutcome)?;
        result
            .validate_for(&self.session_id, u32::from(query.limit))
            .map_err(|_| AgentHostError::InvalidOutcome)?;
        Ok(runtime_transcript(result.snapshot))
    }

    fn start_message(
        &mut self,
        pending: PendingEffectId,
        call: &AgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        if cancellation.is_cancelled() {
            return Err(AgentHostError::Cancelled);
        }
        let params = AgentMessageParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", call.operation_id),
            session_id: self.session_id.clone(),
            message: call.message.clone(),
            deadline_ms: duration_ms(call.deadline),
        };
        params.validate().map_err(|_| AgentHostError::Rejected)?;
        self.start_request(
            pending,
            "agent/message",
            serde_json::to_value(params).map_err(|_| AgentHostError::Transport)?,
            call.deadline,
            PendingAgentKind::Message,
        )
    }

    fn start_ask(
        &mut self,
        pending: PendingEffectId,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        if cancellation.is_cancelled() {
            return Err(AgentHostError::Cancelled);
        }
        let params = agent_ask_params(&self.execution_id, &self.session_id, call)?;
        self.start_request(
            pending,
            "agent/ask",
            serde_json::to_value(params).map_err(|_| AgentHostError::Transport)?,
            call.deadline,
            PendingAgentKind::Ask,
        )
    }

    fn start_transcript(
        &mut self,
        pending: PendingEffectId,
        query: &TranscriptQuery,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        if cancellation.is_cancelled() {
            return Err(AgentHostError::Cancelled);
        }
        let params = AgentTranscriptParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", query.operation_id),
            session_id: self.session_id.clone(),
            limit: u32::from(query.limit),
            deadline_ms: duration_ms(query.deadline),
        };
        params.validate().map_err(|_| AgentHostError::Rejected)?;
        self.start_request(
            pending,
            "agent/transcript",
            serde_json::to_value(params).map_err(|_| AgentHostError::Transport)?,
            query.deadline,
            PendingAgentKind::Transcript {
                limit: u32::from(query.limit),
            },
        )
    }

    fn poll(
        &mut self,
        pending: PendingEffectId,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<AgentProviderPoll, AgentHostError> {
        if cancellation.is_cancelled() {
            if let Some(active) = self.active.remove(&pending.0) {
                cancel_outbound_request(
                    active.outbound,
                    &self.writer,
                    &self.tracker,
                    &self.pending,
                    &self.disconnected,
                );
            }
            return Err(AgentHostError::Cancelled);
        }
        let poll = self
            .active
            .get(&pending.0)
            .ok_or(AgentHostError::InvalidOutcome)
            .and_then(|active| {
                poll_outbound_request(&active.outbound).map_err(agent_outbound_error)
            });
        let response = match poll {
            Ok(Some(response)) => response,
            Ok(None) => return Ok(AgentProviderPoll::Pending),
            Err(error) => {
                if let Some(active) = self.active.remove(&pending.0) {
                    cancel_outbound_request(
                        active.outbound,
                        &self.writer,
                        &self.tracker,
                        &self.pending,
                        &self.disconnected,
                    );
                }
                return Err(error);
            }
        };
        let active = self
            .active
            .remove(&pending.0)
            .ok_or(AgentHostError::InvalidOutcome)?;
        self.finish_response(&response, active.kind)
    }

    fn cancel_pending(
        &mut self,
        pending: PendingEffectId,
        _execution_id: allen_runtime::ExternalExecutionId,
        _operation_id: u64,
    ) {
        if let Some(active) = self.active.remove(&pending.0) {
            cancel_outbound_request(
                active.outbound,
                &self.writer,
                &self.tracker,
                &self.pending,
                &self.disconnected,
            );
        }
    }
}

#[derive(Clone, Copy)]
enum ResponseProviderKind {
    Model,
    User,
}

impl ResponseProviderKind {
    const fn method(self) -> &'static str {
        match self {
            Self::Model => "model/request",
            Self::User => "user/ask",
        }
    }

    const fn identity(self) -> &'static str {
        match self {
            Self::Model => "josh:model",
            Self::User => "josh:user",
        }
    }
}

struct WireResponseProvider {
    execution_id: String,
    kind: ResponseProviderKind,
    writer: Arc<ConnectionWriter>,
    tracker: Arc<Mutex<ProtocolTracker>>,
    pending: Arc<PendingRegistry>,
    outgoing_ids: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
    active: BTreeMap<u64, PendingOutbound>,
}

impl WireResponseProvider {
    #[allow(clippy::too_many_arguments)]
    fn new(
        execution_id: String,
        kind: ResponseProviderKind,
        writer: Arc<ConnectionWriter>,
        tracker: Arc<Mutex<ProtocolTracker>>,
        pending: Arc<PendingRegistry>,
        outgoing_ids: Arc<AtomicU64>,
        disconnected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            execution_id,
            kind,
            writer,
            tracker,
            pending,
            outgoing_ids,
            disconnected,
            active: BTreeMap::new(),
        }
    }

    fn params(&self, call: &AgentAskCall) -> Result<Value, ResponseHostError> {
        let PromptPayload::Structured(prompt) = &call.prompt else {
            return Err(ResponseHostError::Rejected);
        };
        let operation_id = format!("op-{}", call.operation_id);
        let interaction_id = format!("interaction-{}", call.interaction_id);
        let prompt = structured_prompt_payload(prompt);
        let response_schema = response_schema_payload(&call.response_schema);
        let validation_issues = validation_issue_payloads(&call.validation_issues);
        let deadline_ms = duration_ms(call.deadline);
        match self.kind {
            ResponseProviderKind::Model => {
                let params = ModelRequestParams {
                    execution_id: self.execution_id.clone(),
                    operation_id,
                    interaction_id,
                    prompt,
                    response_schema,
                    attempt: call.attempt,
                    validation_issues,
                    deadline_ms,
                };
                params.validate().map_err(|_| ResponseHostError::Rejected)?;
                serde_json::to_value(params).map_err(|_| ResponseHostError::Transport)
            }
            ResponseProviderKind::User => {
                let params = UserAskParams {
                    execution_id: self.execution_id.clone(),
                    operation_id,
                    interaction_id,
                    prompt,
                    response_schema,
                    attempt: call.attempt,
                    validation_issues,
                    deadline_ms,
                };
                params.validate().map_err(|_| ResponseHostError::Rejected)?;
                serde_json::to_value(params).map_err(|_| ResponseHostError::Transport)
            }
        }
    }

    fn response_error(&self, response: &WireMessage) -> Option<ResponseHostError> {
        let WireMessage::Response {
            result: None,
            error: Some(error),
            ..
        } = response
        else {
            return None;
        };
        let unavailable = match self.kind {
            ResponseProviderKind::Model => WireErrorCode::ModelUnavailable,
            ResponseProviderKind::User => WireErrorCode::UserUnavailable,
        };
        Some(
            match classify_provider_wire_error(
                error.code,
                unavailable,
                Some(match self.kind {
                    ResponseProviderKind::Model => WireErrorCode::ModelDenied,
                    ResponseProviderKind::User => WireErrorCode::UserDenied,
                }),
            ) {
                ProviderWireError::Unavailable => ResponseHostError::Unavailable,
                ProviderWireError::Rejected => ResponseHostError::Rejected,
                ProviderWireError::Cancelled => ResponseHostError::Cancelled,
                ProviderWireError::ProtocolViolation => ResponseHostError::InvalidOutcome,
            },
        )
    }

    fn finish(&self, response: &WireMessage) -> Result<Value, ResponseHostError> {
        if let Some(error) = self.response_error(response) {
            return Err(error);
        }
        response_result::<TypedResponseResult>(response)
            .map(|result| result.value)
            .map_err(|_| ResponseHostError::InvalidOutcome)
    }

    fn cancel_active(&mut self, pending: PendingEffectId) {
        if let Some(outbound) = self.active.remove(&pending.0) {
            cancel_outbound_request(
                outbound,
                &self.writer,
                &self.tracker,
                &self.pending,
                &self.disconnected,
            );
        }
    }
}

impl ResponseProvider for WireResponseProvider {
    fn identity(&self) -> &str {
        self.kind.identity()
    }

    fn request(
        &mut self,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<Value, ResponseHostError> {
        if cancellation.is_cancelled() {
            return Err(ResponseHostError::Cancelled);
        }
        let deadline_at = Instant::now()
            .checked_add(call.deadline)
            .unwrap_or_else(Instant::now);
        let response = send_outbound_request(
            self.kind.method(),
            self.params(call)?,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            &self.disconnected,
            deadline_at,
            || cancellation.is_cancelled(),
        )
        .map_err(response_outbound_error)?;
        self.finish(&response)
    }

    fn start_request(
        &mut self,
        pending: PendingEffectId,
        call: &AgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        if cancellation.is_cancelled() {
            return Err(ResponseHostError::Cancelled);
        }
        if self.active.contains_key(&pending.0) {
            return Err(ResponseHostError::Rejected);
        }
        let deadline_at = Instant::now()
            .checked_add(call.deadline)
            .unwrap_or_else(Instant::now);
        let outbound = start_outbound_request(
            self.kind.method(),
            self.params(call)?,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            deadline_at,
        )
        .map_err(response_outbound_error)?;
        self.active.insert(pending.0, outbound);
        Ok(ResponseProviderPoll::Pending)
    }

    fn poll(
        &mut self,
        pending: PendingEffectId,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<ResponseProviderPoll, ResponseHostError> {
        if cancellation.is_cancelled() {
            self.cancel_active(pending);
            return Err(ResponseHostError::Cancelled);
        }
        let response = match self
            .active
            .get(&pending.0)
            .ok_or(ResponseHostError::InvalidOutcome)
            .and_then(|outbound| poll_outbound_request(outbound).map_err(response_outbound_error))
        {
            Ok(Some(response)) => response,
            Ok(None) => return Ok(ResponseProviderPoll::Pending),
            Err(error) => {
                self.cancel_active(pending);
                return Err(error);
            }
        };
        self.active.remove(&pending.0);
        self.finish(&response).map(ResponseProviderPoll::Response)
    }

    fn cancel(
        &mut self,
        pending: PendingEffectId,
        _execution_id: allen_runtime::ExternalExecutionId,
        _operation_id: u64,
    ) {
        self.cancel_active(pending);
    }
}

#[derive(Clone, Copy)]
enum PendingSubAgentKind {
    Create,
    Message,
    Response,
}

struct PendingWireSubAgent {
    outbound: PendingOutbound,
    kind: PendingSubAgentKind,
}

type WireResponseFields = (
    String,
    String,
    StructuredPromptPayload,
    ResponseSchemaPayload,
    Vec<ValidationIssuePayload>,
    u64,
);

struct WireSubAgentProvider {
    execution_id: String,
    writer: Arc<ConnectionWriter>,
    tracker: Arc<Mutex<ProtocolTracker>>,
    pending: Arc<PendingRegistry>,
    outgoing_ids: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
    active: BTreeMap<u64, PendingWireSubAgent>,
}

impl WireSubAgentProvider {
    fn new(
        execution_id: String,
        writer: Arc<ConnectionWriter>,
        tracker: Arc<Mutex<ProtocolTracker>>,
        pending: Arc<PendingRegistry>,
        outgoing_ids: Arc<AtomicU64>,
        disconnected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            execution_id,
            writer,
            tracker,
            pending,
            outgoing_ids,
            disconnected,
            active: BTreeMap::new(),
        }
    }

    fn projection(projection: &SubAgentProjection) -> SubAgentProjectionPayload {
        SubAgentProjectionPayload {
            capabilities: projection.capabilities.iter().cloned().collect(),
            limits: projection.limits.clone(),
            tools: projection.tools.iter().cloned().collect(),
        }
    }

    fn response_fields(call: &AgentAskCall) -> Result<WireResponseFields, SubAgentHostError> {
        let PromptPayload::Structured(prompt) = &call.prompt else {
            return Err(SubAgentHostError::Rejected);
        };
        Ok((
            format!("op-{}", call.operation_id),
            format!("interaction-{}", call.interaction_id),
            structured_prompt_payload(prompt),
            response_schema_payload(&call.response_schema),
            validation_issue_payloads(&call.validation_issues),
            duration_ms(call.deadline),
        ))
    }

    fn send(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<WireMessage, SubAgentHostError> {
        let deadline_at = Instant::now()
            .checked_add(deadline)
            .unwrap_or_else(Instant::now);
        send_outbound_request(
            method,
            params,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            &self.disconnected,
            deadline_at,
            || cancellation.is_cancelled(),
        )
        .map_err(sub_agent_outbound_error)
    }

    fn start(
        &mut self,
        pending: PendingEffectId,
        method: &str,
        params: Value,
        deadline: Duration,
        kind: PendingSubAgentKind,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        if self.active.contains_key(&pending.0) {
            return Err(SubAgentHostError::Rejected);
        }
        let deadline_at = Instant::now()
            .checked_add(deadline)
            .unwrap_or_else(Instant::now);
        let outbound = start_outbound_request(
            method,
            params,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            deadline_at,
        )
        .map_err(sub_agent_outbound_error)?;
        self.active
            .insert(pending.0, PendingWireSubAgent { outbound, kind });
        Ok(SubAgentProviderPoll::Pending)
    }

    fn response_error(response: &WireMessage) -> Option<SubAgentHostError> {
        let WireMessage::Response {
            result: None,
            error: Some(error),
            ..
        } = response
        else {
            return None;
        };
        Some(
            match classify_provider_wire_error(
                error.code,
                WireErrorCode::SubAgentUnavailable,
                Some(WireErrorCode::SubAgentDenied),
            ) {
                ProviderWireError::Unavailable => SubAgentHostError::Unavailable,
                ProviderWireError::Rejected => SubAgentHostError::Rejected,
                ProviderWireError::Cancelled => SubAgentHostError::Cancelled,
                ProviderWireError::ProtocolViolation => SubAgentHostError::InvalidOutcome,
            },
        )
    }

    fn finish(
        response: &WireMessage,
        kind: PendingSubAgentKind,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        if let Some(error) = Self::response_error(response) {
            return Err(error);
        }
        match kind {
            PendingSubAgentKind::Create => response_result::<SubAgentCreateResult>(response)
                .map_err(|_| SubAgentHostError::InvalidOutcome)
                .and_then(|result| SubAgentId::parse(result.sub_agent_id))
                .map(SubAgentProviderPoll::Created),
            PendingSubAgentKind::Message => response_result::<AgentMessageResult>(response)
                .map_err(|_| SubAgentHostError::InvalidOutcome)
                .map(|result| SubAgentProviderPoll::Message(result.accepted)),
            PendingSubAgentKind::Response => response_result::<TypedResponseResult>(response)
                .map_err(|_| SubAgentHostError::InvalidOutcome)
                .map(|result| SubAgentProviderPoll::Response(result.value)),
        }
    }

    fn cancel_active(&mut self, pending: PendingEffectId) {
        if let Some(active) = self.active.remove(&pending.0) {
            cancel_outbound_request(
                active.outbound,
                &self.writer,
                &self.tracker,
                &self.pending,
                &self.disconnected,
            );
        }
    }
}

impl SubAgentProvider for WireSubAgentProvider {
    fn identity(&self) -> &'static str {
        "josh:sub-agent"
    }

    fn create(
        &mut self,
        call: &SubAgentCreateCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentId, SubAgentHostError> {
        let params = SubAgentCreateParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", call.operation_id),
            prompt: structured_prompt_payload(&call.prompt),
            projection: Self::projection(&call.projection),
            deadline_ms: duration_ms(call.deadline),
        };
        params.validate().map_err(|_| SubAgentHostError::Rejected)?;
        let response = self.send(
            "sub_agent/create",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.deadline,
            cancellation,
        )?;
        match Self::finish(&response, PendingSubAgentKind::Create)? {
            SubAgentProviderPoll::Created(id) => Ok(id),
            _ => Err(SubAgentHostError::InvalidOutcome),
        }
    }

    fn run(
        &mut self,
        call: &SubAgentRunCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<Value, SubAgentHostError> {
        let params = sub_agent_run_params(&self.execution_id, call)?;
        let response = self.send(
            "sub_agent/run",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.response.deadline,
            cancellation,
        )?;
        match Self::finish(&response, PendingSubAgentKind::Response)? {
            SubAgentProviderPoll::Response(value) => Ok(value),
            _ => Err(SubAgentHostError::InvalidOutcome),
        }
    }

    fn message(
        &mut self,
        call: &SubAgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<bool, SubAgentHostError> {
        let params = SubAgentMessageParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", call.operation_id),
            sub_agent_id: call.target.as_str().to_owned(),
            message: call.message.clone(),
            deadline_ms: duration_ms(call.deadline),
        };
        params.validate().map_err(|_| SubAgentHostError::Rejected)?;
        let response = self.send(
            "sub_agent/message",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.deadline,
            cancellation,
        )?;
        match Self::finish(&response, PendingSubAgentKind::Message)? {
            SubAgentProviderPoll::Message(accepted) => Ok(accepted),
            _ => Err(SubAgentHostError::InvalidOutcome),
        }
    }

    fn ask(
        &mut self,
        call: &SubAgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<Value, SubAgentHostError> {
        let params = sub_agent_ask_params(&self.execution_id, call)?;
        let response = self.send(
            "sub_agent/ask",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.response.deadline,
            cancellation,
        )?;
        match Self::finish(&response, PendingSubAgentKind::Response)? {
            SubAgentProviderPoll::Response(value) => Ok(value),
            _ => Err(SubAgentHostError::InvalidOutcome),
        }
    }

    fn start_create(
        &mut self,
        pending: PendingEffectId,
        call: &SubAgentCreateCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        if cancellation.is_cancelled() {
            return Err(SubAgentHostError::Cancelled);
        }
        let params = SubAgentCreateParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", call.operation_id),
            prompt: structured_prompt_payload(&call.prompt),
            projection: Self::projection(&call.projection),
            deadline_ms: duration_ms(call.deadline),
        };
        params.validate().map_err(|_| SubAgentHostError::Rejected)?;
        self.start(
            pending,
            "sub_agent/create",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.deadline,
            PendingSubAgentKind::Create,
        )
    }

    fn start_run(
        &mut self,
        pending: PendingEffectId,
        call: &SubAgentRunCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        if cancellation.is_cancelled() {
            return Err(SubAgentHostError::Cancelled);
        }
        let params = sub_agent_run_params(&self.execution_id, call)?;
        self.start(
            pending,
            "sub_agent/run",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.response.deadline,
            PendingSubAgentKind::Response,
        )
    }

    fn start_message(
        &mut self,
        pending: PendingEffectId,
        call: &SubAgentMessageCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        if cancellation.is_cancelled() {
            return Err(SubAgentHostError::Cancelled);
        }
        let params = SubAgentMessageParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", call.operation_id),
            sub_agent_id: call.target.as_str().to_owned(),
            message: call.message.clone(),
            deadline_ms: duration_ms(call.deadline),
        };
        params.validate().map_err(|_| SubAgentHostError::Rejected)?;
        self.start(
            pending,
            "sub_agent/message",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.deadline,
            PendingSubAgentKind::Message,
        )
    }

    fn start_ask(
        &mut self,
        pending: PendingEffectId,
        call: &SubAgentAskCall,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        if cancellation.is_cancelled() {
            return Err(SubAgentHostError::Cancelled);
        }
        let params = sub_agent_ask_params(&self.execution_id, call)?;
        self.start(
            pending,
            "sub_agent/ask",
            serde_json::to_value(params).map_err(|_| SubAgentHostError::Transport)?,
            call.response.deadline,
            PendingSubAgentKind::Response,
        )
    }

    fn poll(
        &mut self,
        pending: PendingEffectId,
        cancellation: &mut dyn AgentCancellationSignal,
    ) -> Result<SubAgentProviderPoll, SubAgentHostError> {
        if cancellation.is_cancelled() {
            self.cancel_active(pending);
            return Err(SubAgentHostError::Cancelled);
        }
        let response = match self
            .active
            .get(&pending.0)
            .ok_or(SubAgentHostError::InvalidOutcome)
            .and_then(|active| {
                poll_outbound_request(&active.outbound).map_err(sub_agent_outbound_error)
            }) {
            Ok(Some(response)) => response,
            Ok(None) => return Ok(SubAgentProviderPoll::Pending),
            Err(error) => {
                self.cancel_active(pending);
                return Err(error);
            }
        };
        let active = self
            .active
            .remove(&pending.0)
            .ok_or(SubAgentHostError::InvalidOutcome)?;
        Self::finish(&response, active.kind)
    }

    fn cancel(
        &mut self,
        pending: PendingEffectId,
        _execution_id: allen_runtime::ExternalExecutionId,
        _operation_id: u64,
    ) {
        self.cancel_active(pending);
    }
}

fn sub_agent_run_params(
    execution_id: &str,
    call: &SubAgentRunCall,
) -> Result<SubAgentRunParams, SubAgentHostError> {
    let (operation_id, interaction_id, prompt, response_schema, validation_issues, deadline_ms) =
        WireSubAgentProvider::response_fields(&call.response)?;
    let params = SubAgentRunParams {
        execution_id: execution_id.to_owned(),
        operation_id,
        interaction_id,
        prompt,
        projection: WireSubAgentProvider::projection(&call.projection),
        response_schema,
        attempt: call.response.attempt,
        validation_issues,
        deadline_ms,
    };
    params.validate().map_err(|_| SubAgentHostError::Rejected)?;
    Ok(params)
}

fn sub_agent_ask_params(
    execution_id: &str,
    call: &SubAgentAskCall,
) -> Result<SubAgentAskParams, SubAgentHostError> {
    let (operation_id, interaction_id, prompt, response_schema, validation_issues, deadline_ms) =
        WireSubAgentProvider::response_fields(&call.response)?;
    let params = SubAgentAskParams {
        execution_id: execution_id.to_owned(),
        operation_id,
        sub_agent_id: call.target.as_str().to_owned(),
        interaction_id,
        prompt,
        response_schema,
        attempt: call.response.attempt,
        validation_issues,
        deadline_ms,
    };
    params.validate().map_err(|_| SubAgentHostError::Rejected)?;
    Ok(params)
}

const fn sub_agent_outbound_error(error: OutboundError) -> SubAgentHostError {
    match error {
        OutboundError::Cancelled => SubAgentHostError::Cancelled,
        OutboundError::Deadline => SubAgentHostError::Timeout,
        OutboundError::Transport => SubAgentHostError::Transport,
    }
}

const fn response_outbound_error(error: OutboundError) -> ResponseHostError {
    match error {
        OutboundError::Cancelled => ResponseHostError::Cancelled,
        OutboundError::Deadline => ResponseHostError::Timeout,
        OutboundError::Transport => ResponseHostError::Transport,
    }
}

const fn agent_outbound_error(error: OutboundError) -> AgentHostError {
    match error {
        OutboundError::Cancelled => AgentHostError::Cancelled,
        OutboundError::Deadline => AgentHostError::Timeout,
        OutboundError::Transport => AgentHostError::Transport,
    }
}

struct WireExternalGrantProvider {
    execution_id: String,
    session_id: String,
    writer: Arc<ConnectionWriter>,
    tracker: Arc<Mutex<ProtocolTracker>>,
    pending: Arc<PendingRegistry>,
    outgoing_ids: Arc<AtomicU64>,
    disconnected: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    grants: Arc<GrantRegistry>,
    event_sequence: Arc<AtomicU64>,
    clock: Arc<dyn EventClock>,
    replayed: Arc<AtomicBool>,
    active: BTreeMap<u64, PendingPermissionRequest>,
}

struct PendingPermissionRequest {
    outbound: PendingOutbound,
    request: ExternalGrantRequest,
    params: PermissionRequestParams,
}

impl WireExternalGrantProvider {
    #[allow(clippy::too_many_arguments)]
    fn new(
        execution_id: String,
        session_id: String,
        writer: Arc<ConnectionWriter>,
        tracker: Arc<Mutex<ProtocolTracker>>,
        pending: Arc<PendingRegistry>,
        outgoing_ids: Arc<AtomicU64>,
        disconnected: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        deadline: Instant,
        grants: Arc<GrantRegistry>,
        event_sequence: Arc<AtomicU64>,
        clock: Arc<dyn EventClock>,
        replayed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            execution_id,
            session_id,
            writer,
            tracker,
            pending,
            outgoing_ids,
            disconnected,
            cancelled,
            deadline,
            grants,
            event_sequence,
            clock,
            replayed,
            active: BTreeMap::new(),
        }
    }

    fn decision_event(
        &self,
        operation_id: u64,
        decision: &str,
        reason_code: &str,
    ) -> Result<(), VmError> {
        let event = ExecutionEventParams {
            execution_id: self.execution_id.clone(),
            sequence: self.event_sequence.load(Ordering::Relaxed),
            elapsed_ms: self.clock.elapsed_ms(),
            kind: EventKind::PermissionDecision,
            replayed: self.replayed.load(Ordering::Acquire),
            fields: BTreeMap::from([
                ("decision".to_owned(), Value::String(decision.to_owned())),
                (
                    "operation_id".to_owned(),
                    Value::String(format!("op-{operation_id}")),
                ),
                (
                    "reason_code".to_owned(),
                    Value::String(reason_code.to_owned()),
                ),
            ]),
        };
        event.validate().map_err(|_| VmError::AgentUnavailable)?;
        write_event(&self.writer, &event).map_err(|_| VmError::AgentUnavailable)?;
        self.event_sequence.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn wire_error(response: &WireMessage) -> Option<VmError> {
        let WireMessage::Response {
            result: None,
            error: Some(error),
            ..
        } = response
        else {
            return None;
        };
        Some(
            match classify_provider_wire_error(
                error.code,
                WireErrorCode::PermissionUnavailable,
                None,
            ) {
                ProviderWireError::Unavailable => VmError::AgentUnavailable,
                ProviderWireError::Cancelled => VmError::Cancelled,
                ProviderWireError::Rejected | ProviderWireError::ProtocolViolation => {
                    VmError::ProtocolViolation
                }
            },
        )
    }
}

impl ExternalGrantDecisionProvider for WireExternalGrantProvider {
    fn decide(&mut self, request: &ExternalGrantRequest) -> Result<ExternalGrantDecision, VmError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(VmError::Cancelled);
        }
        let kind = match request.kind {
            ExternalTargetKind::File => PermissionTargetKind::File,
            ExternalTargetKind::Directory => PermissionTargetKind::Directory,
        };
        let mut rights = Vec::new();
        if request.rights.read {
            rights.push(PermissionRight::Read);
            if request.kind == ExternalTargetKind::Directory {
                rights.push(PermissionRight::List);
            }
        }
        if request.rights.write {
            rights.push(PermissionRight::Write);
        }
        rights.sort_unstable();
        let params = PermissionRequestParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", request.operation_id),
            session_id: self.session_id.clone(),
            pending_target_id: format!("pending-target-{}", request.pending_target_id),
            kind,
            path: request
                .path
                .to_str()
                .ok_or(VmError::AgentUnavailable)?
                .to_owned(),
            rights,
            recursive: request.recursive,
            max_bytes: request.max_bytes,
            duration: GrantDuration::Execution,
            reason: request.reason.clone(),
        };
        params.validate().map_err(|_| VmError::AgentUnavailable)?;
        let response = send_outbound_request(
            "permission/request",
            serde_json::to_value(&params).map_err(|_| VmError::AgentUnavailable)?,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            &self.disconnected,
            self.deadline,
            || self.cancelled.load(Ordering::Acquire),
        )
        .map_err(|error| match error {
            OutboundError::Cancelled => VmError::Cancelled,
            OutboundError::Deadline => VmError::Timeout {
                resource: allen_vm::RESOURCE_WALL_TIME,
            },
            OutboundError::Transport => VmError::AgentUnavailable,
        })?;
        if let Some(error) = Self::wire_error(&response) {
            return Err(error);
        }
        let result = response_result::<PermissionRequestResult>(&response)
            .map_err(|_| VmError::AgentUnavailable)?;
        result
            .validate_for(&params)
            .map_err(|_| VmError::AgentUnavailable)?;
        match result {
            PermissionRequestResult::Deny { reason_code } => {
                self.decision_event(request.operation_id, "deny", &reason_code)?;
                Ok(ExternalGrantDecision::Deny)
            }
            PermissionRequestResult::Allow {
                grant_id,
                path,
                rights,
                recursive,
                max_bytes,
                duration: GrantDuration::Execution,
            } => {
                let approved = Rights::new(
                    rights.contains(&PermissionRight::Read),
                    rights.contains(&PermissionRight::Write),
                );
                self.decision_event(request.operation_id, "allow", "allowed")?;
                self.grants.allowed(request.pending_target_id, grant_id)?;
                Ok(ExternalGrantDecision::Allow {
                    execution_id: request.execution_id,
                    kind: request.kind,
                    path: path.into(),
                    rights: approved,
                    recursive,
                    max_bytes,
                    duration: RuntimeGrantDuration::Execution(request.execution_id),
                })
            }
        }
    }

    fn grant_issued(&mut self, pending_target_id: u64, grant_id: ExternalGrantId) {
        self.grants.issued(pending_target_id, grant_id);
    }

    fn take_revocations(&mut self) -> Result<Vec<ExternalGrantId>, VmError> {
        self.grants.take_revocations()
    }

    fn start_decide(
        &mut self,
        pending_id: PendingEffectId,
        request: &ExternalGrantRequest,
    ) -> Result<ExternalGrantPoll, VmError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(VmError::Cancelled);
        }
        if self.active.contains_key(&pending_id.0) {
            return Err(VmError::AgentUnavailable);
        }
        let kind = match request.kind {
            ExternalTargetKind::File => PermissionTargetKind::File,
            ExternalTargetKind::Directory => PermissionTargetKind::Directory,
        };
        let mut rights = Vec::new();
        if request.rights.read {
            rights.push(PermissionRight::Read);
            if request.kind == ExternalTargetKind::Directory {
                rights.push(PermissionRight::List);
            }
        }
        if request.rights.write {
            rights.push(PermissionRight::Write);
        }
        rights.sort_unstable();
        let params = PermissionRequestParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", request.operation_id),
            session_id: self.session_id.clone(),
            pending_target_id: format!("pending-target-{}", request.pending_target_id),
            kind,
            path: request
                .path
                .to_str()
                .ok_or(VmError::AgentUnavailable)?
                .to_owned(),
            rights,
            recursive: request.recursive,
            max_bytes: request.max_bytes,
            duration: GrantDuration::Execution,
            reason: request.reason.clone(),
        };
        params.validate().map_err(|_| VmError::AgentUnavailable)?;
        let outbound = start_outbound_request(
            "permission/request",
            serde_json::to_value(&params).map_err(|_| VmError::AgentUnavailable)?,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            self.deadline,
        )
        .map_err(permission_outbound_error)?;
        self.active.insert(
            pending_id.0,
            PendingPermissionRequest {
                outbound,
                request: request.clone(),
                params,
            },
        );
        Ok(ExternalGrantPoll::Pending)
    }

    fn poll(&mut self, pending_id: PendingEffectId) -> Result<ExternalGrantPoll, VmError> {
        if self.cancelled.load(Ordering::Acquire) {
            self.cancel_permission_request(pending_id);
            return Err(VmError::Cancelled);
        }
        let poll = self
            .active
            .get(&pending_id.0)
            .ok_or(VmError::AgentUnavailable)
            .and_then(|active| {
                poll_outbound_request(&active.outbound).map_err(permission_outbound_error)
            });
        let response = match poll {
            Ok(Some(response)) => response,
            Ok(None) => return Ok(ExternalGrantPoll::Pending),
            Err(error) => {
                self.cancel_permission_request(pending_id);
                return Err(error);
            }
        };
        let active = self
            .active
            .remove(&pending_id.0)
            .ok_or(VmError::AgentUnavailable)?;
        if let Some(error) = Self::wire_error(&response) {
            return Err(error);
        }
        let result = response_result::<PermissionRequestResult>(&response)
            .map_err(|_| VmError::AgentUnavailable)?;
        result
            .validate_for(&active.params)
            .map_err(|_| VmError::AgentUnavailable)?;
        let decision = match result {
            PermissionRequestResult::Deny { reason_code } => {
                self.decision_event(active.request.operation_id, "deny", &reason_code)?;
                ExternalGrantDecision::Deny
            }
            PermissionRequestResult::Allow {
                grant_id,
                path,
                rights,
                recursive,
                max_bytes,
                duration: GrantDuration::Execution,
            } => {
                let approved = Rights::new(
                    rights.contains(&PermissionRight::Read),
                    rights.contains(&PermissionRight::Write),
                );
                self.decision_event(active.request.operation_id, "allow", "allowed")?;
                self.grants
                    .allowed(active.request.pending_target_id, grant_id)?;
                ExternalGrantDecision::Allow {
                    execution_id: active.request.execution_id,
                    kind: active.request.kind,
                    path: path.into(),
                    rights: approved,
                    recursive,
                    max_bytes,
                    duration: RuntimeGrantDuration::Execution(active.request.execution_id),
                }
            }
        };
        Ok(ExternalGrantPoll::Decision(decision))
    }

    fn cancel_pending(&mut self, pending_id: PendingEffectId) {
        self.cancel_permission_request(pending_id);
    }
}

impl WireExternalGrantProvider {
    fn cancel_permission_request(&mut self, pending_id: PendingEffectId) {
        if let Some(active) = self.active.remove(&pending_id.0) {
            cancel_outbound_request(
                active.outbound,
                &self.writer,
                &self.tracker,
                &self.pending,
                &self.disconnected,
            );
        }
    }
}

const fn permission_outbound_error(error: OutboundError) -> VmError {
    match error {
        OutboundError::Cancelled => VmError::Cancelled,
        OutboundError::Deadline => VmError::Timeout {
            resource: allen_vm::RESOURCE_WALL_TIME,
        },
        OutboundError::Transport => VmError::AgentUnavailable,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn runtime_transcript(snapshot: josh_protocol::TranscriptSnapshot) -> RuntimeTranscriptSnapshot {
    RuntimeTranscriptSnapshot {
        snapshot_id: snapshot.snapshot_id,
        session_id: snapshot.session_id,
        policy_version: snapshot.policy_version,
        captured_at: snapshot.captured_at,
        truncated: snapshot.truncated,
        messages: snapshot
            .messages
            .into_iter()
            .map(|message| RuntimeTranscriptMessage {
                id: message.id,
                role: match message.role {
                    TranscriptRole::User => RuntimeTranscriptRole::User,
                    TranscriptRole::Assistant => RuntimeTranscriptRole::Assistant,
                    TranscriptRole::SystemVisible => RuntimeTranscriptRole::SystemVisible,
                    TranscriptRole::Tool => RuntimeTranscriptRole::Tool,
                },
                time: message.time,
                content: message
                    .content
                    .into_iter()
                    .map(|part| match part {
                        TranscriptPart::Text { text } => RuntimeTranscriptPart::Text { text },
                        TranscriptPart::Json { value } => RuntimeTranscriptPart::Json { value },
                        TranscriptPart::ToolCall {
                            name,
                            call_id,
                            input,
                        } => RuntimeTranscriptPart::ToolCall {
                            name,
                            call_id,
                            input,
                        },
                        TranscriptPart::ToolResult {
                            call_id,
                            output,
                            is_error,
                        } => RuntimeTranscriptPart::ToolResult {
                            call_id,
                            output,
                            is_error,
                        },
                        TranscriptPart::Attachment {
                            media_type,
                            name,
                            content_ref,
                        } => RuntimeTranscriptPart::Attachment {
                            media_type,
                            name,
                            content_ref,
                        },
                        TranscriptPart::Redacted { reason_code } => {
                            RuntimeTranscriptPart::Redacted { reason_code }
                        }
                        TranscriptPart::Omitted {
                            content_kind,
                            count,
                        } => RuntimeTranscriptPart::Omitted {
                            content_kind,
                            count,
                        },
                    })
                    .collect(),
            })
            .collect(),
    }
}

struct WireTaskObserver {
    execution_id: String,
    writer: Arc<ConnectionWriter>,
    sequence: Arc<AtomicU64>,
    clock: Arc<dyn EventClock>,
    cancelled: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    deadline: Instant,
    replayed: Arc<AtomicBool>,
    started: bool,
}

impl CheckpointObserver for WireTaskObserver {
    fn checkpoint(&mut self, _checkpoint: Checkpoint) {}

    fn execution_effect_provenance(&mut self, replayed: bool) {
        self.replayed.store(replayed, Ordering::Release);
        if self.started || self.disconnected.load(Ordering::Acquire) {
            return;
        }
        let event = ExecutionEventParams {
            execution_id: self.execution_id.clone(),
            sequence: 2,
            elapsed_ms: 0,
            kind: EventKind::Started,
            replayed,
            fields: BTreeMap::new(),
        };
        let Ok(params) = serde_json::to_value(event) else {
            self.cancelled.store(true, Ordering::Release);
            return;
        };
        let message = WireMessage::Notification {
            method: "execution/event".to_owned(),
            params,
        };
        if self
            .writer
            .write_message_until(&message, self.deadline)
            .is_err()
        {
            self.cancelled.store(true, Ordering::Release);
        } else {
            self.started = true;
        }
    }

    fn task_event(&mut self, task: TaskEvent) {
        if self.disconnected.load(Ordering::Acquire) {
            return;
        }
        let kind = match task.kind {
            TaskEventKind::Spawned => EventKind::TaskStarted,
            TaskEventKind::Cancelled => EventKind::TaskCancelled,
            TaskEventKind::Waiting
            | TaskEventKind::Ready
            | TaskEventKind::Completed
            | TaskEventKind::Failed
            | TaskEventKind::Stopped => return,
        };
        let sequence = self.sequence.load(Ordering::Relaxed);
        let elapsed_ms = self.clock.elapsed_ms();
        let event = ExecutionEventParams {
            execution_id: self.execution_id.clone(),
            sequence,
            elapsed_ms,
            kind,
            replayed: self.replayed.load(Ordering::Acquire),
            fields: BTreeMap::from([
                ("owner_task_id".to_owned(), Value::from(task.owner_id)),
                ("task_id".to_owned(), Value::from(task.task_id)),
            ]),
        };
        let message = WireMessage::Notification {
            method: "execution/event".to_owned(),
            params: if let Ok(params) = serde_json::to_value(event) {
                params
            } else {
                self.cancelled.store(true, Ordering::Release);
                return;
            },
        };
        if self
            .writer
            .write_message_until(&message, self.deadline)
            .is_err()
        {
            self.cancelled.store(true, Ordering::Release);
        } else {
            self.sequence.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn budget_warning(&mut self, warning: BudgetWarning) {
        if self.disconnected.load(Ordering::Acquire) {
            return;
        }
        let sequence = self.sequence.load(Ordering::Relaxed);
        let event = ExecutionEventParams {
            execution_id: self.execution_id.clone(),
            sequence,
            elapsed_ms: self.clock.elapsed_ms(),
            kind: EventKind::BudgetWarning,
            replayed: self.replayed.load(Ordering::Acquire),
            fields: BTreeMap::from([
                (
                    "resource".to_owned(),
                    Value::String(warning.resource.to_owned()),
                ),
                ("used".to_owned(), Value::from(warning.used)),
                ("limit".to_owned(), Value::from(warning.limit)),
            ]),
        };
        let Ok(params) = serde_json::to_value(event) else {
            self.cancelled.store(true, Ordering::Release);
            return;
        };
        let message = WireMessage::Notification {
            method: "execution/event".to_owned(),
            params,
        };
        if self
            .writer
            .write_message_until(&message, self.deadline)
            .is_err()
        {
            self.cancelled.store(true, Ordering::Release);
        } else {
            self.sequence.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl WireToolProvider {
    #[allow(clippy::too_many_arguments)]
    fn new(
        execution_id: String,
        catalog: Arc<allen_schema::FrozenCatalog>,
        writer: Arc<ConnectionWriter>,
        tracker: Arc<Mutex<ProtocolTracker>>,
        pending: Arc<PendingRegistry>,
        disconnected: Arc<AtomicBool>,
        event_sequence: Arc<AtomicU64>,
        clock: Arc<dyn EventClock>,
        outgoing_ids: Arc<AtomicU64>,
        replayed: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        protocol_violation: Arc<AtomicBool>,
    ) -> Self {
        Self {
            execution_id,
            catalog,
            writer,
            tracker,
            pending,
            outgoing_ids,
            disconnected,
            event_sequence,
            clock,
            replayed,
            cancelled,
            protocol_violation,
            active: BTreeMap::new(),
        }
    }

    fn terminal_protocol_violation(&self) -> ToolHostError {
        self.protocol_violation.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
        ToolHostError::Cancelled
    }

    fn effect_event(
        &self,
        kind: EventKind,
        invocation: &ToolInvocation,
        schema_digest: &str,
        deadline: Instant,
    ) -> Result<(), ToolHostError> {
        let name = ToolName::parse(&invocation.name).map_err(|_| ToolHostError::Rejected)?;
        let version = allen_schema::ExactVersion::parse(&invocation.version)
            .map_err(|_| ToolHostError::Rejected)?;
        let effect = allen_schema::generated_tool_effect(&name, version)
            .map_err(|_| ToolHostError::Rejected)?;
        let sequence = self.event_sequence.load(Ordering::Relaxed);
        let elapsed_ms = self.clock.elapsed_ms();
        let event = ExecutionEventParams {
            execution_id: self.execution_id.clone(),
            sequence,
            elapsed_ms,
            kind,
            replayed: self.replayed.load(Ordering::Acquire),
            fields: BTreeMap::from([
                ("effect".to_owned(), Value::String(effect)),
                (
                    "operation_id".to_owned(),
                    Value::String(format!("op-{}", invocation.operation_id)),
                ),
                (
                    "schema_digest".to_owned(),
                    Value::String(schema_digest.to_owned()),
                ),
            ]),
        };
        event.validate().map_err(|_| ToolHostError::Transport)?;
        let message = WireMessage::Notification {
            method: "execution/event".to_owned(),
            params: serde_json::to_value(event).map_err(|_| ToolHostError::Transport)?,
        };
        self.writer
            .write_message_until(&message, deadline)
            .map_err(|_| ToolHostError::Transport)?;
        self.event_sequence.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl ToolProvider for WireToolProvider {
    #[allow(clippy::too_many_lines)]
    fn invoke(
        &mut self,
        invocation: &ToolInvocation,
        input: Value,
        cancellation: &mut dyn ToolCancellationSignal,
    ) -> Result<ToolOutcome, ToolHostError> {
        let name = ToolName::parse(&invocation.name).map_err(|_| ToolHostError::Rejected)?;
        let definition = self.catalog.get(&name).ok_or(ToolHostError::Rejected)?;
        let request_number = self.outgoing_ids.fetch_add(1, Ordering::Relaxed);
        if request_number == u64::MAX {
            return Err(ToolHostError::Transport);
        }
        let request_id = format!("r-{request_number}");
        let deadline_ms = u64::try_from(invocation.deadline.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let deadline = Instant::now()
            .checked_add(invocation.deadline)
            .unwrap_or_else(Instant::now);
        let params = ToolInvokeParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", invocation.operation_id),
            tool: invocation.name.clone(),
            tool_version: invocation.version.clone(),
            catalog_digest: invocation.catalog_digest.clone(),
            input_schema: definition.input_schema.digest().to_owned(),
            output_schema: definition.output_schema.digest().to_owned(),
            error_schema: definition.error_schema.digest().to_owned(),
            input,
            deadline_ms,
        };
        params.validate().map_err(|_| ToolHostError::Rejected)?;
        let message = WireMessage::Request {
            id: request_id.clone(),
            method: "tool/invoke".to_owned(),
            params: serde_json::to_value(params).map_err(|_| ToolHostError::Transport)?,
        };
        self.effect_event(
            EventKind::EffectStarted,
            invocation,
            definition.input_schema.digest(),
            deadline,
        )?;
        let call = Arc::new(PendingCall::default());
        self.pending
            .insert(request_id.clone(), Arc::clone(&call))
            .map_err(|()| ToolHostError::Transport)?;
        if self
            .tracker
            .lock()
            .map_err(|_| ToolHostError::Transport)?
            .register_outgoing_request(&request_id, "tool/invoke")
            .is_err()
        {
            self.pending.remove(&request_id);
            return Err(ToolHostError::Transport);
        }
        if self.writer.write_message_until(&message, deadline).is_err() {
            self.pending.remove(&request_id);
            let _ = self
                .tracker
                .lock()
                .map(|mut tracker| tracker.cancel_outgoing(&request_id));
            return Err(ToolHostError::Transport);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = call
            .wait(|| cancellation.is_cancelled(), remaining)
            .map_err(|()| ToolHostError::Transport)?;
        let PendingWait::Response(response) = wait else {
            self.pending.remove(&request_id);
            let cancelled = self
                .tracker
                .lock()
                .map_err(|_| ToolHostError::Transport)?
                .cancel_outgoing(&request_id);
            if cancelled && !self.disconnected.load(Ordering::Acquire) {
                let _ = self.writer.write_message_until(
                    &WireMessage::Cancel {
                        id: request_id.clone(),
                        reason: None,
                    },
                    deadline,
                );
            }
            if !self.disconnected.load(Ordering::Acquire) {
                let _ = self.effect_event(
                    EventKind::EffectFailed,
                    invocation,
                    definition.error_schema.digest(),
                    deadline,
                );
            }
            return Err(match wait {
                PendingWait::Deadline => ToolHostError::Timeout,
                PendingWait::Cancelled => ToolHostError::Cancelled,
                PendingWait::Response(_) => unreachable!(),
            });
        };
        let mut outcome = match response {
            WireMessage::Response {
                result: Some(_),
                error: None,
                ..
            } => match response_result::<ToolInvokeResult>(&response) {
                Ok(ToolInvokeResult::Ok { value }) => Ok(ToolOutcome::Output(value)),
                Ok(ToolInvokeResult::Error { error }) => Ok(ToolOutcome::DeclaredError(error)),
                Err(_) => Err(ToolHostError::InvalidOutcome),
            },
            WireMessage::Response {
                result: None,
                error: Some(error),
                ..
            } => match classify_provider_wire_error(
                error.code,
                WireErrorCode::ToolUnavailable,
                Some(WireErrorCode::ToolDenied),
            ) {
                ProviderWireError::Unavailable => Err(ToolHostError::Unavailable),
                ProviderWireError::Rejected => Err(ToolHostError::Rejected),
                ProviderWireError::Cancelled => Err(ToolHostError::Cancelled),
                ProviderWireError::ProtocolViolation => Err(self.terminal_protocol_violation()),
            },
            _ => Err(ToolHostError::InvalidOutcome),
        };
        if let Ok(value) = &outcome {
            let validation = match value {
                ToolOutcome::Output(value) => definition
                    .output_schema
                    .validate(value, &allen_schema::SchemaLimits::default()),
                ToolOutcome::DeclaredError(value) => definition
                    .error_schema
                    .validate(value, &allen_schema::SchemaLimits::default()),
            };
            if validation.is_err() {
                outcome = Err(ToolHostError::InvalidOutcome);
            }
        }
        let (kind, digest) = match &outcome {
            Ok(ToolOutcome::Output(_)) => (
                EventKind::EffectCompleted,
                definition.output_schema.digest(),
            ),
            Ok(ToolOutcome::DeclaredError(_)) => {
                (EventKind::EffectCompleted, definition.error_schema.digest())
            }
            Err(_) => (EventKind::EffectFailed, definition.error_schema.digest()),
        };
        self.effect_event(kind, invocation, digest, deadline)?;
        outcome
    }

    fn start_invoke(
        &mut self,
        pending_id: PendingEffectId,
        invocation: &ToolInvocation,
        input: Value,
        cancellation: &mut dyn ToolCancellationSignal,
    ) -> Result<ToolProviderPoll, ToolHostError> {
        if cancellation.is_cancelled() {
            return Err(ToolHostError::Cancelled);
        }
        if self.active.contains_key(&pending_id.0) {
            return Err(ToolHostError::Rejected);
        }
        let name = ToolName::parse(&invocation.name).map_err(|_| ToolHostError::Rejected)?;
        let definition = self
            .catalog
            .get(&name)
            .cloned()
            .ok_or(ToolHostError::Rejected)?;
        let deadline = Instant::now()
            .checked_add(invocation.deadline)
            .unwrap_or_else(Instant::now);
        let params = ToolInvokeParams {
            execution_id: self.execution_id.clone(),
            operation_id: format!("op-{}", invocation.operation_id),
            tool: invocation.name.clone(),
            tool_version: invocation.version.clone(),
            catalog_digest: invocation.catalog_digest.clone(),
            input_schema: definition.input_schema.digest().to_owned(),
            output_schema: definition.output_schema.digest().to_owned(),
            error_schema: definition.error_schema.digest().to_owned(),
            input,
            deadline_ms: duration_ms(invocation.deadline),
        };
        params.validate().map_err(|_| ToolHostError::Rejected)?;
        self.effect_event(
            EventKind::EffectStarted,
            invocation,
            definition.input_schema.digest(),
            deadline,
        )?;
        let Ok(outbound) = start_outbound_request(
            "tool/invoke",
            serde_json::to_value(params).map_err(|_| ToolHostError::Transport)?,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.outgoing_ids,
            deadline,
        ) else {
            let _ = self.effect_event(
                EventKind::EffectFailed,
                invocation,
                definition.error_schema.digest(),
                deadline,
            );
            return Err(ToolHostError::Transport);
        };
        self.active.insert(
            pending_id.0,
            PendingToolRequest {
                outbound,
                invocation: invocation.clone(),
                definition,
            },
        );
        Ok(ToolProviderPoll::Pending)
    }

    fn poll(
        &mut self,
        pending_id: PendingEffectId,
        cancellation: &mut dyn ToolCancellationSignal,
    ) -> Result<ToolProviderPoll, ToolHostError> {
        if cancellation.is_cancelled() {
            self.cancel_tool_request(pending_id);
            return Err(ToolHostError::Cancelled);
        }
        let poll = self
            .active
            .get(&pending_id.0)
            .ok_or(ToolHostError::InvalidOutcome)
            .and_then(|active| {
                poll_outbound_request(&active.outbound).map_err(tool_outbound_error)
            });
        let response = match poll {
            Ok(Some(response)) => response,
            Ok(None) => return Ok(ToolProviderPoll::Pending),
            Err(error) => {
                self.cancel_tool_request(pending_id);
                return Err(error);
            }
        };
        let active = self
            .active
            .remove(&pending_id.0)
            .ok_or(ToolHostError::InvalidOutcome)?;
        let mut outcome = match response {
            WireMessage::Response {
                result: Some(_),
                error: None,
                ..
            } => match response_result::<ToolInvokeResult>(&response) {
                Ok(ToolInvokeResult::Ok { value }) => Ok(ToolOutcome::Output(value)),
                Ok(ToolInvokeResult::Error { error }) => Ok(ToolOutcome::DeclaredError(error)),
                Err(_) => Err(ToolHostError::InvalidOutcome),
            },
            WireMessage::Response {
                result: None,
                error: Some(error),
                ..
            } => match classify_provider_wire_error(
                error.code,
                WireErrorCode::ToolUnavailable,
                Some(WireErrorCode::ToolDenied),
            ) {
                ProviderWireError::Unavailable => Err(ToolHostError::Unavailable),
                ProviderWireError::Rejected => Err(ToolHostError::Rejected),
                ProviderWireError::Cancelled => Err(ToolHostError::Cancelled),
                ProviderWireError::ProtocolViolation => Err(self.terminal_protocol_violation()),
            },
            _ => Err(ToolHostError::InvalidOutcome),
        };
        if let Ok(value) = &outcome {
            let validation = match value {
                ToolOutcome::Output(value) => active
                    .definition
                    .output_schema
                    .validate(value, &allen_schema::SchemaLimits::default()),
                ToolOutcome::DeclaredError(value) => active
                    .definition
                    .error_schema
                    .validate(value, &allen_schema::SchemaLimits::default()),
            };
            if validation.is_err() {
                outcome = Err(ToolHostError::InvalidOutcome);
            }
        }
        let (kind, digest) = match &outcome {
            Ok(ToolOutcome::Output(_)) => (
                EventKind::EffectCompleted,
                active.definition.output_schema.digest(),
            ),
            Ok(ToolOutcome::DeclaredError(_)) => (
                EventKind::EffectCompleted,
                active.definition.error_schema.digest(),
            ),
            Err(_) => (
                EventKind::EffectFailed,
                active.definition.error_schema.digest(),
            ),
        };
        self.effect_event(kind, &active.invocation, digest, active.outbound.deadline)?;
        outcome.map(ToolProviderPoll::Outcome)
    }

    fn cancel_pending(
        &mut self,
        pending_id: PendingEffectId,
        _execution_id: allen_runtime::ExternalExecutionId,
        _operation_id: u64,
    ) {
        self.cancel_tool_request(pending_id);
    }
}

impl WireToolProvider {
    fn cancel_tool_request(&mut self, pending_id: PendingEffectId) {
        let Some(active) = self.active.remove(&pending_id.0) else {
            return;
        };
        let deadline = active.outbound.deadline;
        cancel_outbound_request(
            active.outbound,
            &self.writer,
            &self.tracker,
            &self.pending,
            &self.disconnected,
        );
        if !self.disconnected.load(Ordering::Acquire) {
            let _ = self.effect_event(
                EventKind::EffectFailed,
                &active.invocation,
                active.definition.error_schema.digest(),
                deadline,
            );
        }
    }
}

const fn tool_outbound_error(error: OutboundError) -> ToolHostError {
    match error {
        OutboundError::Cancelled => ToolHostError::Cancelled,
        OutboundError::Deadline => ToolHostError::Timeout,
        OutboundError::Transport => ToolHostError::Transport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    const ALL_WIRE_ERROR_CODES: [WireErrorCode; 24] = [
        WireErrorCode::RequestInvalid,
        WireErrorCode::RequestMethodNotFound,
        WireErrorCode::RequestInvalidState,
        WireErrorCode::RequestLimit,
        WireErrorCode::RequestCancelled,
        WireErrorCode::CatalogInvalid,
        WireErrorCode::CatalogMismatch,
        WireErrorCode::ProgramInvalid,
        WireErrorCode::ProgramUnsatisfied,
        WireErrorCode::ExecutionDuplicate,
        WireErrorCode::ExecutionFailed,
        WireErrorCode::ToolDenied,
        WireErrorCode::ToolUnavailable,
        WireErrorCode::AgentDenied,
        WireErrorCode::AgentUnavailable,
        WireErrorCode::ModelDenied,
        WireErrorCode::ModelUnavailable,
        WireErrorCode::UserDenied,
        WireErrorCode::UserUnavailable,
        WireErrorCode::SubAgentDenied,
        WireErrorCode::SubAgentUnavailable,
        WireErrorCode::ReplayDiverged,
        WireErrorCode::PermissionUnavailable,
        WireErrorCode::ProtocolViolation,
    ];

    const fn wire_error_code_is_known(code: WireErrorCode) -> bool {
        match code {
            WireErrorCode::RequestInvalid
            | WireErrorCode::RequestMethodNotFound
            | WireErrorCode::RequestInvalidState
            | WireErrorCode::RequestLimit
            | WireErrorCode::RequestCancelled
            | WireErrorCode::CatalogInvalid
            | WireErrorCode::CatalogMismatch
            | WireErrorCode::ProgramInvalid
            | WireErrorCode::ProgramUnsatisfied
            | WireErrorCode::ExecutionDuplicate
            | WireErrorCode::ExecutionFailed
            | WireErrorCode::ToolDenied
            | WireErrorCode::ToolUnavailable
            | WireErrorCode::AgentDenied
            | WireErrorCode::AgentUnavailable
            | WireErrorCode::ModelDenied
            | WireErrorCode::ModelUnavailable
            | WireErrorCode::UserDenied
            | WireErrorCode::UserUnavailable
            | WireErrorCode::SubAgentDenied
            | WireErrorCode::SubAgentUnavailable
            | WireErrorCode::ReplayDiverged
            | WireErrorCode::PermissionUnavailable
            | WireErrorCode::ProtocolViolation => true,
        }
    }

    #[test]
    fn provider_wire_errors_accept_only_their_exact_domain_and_cancellation() {
        let domains = [
            (
                WireErrorCode::ToolUnavailable,
                Some(WireErrorCode::ToolDenied),
            ),
            (
                WireErrorCode::AgentUnavailable,
                Some(WireErrorCode::AgentDenied),
            ),
            (
                WireErrorCode::ModelUnavailable,
                Some(WireErrorCode::ModelDenied),
            ),
            (
                WireErrorCode::UserUnavailable,
                Some(WireErrorCode::UserDenied),
            ),
            (
                WireErrorCode::SubAgentUnavailable,
                Some(WireErrorCode::SubAgentDenied),
            ),
            (WireErrorCode::PermissionUnavailable, None),
        ];
        for (expected_unavailable, expected_denied) in domains {
            for code in ALL_WIRE_ERROR_CODES {
                assert!(wire_error_code_is_known(code));
                let expected = if code == expected_unavailable {
                    ProviderWireError::Unavailable
                } else if Some(code) == expected_denied {
                    ProviderWireError::Rejected
                } else if code == WireErrorCode::RequestCancelled {
                    ProviderWireError::Cancelled
                } else {
                    ProviderWireError::ProtocolViolation
                };
                assert_eq!(
                    classify_provider_wire_error(code, expected_unavailable, expected_denied),
                    expected,
                    "{code:?} for {expected_unavailable:?}/{expected_denied:?}"
                );
            }
        }
    }

    #[test]
    fn provider_adapters_map_only_exact_domain_denials_to_rejected() {
        let response = |code| WireMessage::Response {
            id: "provider".to_owned(),
            result: None,
            error: Some(WireError {
                code,
                message: "private provider detail".to_owned(),
                data: Some(serde_json::json!({"secret":"private provider detail"})),
            }),
        };

        assert_eq!(
            WireInvokingAgentProvider::response_error(&response(WireErrorCode::AgentDenied)),
            Some(AgentHostError::Rejected)
        );
        assert_eq!(
            WireInvokingAgentProvider::response_error(&response(WireErrorCode::ModelDenied)),
            Some(AgentHostError::InvalidOutcome)
        );
        assert_eq!(
            WireSubAgentProvider::response_error(&response(WireErrorCode::SubAgentDenied)),
            Some(SubAgentHostError::Rejected)
        );
        assert_eq!(
            WireSubAgentProvider::response_error(&response(WireErrorCode::UserDenied)),
            Some(SubAgentHostError::InvalidOutcome)
        );
    }

    #[test]
    fn provider_protocol_violation_result_is_redacted_and_terminal() {
        let result = provider_protocol_violation_result();
        result.validate().unwrap();
        let encoded = serde_json::to_string(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap()["error"]["code"],
            "protocol.violation"
        );
        assert!(!encoded.contains("provider"));
        assert!(!encoded.contains("unavailable"));
    }

    #[test]
    fn permission_wire_errors_require_the_permission_domain() {
        let response = |code| WireMessage::Response {
            id: "permission".to_owned(),
            result: None,
            error: Some(WireError {
                code,
                message: "provider detail".to_owned(),
                data: Some(serde_json::json!({"secret":"provider detail"})),
            }),
        };
        assert_eq!(
            WireExternalGrantProvider::wire_error(&response(WireErrorCode::PermissionUnavailable)),
            Some(VmError::AgentUnavailable)
        );
        assert_eq!(
            WireExternalGrantProvider::wire_error(&response(WireErrorCode::RequestCancelled)),
            Some(VmError::Cancelled)
        );
        assert_eq!(
            WireExternalGrantProvider::wire_error(&response(WireErrorCode::AgentUnavailable)),
            Some(VmError::ProtocolViolation)
        );
    }

    #[test]
    fn frame_limit_failure_result_fits_when_large_completed_output_does_not() {
        let response = |result: &ExecutionResult| WireMessage::Response {
            id: "request".to_owned(),
            result: Some(serde_json::to_value(result).unwrap()),
            error: None,
        };
        let large = ExecutionResult::Completed {
            output: serde_json::Value::String("x".repeat(2_048)),
        };
        assert!(encode_frame(&response(&large), 1_024).is_err());
        let bounded = frame_limit_result();
        bounded.validate().unwrap();
        assert!(encode_frame(&response(&bounded), 1_024).is_ok());
    }

    struct FixedClock(u64);

    impl EventClock for FixedClock {
        fn elapsed_ms(&self) -> u64 {
            self.0
        }
    }

    struct BrokenWriter;

    impl Write for BrokenWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BlockingWriter(Arc<(Mutex<bool>, Condvar)>);

    #[derive(Clone, Default)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let (gate, ready) = &*self.0;
            let mut released = gate.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn notification(sequence: u64) -> WireMessage {
        WireMessage::Notification {
            method: "execution/event".to_owned(),
            params: serde_json::json!({
                "execution_id": "exec-1",
                "sequence": sequence,
                "elapsed_ms": 0,
                "kind": "started",
                "fields": {}
            }),
        }
    }

    fn test_writer<W: Write + Send + 'static>(
        output: W,
        disconnected: Arc<AtomicBool>,
    ) -> ConnectionWriter {
        ConnectionWriter::new(
            output,
            josh_protocol::DEFAULT_MAX_FRAME_BYTES,
            disconnected,
            Arc::new(Mutex::new(Session::new())),
            Arc::new(Mutex::new(ProtocolTracker::new(
                josh_protocol::PeerRole::Runtime,
                8,
            ))),
            Arc::new(PendingRegistry::default()),
        )
    }

    #[test]
    fn pending_tool_wait_stops_at_its_deadline() {
        let call = PendingCall::default();
        let started = Instant::now();
        assert_eq!(
            call.wait(|| false, std::time::Duration::from_millis(5))
                .unwrap(),
            PendingWait::Deadline
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(
            PendingCall::default()
                .wait(|| true, Duration::from_secs(1))
                .unwrap(),
            PendingWait::Cancelled
        );
    }

    #[test]
    fn broken_writer_marks_the_connection_and_rejects_more_frames() {
        let disconnected = Arc::new(AtomicBool::new(false));
        let writer = test_writer(BrokenWriter, Arc::clone(&disconnected));
        writer.write_message(&notification(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !disconnected.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(disconnected.load(Ordering::Acquire));
        assert!(writer.write_message(&notification(2)).is_err());
    }

    #[test]
    fn full_writer_queue_cancels_the_observed_execution_by_deadline() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let disconnected = Arc::new(AtomicBool::new(false));
        let writer = Arc::new(test_writer(
            BlockingWriter(Arc::clone(&gate)),
            Arc::clone(&disconnected),
        ));
        for sequence in 1..=OUTBOUND_QUEUE_DEPTH + 1 {
            let _ = writer.write_message_until(
                &notification(u64::try_from(sequence).unwrap()),
                Instant::now() + Duration::from_millis(50),
            );
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut observer = WireTaskObserver {
            execution_id: "exec-1".to_owned(),
            writer,
            sequence: Arc::new(AtomicU64::new(20)),
            clock: Arc::new(FixedClock(7)),
            cancelled: Arc::clone(&cancelled),
            disconnected,
            deadline: Instant::now() + Duration::from_millis(10),
            replayed: Arc::new(AtomicBool::new(false)),
            started: false,
        };
        observer.task_event(TaskEvent {
            sequence: 1,
            task_id: 1,
            owner_id: 0,
            kind: TaskEventKind::Spawned,
        });
        assert!(cancelled.load(Ordering::Acquire));
        let (released, ready) = &*gate;
        *released.lock().unwrap() = true;
        ready.notify_one();
    }

    #[test]
    fn injected_event_clock_and_sequence_commit_are_observable() {
        let output = CapturingWriter::default();
        let bytes = Arc::clone(&output.0);
        let disconnected = Arc::new(AtomicBool::new(false));
        let writer = Arc::new(test_writer(output, Arc::clone(&disconnected)));
        let sequence = Arc::new(AtomicU64::new(20));
        let mut observer = WireTaskObserver {
            execution_id: "exec-clock".to_owned(),
            writer,
            sequence: Arc::clone(&sequence),
            clock: Arc::new(FixedClock(7)),
            cancelled: Arc::new(AtomicBool::new(false)),
            disconnected,
            deadline: Instant::now() + Duration::from_secs(1),
            replayed: Arc::new(AtomicBool::new(false)),
            started: false,
        };
        observer.task_event(TaskEvent {
            sequence: 1,
            task_id: 2,
            owner_id: 0,
            kind: TaskEventKind::Spawned,
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while bytes.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let frame = bytes.lock().unwrap().clone();
        let message = FrameReader::new(frame.as_slice(), josh_protocol::DEFAULT_MAX_FRAME_BYTES)
            .read_message()
            .unwrap()
            .unwrap();
        let WireMessage::Notification { params, .. } = message else {
            panic!("missing event notification");
        };
        assert_eq!(params["elapsed_ms"], 7);
        assert_eq!(params["sequence"], 20);
        assert_eq!(sequence.load(Ordering::Relaxed), 21);

        bytes.lock().unwrap().clear();
        observer.budget_warning(BudgetWarning {
            resource: "instructions",
            used: 90,
            limit: 100,
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while bytes.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let frame = bytes.lock().unwrap().clone();
        let message = FrameReader::new(frame.as_slice(), josh_protocol::DEFAULT_MAX_FRAME_BYTES)
            .read_message()
            .unwrap()
            .unwrap();
        let WireMessage::Notification { params, .. } = message else {
            panic!("missing budget warning notification");
        };
        assert_eq!(params["kind"], "budget_warning");
        assert_eq!(params["elapsed_ms"], 7);
        assert_eq!(params["sequence"], 21);
        assert_eq!(params["fields"]["resource"], "instructions");
        assert_eq!(params["fields"]["used"], 90);
        assert_eq!(params["fields"]["limit"], 100);
        assert_eq!(sequence.load(Ordering::Relaxed), 22);
    }

    #[test]
    fn execution_events_mark_replayed_effect_provenance() {
        let output = CapturingWriter::default();
        let bytes = Arc::clone(&output.0);
        let disconnected = Arc::new(AtomicBool::new(false));
        let writer = Arc::new(test_writer(output, Arc::clone(&disconnected)));
        let mut observer = WireTaskObserver {
            execution_id: "exec-replay".to_owned(),
            writer,
            sequence: Arc::new(AtomicU64::new(3)),
            clock: Arc::new(FixedClock(7)),
            cancelled: Arc::new(AtomicBool::new(false)),
            disconnected,
            deadline: Instant::now() + Duration::from_secs(1),
            replayed: Arc::new(AtomicBool::new(false)),
            started: false,
        };

        observer.execution_effect_provenance(true);
        let deadline = Instant::now() + Duration::from_secs(1);
        while bytes.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let frame = bytes.lock().unwrap().clone();
        let message = FrameReader::new(frame.as_slice(), josh_protocol::DEFAULT_MAX_FRAME_BYTES)
            .read_message()
            .unwrap()
            .unwrap();
        let WireMessage::Notification { params, .. } = message else {
            panic!("missing replayed started notification");
        };
        assert_eq!(params["kind"], "started");
        assert_eq!(params["replayed"], true);

        bytes.lock().unwrap().clear();
        observer.task_event(TaskEvent {
            sequence: 1,
            task_id: 2,
            owner_id: 0,
            kind: TaskEventKind::Spawned,
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while bytes.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let frame = bytes.lock().unwrap().clone();
        let message = FrameReader::new(frame.as_slice(), josh_protocol::DEFAULT_MAX_FRAME_BYTES)
            .read_message()
            .unwrap()
            .unwrap();
        let WireMessage::Notification { params, .. } = message else {
            panic!("missing replayed task notification");
        };
        assert_eq!(params["kind"], "task_started");
        assert_eq!(params["replayed"], true);

        let live_output = CapturingWriter::default();
        let live_bytes = Arc::clone(&live_output.0);
        let live_disconnected = Arc::new(AtomicBool::new(false));
        let mut live = WireTaskObserver {
            execution_id: "exec-live".to_owned(),
            writer: Arc::new(test_writer(live_output, Arc::clone(&live_disconnected))),
            sequence: Arc::new(AtomicU64::new(3)),
            clock: Arc::new(FixedClock(7)),
            cancelled: Arc::new(AtomicBool::new(false)),
            disconnected: live_disconnected,
            deadline: Instant::now() + Duration::from_secs(1),
            replayed: Arc::new(AtomicBool::new(true)),
            started: false,
        };
        live.execution_effect_provenance(false);
        let deadline = Instant::now() + Duration::from_secs(1);
        while live_bytes.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let frame = live_bytes.lock().unwrap().clone();
        let message = FrameReader::new(frame.as_slice(), josh_protocol::DEFAULT_MAX_FRAME_BYTES)
            .read_message()
            .unwrap()
            .unwrap();
        let WireMessage::Notification { params, .. } = message else {
            panic!("missing live started notification");
        };
        assert_eq!(params["replayed"], false);
    }
}
