mod debug_log;
mod runtime;
mod session_updates;
mod terminal_updates;
mod transport;
mod v1;
mod v2;

use debug_log::AcpDebugLog;
pub use debug_log::{AcpDebugMessage, AcpDebugMessageContent, AcpDebugMessageDirection};
use session_updates::{InboundSessionNotification, InboundSessionUpdate, PendingSessionUpdates};
pub use terminal_updates::{DecodedTerminalNotification, DecodedTerminalUpdate};
#[cfg(any(test, feature = "test-support"))]
pub use v1::test_support;
pub use v1::{AcpConnection, AcpSessionList, GEMINI_TERMINAL_AUTH_METHOD_ID};
pub use v2::AcpV2Connection;

use acp_thread::{AcpThread, AuthRequired, ElicitationStore, LoadError};
use agent_client_protocol::schema::{
    MaybeUndefined,
    v1::{self as acp, ErrorCode},
    v2 as acp_v2,
};
use agent_client_protocol::{
    Agent, Client, HandleDispatchFrom, Handled, JsonRpcMessage, JsonRpcResponse, Responder,
    UntypedMessage, V2Builder, V2ConnectionTo,
};
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use feature_flags::{AcpBetaFeatureFlag, FeatureFlagAppExt as _};
use futures::{Future, channel::mpsc};
use gpui::{App, AsyncApp, Entity, SharedString, Subscription, Task, WeakEntity};
use project::agent_server_store::{AgentServerCommand, AgentServerStore};
use project::context_server_store::ContextServerConfiguration;
use project::{AgentId, Project};
use std::{cell::RefCell, path::PathBuf, process::ExitStatus, rc::Rc, sync::Arc};
use task::SpawnInTerminal;
use thiserror::Error;
use util::ResultExt as _;
use util::path_list::PathList;

const EXIT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

fn mcp_configurations_for_project(
    project: &Entity<Project>,
    cx: &App,
) -> Vec<(String, Arc<ContextServerConfiguration>)> {
    let store = project.read(cx).context_server_store().read(cx);
    let is_local = project.read(cx).is_local();
    store
        .configured_server_ids()
        .iter()
        .filter_map(|id| {
            let configuration = store.configuration_for_server(id)?;
            match &*configuration {
                ContextServerConfiguration::Custom { remote, .. }
                | ContextServerConfiguration::Extension { remote, .. }
                    if !is_local && !remote =>
                {
                    None
                }
                _ => Some((id.0.to_string(), configuration)),
            }
        })
        .collect()
}

fn work_dirs_from_session_info(cwd: PathBuf, additional_directories: Vec<PathBuf>) -> PathList {
    let mut seen_paths = HashSet::default();
    let mut paths = Vec::with_capacity(1 + additional_directories.len());

    seen_paths.insert(cwd.clone());
    paths.push(cwd);

    for path in additional_directories {
        if seen_paths.insert(path.clone()) {
            paths.push(path);
        }
    }

    PathList::new(&paths)
}

async fn exited_load_error_after_drain(
    status: ExitStatus,
    drained: impl Future<Output = ()>,
    debug_log: &AcpDebugLog,
    cx: &AsyncApp,
) -> LoadError {
    // Descendants can keep stdout or stderr open after the direct child exits.
    let timeout = cx.background_executor().timer(EXIT_DRAIN_TIMEOUT);
    futures::pin_mut!(drained, timeout);
    if let futures::future::Either::Right(_) = futures::future::select(drained, timeout).await {
        log::warn!("Timed out draining ACP output after agent exit");
    }

    LoadError::Exited {
        status,
        stderr: debug_log.trailing_stderr().map(SharedString::from),
    }
}

#[derive(Debug, Error)]
#[error("Unsupported version")]
pub struct UnsupportedVersion;

#[derive(Debug, Error)]
#[error("ACP v1 fallback failed: {failure}; v2 negotiation: {negotiation:#}")]
struct V1FallbackFailure {
    failure: String,
    negotiation: anyhow::Error,
}

fn fallback_failure(failure: anyhow::Error, negotiation: anyhow::Error) -> anyhow::Error {
    let context = V1FallbackFailure {
        failure: format!("{failure:#}"),
        negotiation,
    };
    // The final attempt's typed error must remain authoritative for UI error classification.
    failure.context(context)
}

pub async fn connect(
    agent_id: AgentId,
    project: Entity<Project>,
    command: AgentServerCommand,
    agent_server_store: WeakEntity<AgentServerStore>,
    default_mode: Option<acp_v2::SessionConfigValueId>,
    default_config_options: HashMap<String, settings::AgentConfigOptionValue>,
    cx: &mut AsyncApp,
) -> Result<Rc<dyn acp_thread::AgentConnection>> {
    let negotiation_error = if cx.update(|cx| cx.has_flag::<AcpBetaFeatureFlag>()) {
        match AcpV2Connection::stdio(
            agent_id.clone(),
            project.clone(),
            command.clone(),
            agent_server_store.clone(),
            cx,
        )
        .await
        {
            Ok(connection) => return Ok(Rc::new(connection)),
            Err(error) if error.is::<UnsupportedVersion>() => Some(error),
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    v1::connect(
        agent_id,
        project,
        command,
        agent_server_store,
        default_mode,
        default_config_options,
        cx,
    )
    .await
    .map_err(|error| match negotiation_error {
        Some(negotiation) => fallback_failure(error, negotiation),
        None => error,
    })
}

fn v1_only_mismatch(error: &agent_client_protocol::Error) -> bool {
    if error.code != agent_client_protocol::Error::new(-32022, "").code {
        return false;
    }
    let Some(data) = error.data.as_ref() else {
        return false;
    };
    let Some(supported) = data.get("supported").and_then(serde_json::Value::as_array) else {
        return false;
    };
    data.get("requested").and_then(serde_json::Value::as_u64) == Some(2)
        && supported
            .iter()
            .all(|value| value.as_u64().is_some_and(|version| version > 0))
        && supported.iter().any(|value| value.as_u64() == Some(1))
        && !supported.iter().any(|value| value.as_u64() == Some(2))
}

fn initialization_error(error: anyhow::Error, debug_log: &AcpDebugLog) -> anyhow::Error {
    match debug_log.initialization_response() {
        Some(Ok(Some(value))) => {
            if serde_json::from_value::<acp::InitializeResponse>(value).is_ok_and(|response| {
                response.protocol_version == agent_client_protocol::schema::ProtocolVersion::V1
            }) {
                error.context(UnsupportedVersion)
            } else {
                error
            }
        }
        Some(Err(protocol_error)) => {
            let retry_v1 = v1_only_mismatch(&protocol_error);
            let error = if error.is::<agent_client_protocol::Error>() {
                error
            } else {
                anyhow!(protocol_error).context(format!("ACP initialization failed: {error}"))
            };
            if retry_v1 {
                error.context(UnsupportedVersion)
            } else {
                error
            }
        }
        _ => error,
    }
}

/// Helper for flattening the nested `Result` shapes that come out of
/// `entity.update(cx, |_, cx| fallible_op(cx))` into a single `Result<T,
/// acp::Error>`.
///
/// `anyhow::Error` values get converted via `acp::Error::from`, which
/// downcasts an `acp::Error` back out of `anyhow` when present, so typed
/// errors like auth-required survive the trip.
trait FlattenAcpResult<T> {
    fn flatten_acp(self) -> Result<T, acp::Error>;
}

impl<T> FlattenAcpResult<T> for Result<Result<T, anyhow::Error>, anyhow::Error> {
    fn flatten_acp(self) -> Result<T, acp::Error> {
        match self {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err.into()),
            Err(err) => Err(err.into()),
        }
    }
}

impl<T> FlattenAcpResult<T> for Result<Result<T, acp::Error>, anyhow::Error> {
    fn flatten_acp(self) -> Result<T, acp::Error> {
        match self {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err),
            Err(err) => Err(err.into()),
        }
    }
}

/// Holds state needed by foreground work dispatched from background handler closures.
struct ClientContext {
    sessions: Rc<RefCell<HashMap<acp_v2::SessionId, AcpSession>>>,
    pending_session_updates: Option<Rc<RefCell<PendingSessionUpdates>>>,
    session_list: Rc<RefCell<Option<Rc<AcpSessionList>>>>,
    request_elicitations: Entity<ElicitationStore>,
    // Legacy and ingress-only contexts do not track caller ownership of outgoing RPCs.
    active_request_scopes: Option<RefCell<HashSet<acp_v2::RequestId>>>,
    subagents: Rc<RefCell<HashMap<acp_v2::SessionId, SubagentSession>>>,
}

/// A session that the agent runs a subagent in, as its `subagent_update`s
/// report it. Zed shows its tool calls in the session it runs under, under the
/// tool call that delegated to it, as it does for an agent that reports
/// subagents only through `_meta.claudeCode.parentToolUseId`.
struct SubagentSession {
    parent_session_id: acp_v2::SessionId,
    /// The tool call whose work the subagent does now: the one that launched
    /// it, or the last one that sent it a message and so resumed it.
    tool_call_id: Option<acp_v2::ToolCallId>,
    running: bool,
    /// Whether the agent stops the subagent alone when it gets the
    /// subagent's session in `session/cancel`.
    cancellable: bool,
    /// The parent's display title for the subagent, from its announcement.
    title: Option<String>,
    /// The parent's description of the subagent's role.
    description: Option<String>,
    /// The tool calls that Zed shows for the messages that launched or resumed
    /// the subagent, for an agent that reports no tool call of its own for them.
    synthesized_tool_calls: HashSet<acp_v2::ToolCallId>,
}

fn dispatch_queue_closed_error() -> acp::Error {
    acp::Error::internal_error().data("ACP foreground dispatch queue closed")
}

/// Work items sent from `Send` handler closures to the `!Send` foreground thread.
trait ForegroundWorkItem: Send {
    fn run(self: Box<Self>, cx: &mut AsyncApp, ctx: &ClientContext);
    fn reject(self: Box<Self>);
}

type ForegroundWork = Box<dyn ForegroundWorkItem>;

struct ForegroundBarrier {
    acknowledgment: futures::channel::oneshot::Sender<()>,
}

impl ForegroundWorkItem for ForegroundBarrier {
    fn run(self: Box<Self>, _cx: &mut AsyncApp, _context: &ClientContext) {
        if self.acknowledgment.send(()).is_err() {
            log::debug!("ACP foreground barrier was cancelled before acknowledgment");
        }
    }

    fn reject(self: Box<Self>) {
        log::debug!("ACP foreground dispatch queue closed before acknowledgment");
    }
}

struct ClientDisconnectWork {
    acknowledgment: futures::channel::oneshot::Sender<()>,
}

impl ForegroundWorkItem for ClientDisconnectWork {
    fn run(self: Box<Self>, cx: &mut AsyncApp, context: &ClientContext) {
        if let Some(scopes) = &context.active_request_scopes {
            scopes.borrow_mut().clear();
        }
        if let Some(pending) = &context.pending_session_updates {
            pending.borrow_mut().clear();
        }
        let threads = context
            .sessions
            .borrow()
            .values()
            .map(|session| session.thread.clone())
            .collect::<Vec<_>>();
        for thread in threads {
            if let Some(thread) = thread.upgrade() {
                thread.update(cx, |thread, cx| {
                    thread.cancel_outstanding_elicitations(cx);
                    thread.handle_connection_lost(cx);
                    cx.notify();
                });
            }
        }
        context.request_elicitations.update(cx, |store, cx| {
            store.cancel_all(cx);
        });
        if self.acknowledgment.send(()).is_err() {
            log::debug!("ACP disconnect cleanup waiter was cancelled before acknowledgment");
        }
    }

    fn reject(self: Box<Self>) {
        log::debug!("ACP foreground dispatch queue closed before disconnect cleanup");
    }
}

async fn settle_after_disconnect(
    dispatch_tx: mpsc::UnboundedSender<ForegroundWork>,
) -> Result<(), acp::Error> {
    let (acknowledgment, received) = futures::channel::oneshot::channel();
    let work: ForegroundWork = Box::new(ClientDisconnectWork { acknowledgment });
    if let Err(error) = dispatch_tx.unbounded_send(work) {
        error.into_inner().reject();
        return Err(dispatch_queue_closed_error());
    }
    received.await.map_err(|_| dispatch_queue_closed_error())
}

async fn drain_foreground_queue(dispatch_tx: &mpsc::UnboundedSender<ForegroundWork>) -> Result<()> {
    // The SDK response can precede application of the old session's queued replay.
    let (acknowledgment, received) = futures::channel::oneshot::channel();
    let barrier: ForegroundWork = Box::new(ForegroundBarrier { acknowledgment });
    if let Err(error) = dispatch_tx.unbounded_send(barrier) {
        error.into_inner().reject();
        return Err(anyhow!(dispatch_queue_closed_error()));
    }
    received
        .await
        .context("ACP foreground dispatch queue closed")
}

struct RequestForegroundWork<Req, Res>
where
    Req: Send + 'static,
    Res: JsonRpcResponse + Send + 'static,
{
    request: Req,
    responder: Responder<Res>,
    handler: fn(Req, Responder<Res>, &mut AsyncApp, &ClientContext),
}

impl<Req, Res> ForegroundWorkItem for RequestForegroundWork<Req, Res>
where
    Req: Send + 'static,
    Res: JsonRpcResponse + Send + 'static,
{
    fn run(self: Box<Self>, cx: &mut AsyncApp, ctx: &ClientContext) {
        let Self {
            request,
            responder,
            handler,
        } = *self;
        handler(request, responder, cx, ctx);
    }

    fn reject(self: Box<Self>) {
        let Self { responder, .. } = *self;
        log::error!("ACP foreground dispatch queue closed while handling inbound request");
        responder
            .respond_with_error(dispatch_queue_closed_error())
            .log_err();
    }
}

struct NotificationForegroundWork<Notif>
where
    Notif: Send + 'static,
{
    notification: Notif,
    handler: fn(Notif, &mut AsyncApp, &ClientContext),
}

impl<Notif> ForegroundWorkItem for NotificationForegroundWork<Notif>
where
    Notif: Send + 'static,
{
    fn run(self: Box<Self>, cx: &mut AsyncApp, ctx: &ClientContext) {
        let Self {
            notification,
            handler,
        } = *self;
        handler(notification, cx, ctx);
    }

    fn reject(self: Box<Self>) {
        log::error!("ACP foreground dispatch queue closed while handling inbound notification");
    }
}

fn enqueue_request<Req, Res>(
    dispatch_tx: &mpsc::UnboundedSender<ForegroundWork>,
    request: Req,
    responder: Responder<Res>,
    handler: fn(Req, Responder<Res>, &mut AsyncApp, &ClientContext),
) where
    Req: Send + 'static,
    Res: JsonRpcResponse + Send + 'static,
{
    let work: ForegroundWork = Box::new(RequestForegroundWork {
        request,
        responder,
        handler,
    });
    if let Err(err) = dispatch_tx.unbounded_send(work) {
        err.into_inner().reject();
    }
}

fn enqueue_notification<Notif>(
    dispatch_tx: &mpsc::UnboundedSender<ForegroundWork>,
    notification: Notif,
    handler: fn(Notif, &mut AsyncApp, &ClientContext),
) where
    Notif: Send + 'static,
{
    let work: ForegroundWork = Box::new(NotificationForegroundWork {
        notification,
        handler,
    });
    if let Err(err) = dispatch_tx.unbounded_send(work) {
        err.into_inner().reject();
    }
}

#[derive(Clone)]
struct ConfigOptions {
    config_options: Rc<RefCell<Vec<acp_v2::SessionConfigOption>>>,
    tx: Rc<RefCell<watch::Sender<()>>>,
    rx: watch::Receiver<()>,
}

impl ConfigOptions {
    fn new(config_options: Rc<RefCell<Vec<acp_v2::SessionConfigOption>>>) -> Self {
        let (tx, rx) = watch::channel(());
        Self {
            config_options,
            tx: Rc::new(RefCell::new(tx)),
            rx,
        }
    }
}

pub struct AcpSession {
    thread: WeakEntity<AcpThread>,
    suppress_abort_err: bool,
    session_modes: Option<Rc<RefCell<acp::SessionModeState>>>,
    config_options: Option<ConfigOptions>,
    default_config_task: Option<Task<()>>,
    _release_subscription: Subscription,
}

/// Builds a v2 terminal-notification handler without selecting it for application connections.
/// Other session updates remain unclaimed so later inbound handlers can process them.
pub fn v2_terminal_client_builder(
    mut dispatch: impl FnMut(DecodedTerminalNotification) -> Result<(), agent_client_protocol::Error>
    + Send
    + 'static,
) -> V2Builder<Client, impl HandleDispatchFrom<Agent>> {
    with_v2_terminal_notifications(Client.v2(), false, move |notification, _| {
        dispatch(notification)
    })
}

struct NotificationByteCounter(usize);

impl std::io::Write for NotificationByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn notification_size(
    notification: &impl serde::Serialize,
) -> Result<usize, agent_client_protocol::Error> {
    let mut counter = NotificationByteCounter(0);
    serde_json::to_writer(&mut counter, notification).map_err(|_| {
        agent_client_protocol::Error::internal_error().data("Failed to size ACP session update")
    })?;
    Ok(counter.0)
}

fn with_v2_terminal_notifications<Handler: HandleDispatchFrom<Agent>>(
    builder: V2Builder<Client, Handler>,
    measure_payload: bool,
    mut dispatch: impl FnMut(
        DecodedTerminalNotification,
        usize,
    ) -> Result<(), agent_client_protocol::Error>
    + Send
    + 'static,
) -> V2Builder<Client, impl HandleDispatchFrom<Agent>> {
    builder.on_receive_notification(
        async move |message: UntypedMessage, connection: V2ConnectionTo<Agent>| {
            let kind = message
                .params()
                .get("update")
                .and_then(|update| update.get("sessionUpdate"))
                .and_then(serde_json::Value::as_str);
            if !acp_v2::UpdateSessionNotification::matches_method(message.method())
                || !matches!(kind, Some("terminal_update" | "terminal_output_chunk"))
            {
                return Ok(Handled::No {
                    message: (message, connection),
                    retry: false,
                });
            }
            let notification = terminal_updates::parse_terminal_notification(&message)?;
            let encoded_bytes = if measure_payload {
                notification_size(&notification)?
            } else {
                0
            };
            let decoded = terminal_updates::decode_terminal_notification(notification)
                .map_err(|error| {
                    agent_client_protocol::Error::invalid_params().data(format!("{error:#}"))
                })?
                .ok_or_else(|| {
                    agent_client_protocol::Error::internal_error()
                        .data("Terminal notification was not decoded")
                })?;
            dispatch(decoded, encoded_bytes)?;
            Ok(Handled::Yes)
        },
        agent_client_protocol::on_receive_notification!(),
    )
}

fn with_v2_session_notifications<Handler: HandleDispatchFrom<Agent>>(
    builder: V2Builder<Client, Handler>,
    mut dispatch: impl FnMut(
        InboundSessionNotification,
        usize,
    ) -> Result<(), agent_client_protocol::Error>
    + Send
    + 'static,
) -> V2Builder<Client, impl HandleDispatchFrom<Agent>> {
    builder.on_receive_notification(
        async move |notification: acp_v2::UpdateSessionNotification,
                    connection: V2ConnectionTo<Agent>| {
            let encoded_bytes = notification_size(&notification)?;
            let notification = match InboundSessionNotification::try_from(notification) {
                Ok(notification) => notification,
                Err(notification) => {
                    return Ok(Handled::No {
                        message: (notification, connection),
                        retry: false,
                    });
                }
            };
            dispatch(notification, encoded_bytes)?;
            Ok(Handled::Yes)
        },
        agent_client_protocol::on_receive_notification!(),
    )
}

fn with_v2_client_ingress<Handler: HandleDispatchFrom<Agent>>(
    builder: V2Builder<Client, Handler>,
    dispatch_sender: mpsc::UnboundedSender<ForegroundWork>,
) -> V2Builder<Client, impl HandleDispatchFrom<Agent>> {
    let dispatch = {
        let dispatch_sender = dispatch_sender.clone();
        move |notification, encoded_bytes| {
            enqueue_notification(
                &dispatch_sender,
                (notification, encoded_bytes),
                route_v2_session_notification,
            );
            Ok(())
        }
    };
    let builder = with_v2_terminal_notifications(builder, true, {
        let dispatch = dispatch.clone();
        move |notification, encoded_bytes| dispatch(notification.into(), encoded_bytes)
    });
    with_v2_session_notifications(builder, dispatch)
        .on_receive_request(
            {
                let dispatch_sender = dispatch_sender.clone();
                async move |request: acp_v2::RequestPermissionRequest, responder, _connection| {
                    enqueue_request(
                        &dispatch_sender,
                        request,
                        responder,
                        handle_v2_request_permission,
                    );
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let dispatch_sender = dispatch_sender.clone();
                async move |request: acp_v2::CreateElicitationRequest, responder, _connection| {
                    enqueue_request(
                        &dispatch_sender,
                        request,
                        responder,
                        handle_v2_create_elicitation,
                    );
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let dispatch_sender = dispatch_sender.clone();
                async move |notification: acp_v2::CompleteElicitationNotification, _connection| {
                    enqueue_notification(
                        &dispatch_sender,
                        notification,
                        handle_v2_complete_elicitation,
                    );
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

fn terminal_auth_task_id(agent_id: &AgentId, method_id: &acp_v2::AuthMethodId) -> String {
    format!("external-agent-{}-{}-login", agent_id.0, method_id.0)
}

fn terminal_auth_task(
    command: &AgentServerCommand,
    agent_id: &AgentId,
    method: &acp_v2::AuthMethodTerminal,
) -> SpawnInTerminal {
    acp_thread::build_terminal_auth_task(
        terminal_auth_task_id(agent_id, &method.method_id),
        method.name.clone(),
        command.path.to_string_lossy().into_owned(),
        command.args.clone(),
        command.env.clone().unwrap_or_default(),
    )
}

fn first_class_terminal_auth_task(
    agent_id: AgentId,
    store: WeakEntity<AgentServerStore>,
    terminal: acp_v2::AuthMethodTerminal,
    cx: &App,
) -> Task<Result<SpawnInTerminal>> {
    cx.spawn(async move |cx| {
        let command = store
            .update(cx, |store, cx| {
                let agent = store
                    .get_external_agent(&agent_id)
                    .context("Agent server not found")?;
                let environment = terminal
                    .env
                    .iter()
                    .map(|variable| (variable.name.clone(), variable.value.clone()))
                    .collect();
                anyhow::Ok(agent.get_command(
                    terminal.args.clone(),
                    environment,
                    &mut cx.to_async(),
                ))
            })?
            .context("Failed to get agent command")?
            .await?;
        Ok(terminal_auth_task(&command, &agent_id, &terminal))
    })
}

fn map_acp_error(err: acp::Error) -> anyhow::Error {
    let error = anyhow!(err);
    match AuthRequired::from_error(&error) {
        Some(auth_required) => error.context(auth_required),
        None => error,
    }
}

fn session_thread(
    ctx: &ClientContext,
    session_id: &acp_v2::SessionId,
) -> Result<WeakEntity<AcpThread>, acp::Error> {
    let session_id = root_session_id(ctx, session_id);
    let sessions = ctx.sessions.borrow();
    sessions
        .get(&session_id)
        .map(|session| session.thread.clone())
        .ok_or_else(|| acp::Error::internal_error().data(format!("unknown session: {session_id}")))
}

/// The session that `session_id` runs a subagent of, following nested
/// subagents up, or `session_id` itself when it is no subagent's.
fn root_session_id(ctx: &ClientContext, session_id: &acp_v2::SessionId) -> acp_v2::SessionId {
    root_subagent_session(&ctx.subagents.borrow(), session_id)
}

fn root_subagent_session(
    subagents: &HashMap<acp_v2::SessionId, SubagentSession>,
    session_id: &acp_v2::SessionId,
) -> acp_v2::SessionId {
    let mut session_id = session_id;
    // Bounded, so that a parent cycle in what the agent sent cannot hang Zed.
    for _ in 0..subagents.len() {
        match subagents.get(session_id) {
            Some(subagent) => session_id = &subagent.parent_session_id,
            None => break,
        }
    }
    session_id.clone()
}

/// The session of the running subagent that does the work of `tool_call_id`
/// in `session_id`, when the agent stops it on its own.
fn cancellable_subagent(
    subagents: &HashMap<acp_v2::SessionId, SubagentSession>,
    session_id: &acp_v2::SessionId,
    tool_call_id: &acp_v2::ToolCallId,
) -> Option<acp_v2::SessionId> {
    subagents
        .iter()
        .find(|(subagent_id, subagent)| {
            subagent.running
                && subagent.cancellable
                && subagent.tool_call_id.as_ref() == Some(tool_call_id)
                && root_subagent_session(subagents, subagent_id) == *session_id
        })
        .map(|(subagent_id, _)| subagent_id.clone())
}

/// Records a `subagent_update` that `parent_session_id` sent about
/// `subagent_id`. `None` leaves what is known unchanged.
fn record_subagent_update(
    ctx: &ClientContext,
    parent_session_id: acp_v2::SessionId,
    subagent_id: acp_v2::SessionId,
    display: SubagentDisplay,
    cancellable: Option<bool>,
    running: Option<bool>,
    cx: &mut AsyncApp,
) {
    if subagent_id == parent_session_id {
        return;
    }
    let tool_call_id = {
        let mut subagents = ctx.subagents.borrow_mut();
        let subagent = subagents
            .entry(subagent_id.clone())
            .or_insert_with(|| SubagentSession {
                parent_session_id,
                tool_call_id: None,
                running: false,
                cancellable: false,
                title: None,
                description: None,
                synthesized_tool_calls: HashSet::default(),
            });
        display.title.update_to(&mut subagent.title);
        display.description.update_to(&mut subagent.description);
        if let Some(cancellable) = cancellable {
            subagent.cancellable = cancellable;
        }
        if let Some(running) = running {
            subagent.running = running;
        }
        subagent.tool_call_id.clone()
    };
    if let (Some(tool_call_id), Some(running)) = (tool_call_id, running) {
        set_subagent_running(ctx, &subagent_id, tool_call_id, running, cx);
    }
}

/// The display metadata of a `subagent_update`, as patches: undefined leaves
/// the stored value, `null` clears it.
#[derive(Default)]
struct SubagentDisplay {
    title: MaybeUndefined<String>,
    description: MaybeUndefined<String>,
}

/// A `session_message` in `session_id`'s transcript, sent by
/// `sender_session_id` to `recipient_session_id`.
///
/// In a subagent's transcript, the message that launched or resumed it, whose
/// id is the tool call that sent it, unless the agent did not know that call.
/// In the parent's transcript, the same message, for which Zed shows a tool
/// call when the agent reports none (Claude Code reports a subagent only as a
/// session to a client that supports subagents), or the subagent's reply,
/// which that tool call then shows.
fn handle_session_message(
    ctx: &ClientContext,
    session_id: &acp_v2::SessionId,
    message_id: &str,
    sender_session_id: Option<&acp_v2::SessionId>,
    recipient_session_id: Option<&acp_v2::SessionId>,
    text: Option<String>,
    cx: &mut AsyncApp,
) {
    if message_id.starts_with(&format!("{session_id}:")) {
        return;
    }
    let is_subagent_of_this_session = |other: Option<&acp_v2::SessionId>| {
        other.is_some_and(|other| {
            other != session_id
                && ctx
                    .subagents
                    .borrow()
                    .get(other)
                    .is_some_and(|subagent| &subagent.parent_session_id == session_id)
        })
    };
    let sent_here = sender_session_id.is_none_or(|sender| sender == session_id);
    if ctx.subagents.borrow().contains_key(session_id) {
        if sent_here {
            // The subagent's own message to its parent; the parent's copy shows it.
            return;
        }
        link_subagent_tool_call(ctx, session_id, acp_v2::ToolCallId::new(message_id), cx);
    } else if sent_here && is_subagent_of_this_session(recipient_session_id) {
        if let Some(subagent_id) = recipient_session_id {
            show_subagent_launch(
                ctx,
                subagent_id,
                acp_v2::ToolCallId::new(message_id),
                text,
                cx,
            );
        }
    } else if recipient_session_id.is_none_or(|recipient| recipient == session_id)
        && is_subagent_of_this_session(sender_session_id)
        && let (Some(subagent_id), Some(text)) = (sender_session_id, text)
    {
        show_subagent_reply(ctx, subagent_id, text, cx);
    }
}

/// Shows a tool call for the message `tool_call_id` that launched or resumed
/// the subagent of `subagent_id`, unless the agent reported one.
fn show_subagent_launch(
    ctx: &ClientContext,
    subagent_id: &acp_v2::SessionId,
    tool_call_id: acp_v2::ToolCallId,
    prompt: Option<String>,
    cx: &mut AsyncApp,
) {
    let Ok(thread) = session_thread(ctx, subagent_id) else {
        return;
    };
    let Some((title, description, running)) =
        ctx.subagents.borrow().get(subagent_id).map(|subagent| {
            (
                subagent.title.clone(),
                subagent.description.clone(),
                subagent.running,
            )
        })
    else {
        return;
    };
    let reported = thread
        .read_with(cx, |thread, _| thread.tool_call(&tool_call_id).is_some())
        .unwrap_or(true);
    if reported {
        return;
    }
    let title = title
        .or_else(|| description.clone())
        .unwrap_or_else(|| "Subagent".to_owned());
    let mut raw_input = serde_json::Map::new();
    if let Some(description) = description {
        raw_input.insert("subagent_type".into(), description.into());
    }
    if let Some(prompt) = prompt {
        raw_input.insert("prompt".into(), prompt.into());
    }
    let update = acp_v2::ToolCallUpdate::new(tool_call_id.clone())
        .name("Agent".to_owned())
        .title(title)
        .kind(acp_v2::ToolKind::Think)
        .status(subagent_tool_call_status(running))
        .raw_input(serde_json::Value::Object(raw_input))
        .meta(acp_v2::Meta::from_iter([(
            "claudeCode".to_owned(),
            serde_json::json!({ "toolName": "Agent" }),
        )]));
    let shown = thread
        .update(cx, |thread, cx| thread.upsert_local_tool_call(update, cx))
        .and_then(|result| result);
    if shown.log_err().is_none() {
        return;
    }
    if let Some(subagent) = ctx.subagents.borrow_mut().get_mut(subagent_id) {
        subagent.synthesized_tool_calls.insert(tool_call_id);
    }
}

/// Shows the reply of the subagent of `subagent_id` in the tool call that Zed
/// shows for its work.
fn show_subagent_reply(
    ctx: &ClientContext,
    subagent_id: &acp_v2::SessionId,
    reply: String,
    cx: &mut AsyncApp,
) {
    let tool_call_id = ctx
        .subagents
        .borrow()
        .get(subagent_id)
        .and_then(|subagent| {
            subagent
                .tool_call_id
                .clone()
                .filter(|tool_call_id| subagent.synthesized_tool_calls.contains(tool_call_id))
        });
    let (Some(tool_call_id), Ok(thread)) = (tool_call_id, session_thread(ctx, subagent_id)) else {
        return;
    };
    let update = acp_v2::ToolCallUpdate::new(tool_call_id).content(vec![reply.into()]);
    thread
        .update(cx, |thread, cx| thread.upsert_local_tool_call(update, cx))
        .and_then(|result| result)
        .log_err();
}

fn subagent_tool_call_status(running: bool) -> acp_v2::ToolCallStatus {
    if running {
        acp_v2::ToolCallStatus::InProgress
    } else {
        acp_v2::ToolCallStatus::Completed
    }
}

/// The text of a message's text blocks.
fn message_text<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let text = texts.into_iter().collect::<Vec<_>>().join("\n\n");
    (!text.is_empty()).then_some(text)
}

/// Makes `tool_call_id` the tool call whose work the subagent of
/// `subagent_id` does, in place of the one it did before.
fn link_subagent_tool_call(
    ctx: &ClientContext,
    subagent_id: &acp_v2::SessionId,
    tool_call_id: acp_v2::ToolCallId,
    cx: &mut AsyncApp,
) {
    let (previous_tool_call_id, running) = {
        let mut subagents = ctx.subagents.borrow_mut();
        let Some(subagent) = subagents.get_mut(subagent_id) else {
            return;
        };
        if subagent.tool_call_id.as_ref() == Some(&tool_call_id) {
            return;
        }
        (
            subagent.tool_call_id.replace(tool_call_id.clone()),
            subagent.running,
        )
    };
    if let Some(previous_tool_call_id) = previous_tool_call_id {
        set_subagent_running(ctx, subagent_id, previous_tool_call_id, false, cx);
    }
    if running {
        set_subagent_running(ctx, subagent_id, tool_call_id, true, cx);
    }
}

fn set_subagent_running(
    ctx: &ClientContext,
    subagent_id: &acp_v2::SessionId,
    tool_call_id: acp_v2::ToolCallId,
    running: bool,
    cx: &mut AsyncApp,
) {
    let Ok(thread) = session_thread(ctx, subagent_id) else {
        return;
    };
    let synthesized = ctx
        .subagents
        .borrow()
        .get(subagent_id)
        .is_some_and(|subagent| subagent.synthesized_tool_calls.contains(&tool_call_id));
    thread
        .update(cx, |thread, cx| {
            if synthesized {
                let update = acp_v2::ToolCallUpdate::new(tool_call_id.clone())
                    .status(subagent_tool_call_status(running));
                thread.upsert_local_tool_call(update, cx).log_err();
            }
            thread.set_subagent_running(tool_call_id, running, cx)
        })
        .ok();
}

fn subagent_parent_tool_use_id(meta: &acp_v2::Meta) -> Option<&str> {
    meta.get("claudeCode")?.get("parentToolUseId")?.as_str()
}

/// Shows an update of a v2 subagent session in the session it runs under.
/// Only its tool calls are shown, under the tool call that delegated to it, as
/// Claude Code tags them in `_meta.claudeCode.parentToolUseId`; the subagent's
/// own messages are not the conversation's.
fn handle_v2_subagent_session_notification(
    mut notification: InboundSessionNotification,
    subagent_id: acp_v2::SessionId,
    cx: &mut AsyncApp,
    context: &ClientContext,
) {
    let thread = match session_thread(context, &subagent_id) {
        Ok(thread) => thread,
        Err(error) => {
            log::warn!("Received ACP v2 subagent update for an unknown session: {error}");
            return;
        }
    };
    match &mut notification.update {
        InboundSessionUpdate::ToolCallUpdate(update) => {
            let reported_parent = update
                .meta
                .value()
                .and_then(subagent_parent_tool_use_id)
                .map(acp_v2::ToolCallId::new);
            if let Some(reported_parent) = reported_parent {
                let unlinked = context
                    .subagents
                    .borrow()
                    .get(&subagent_id)
                    .is_some_and(|subagent| subagent.tool_call_id.is_none());
                if unlinked {
                    link_subagent_tool_call(context, &subagent_id, reported_parent, cx);
                }
            } else {
                let parent = context
                    .subagents
                    .borrow()
                    .get(&subagent_id)
                    .and_then(|subagent| subagent.tool_call_id.clone());
                if let Some(parent) = parent {
                    tag_subagent_tool_call(update, parent, &thread, cx);
                }
            }
        }
        InboundSessionUpdate::ToolCallContentChunk(_) | InboundSessionUpdate::Terminal(_) => {}
        _ => return,
    }
    notification.session_id = root_session_id(context, &subagent_id);
    apply_v2_session_notification(notification, &thread, &context.sessions, cx).log_err();
}

/// Tags `update` with the tool call whose subagent made it. Metadata patches
/// replace a top-level key whole, so the tag joins the `claudeCode` object the
/// update sends, or else the one the tool call already has.
fn tag_subagent_tool_call(
    update: &mut acp_v2::ToolCallUpdate,
    parent: acp_v2::ToolCallId,
    thread: &WeakEntity<AcpThread>,
    cx: &mut AsyncApp,
) {
    let claude_code = match update.meta.value() {
        Some(meta) if meta.contains_key("claudeCode") => meta.get("claudeCode").cloned(),
        // `null` clears the metadata, and a tag would keep the rest of it.
        _ if update.meta.is_null() => return,
        _ => {
            let stored = thread
                .read_with(cx, |thread, _| {
                    thread
                        .tool_call(&update.tool_call_id)
                        .and_then(|(_, call)| call.meta.as_ref()?.get("claudeCode").cloned())
                })
                .ok()
                .flatten();
            if stored
                .as_ref()
                .and_then(|claude_code| claude_code.get("parentToolUseId"))
                .is_some()
            {
                return;
            }
            stored
        }
    };
    let mut claude_code = match claude_code {
        Some(serde_json::Value::Object(claude_code)) => claude_code,
        _ => serde_json::Map::new(),
    };
    claude_code.insert("parentToolUseId".into(), parent.0.to_string().into());
    let mut meta = update.meta.value().cloned().unwrap_or_default();
    meta.insert("claudeCode".into(), claude_code.into());
    update.meta = MaybeUndefined::Value(meta);
}

fn route_v2_session_notification(
    (notification, encoded_bytes): (InboundSessionNotification, usize),
    cx: &mut AsyncApp,
    context: &ClientContext,
) {
    let registered = context
        .sessions
        .borrow()
        .contains_key(&notification.session_id)
        || context
            .subagents
            .borrow()
            .contains_key(&notification.session_id);
    let notification = if !registered && let Some(pending) = &context.pending_session_updates {
        match pending.borrow_mut().stage(notification, encoded_bytes) {
            None => return,
            Some(notification) => notification,
        }
    } else {
        notification
    };
    handle_v2_session_notification(notification, cx, context);
}

fn close_undelivered_session(
    (session_id, connection): (acp_v2::SessionId, V2ConnectionTo<Agent>),
    _cx: &mut AsyncApp,
    context: &ClientContext,
) {
    if let Some(pending) = &context.pending_session_updates {
        pending.borrow_mut().discard_session(&session_id);
    }
    if context.sessions.borrow().contains_key(&session_id) {
        return;
    }
    connection
        .prepare_request(acp_v2::CloseSessionRequest::new(session_id))
        .on_receiving_result(async move |result| {
            result.log_err();
            Ok(())
        })
        .log_err();
}

fn apply_v2_session_notification(
    notification: InboundSessionNotification,
    thread: &WeakEntity<AcpThread>,
    sessions: &RefCell<HashMap<acp_v2::SessionId, AcpSession>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    if let InboundSessionUpdate::ConfigOptions(update) = &notification.update {
        let config_options = {
            let mut sessions = sessions.borrow_mut();
            let session = sessions
                .get_mut(&notification.session_id)
                .filter(|session| session.thread.entity_id() == thread.entity_id())
                .context("Session was released or replaced before applying configuration")?;
            session
                .config_options
                .get_or_insert_with(|| ConfigOptions::new(Rc::new(RefCell::new(Vec::new()))))
                .clone()
        };
        *config_options.config_options.borrow_mut() = update.config_options.clone();
        config_options.tx.borrow_mut().send(()).log_err();
    }
    thread.update(cx, |thread, cx| notification.apply(thread, cx))?
}

fn handle_v2_session_notification(
    notification: InboundSessionNotification,
    cx: &mut AsyncApp,
    context: &ClientContext,
) {
    match &notification.update {
        InboundSessionUpdate::SubagentUpdate(update) => {
            // A state of `null` restores an ancestor of a resumed subagent,
            // which says nothing about whether it works.
            let running = update.state.value().map(|state| {
                matches!(
                    state,
                    acp_v2::StateUpdate::Running(_) | acp_v2::StateUpdate::RequiresAction(_)
                )
            });
            let cancellable = match &update.capabilities {
                MaybeUndefined::Undefined => None,
                MaybeUndefined::Null => Some(false),
                MaybeUndefined::Value(capabilities) => Some(capabilities.cancel.is_some()),
            };
            let display = SubagentDisplay {
                title: update.title.clone(),
                description: update.description.clone(),
            };
            record_subagent_update(
                context,
                notification.session_id.clone(),
                update.session_id.clone(),
                display,
                cancellable,
                running,
                cx,
            );
            return;
        }
        InboundSessionUpdate::SessionMessage(message) => {
            let text = match &message.content {
                MaybeUndefined::Value(content) => {
                    message_text(content.iter().filter_map(|block| match block {
                        acp_v2::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    }))
                }
                _ => None,
            };
            handle_session_message(
                context,
                &notification.session_id,
                &message.message_id.0,
                message.sender_session_id.as_ref(),
                message.recipient_session_id.as_ref(),
                text,
                cx,
            );
            return;
        }
        InboundSessionUpdate::SessionMessageChunk => return,
        _ => {}
    }
    if context
        .subagents
        .borrow()
        .contains_key(&notification.session_id)
    {
        let subagent_id = notification.session_id.clone();
        handle_v2_subagent_session_notification(notification, subagent_id, cx, context);
        return;
    }
    let thread = match session_thread(context, &notification.session_id) {
        Ok(thread) => thread,
        Err(error) => {
            log::warn!("Received ACP v2 update for an unknown session: {error}");
            return;
        }
    };
    let Some(thread) = thread.upgrade() else {
        log::warn!("Received ACP v2 update for a released session");
        return;
    };
    if let InboundSessionUpdate::SessionInfo(update) = &notification.update
        && let Some(session_list) = context.session_list.borrow().as_ref()
    {
        session_list.send_info_update(notification.session_id.clone(), update.clone());
    }
    apply_v2_session_notification(notification, &thread.downgrade(), &context.sessions, cx)
        .log_err();
}

fn respond_err<T: JsonRpcResponse>(responder: Responder<T>, err: acp::Error) {
    // Log the actual error we're returning — otherwise agents that hit an
    // error path (e.g. unknown session) would see only the generic internal
    // error returned over the wire with no trace of why on the client side.
    log::warn!(
        "Responding to ACP request `{method}` with error: {err:?}",
        method = responder.method()
    );
    responder.respond_with_error(err).log_err();
}

fn respond_result<T: JsonRpcResponse>(responder: Responder<T>, result: Result<T, acp::Error>) {
    match result {
        Ok(response) => {
            responder.respond(response).log_err();
        }
        Err(err) => respond_err(responder, err),
    }
}

fn handle_v2_request_permission(
    mut request: acp_v2::RequestPermissionRequest,
    responder: Responder<acp_v2::RequestPermissionResponse>,
    cx: &mut AsyncApp,
    context: &ClientContext,
) {
    let cancellation = responder.cancellation();
    if cancellation.is_cancelled() {
        return respond_err(responder, acp::Error::request_cancelled());
    }
    // A subagent asks in the session it runs under, where its tool calls show.
    request.session_id = root_session_id(context, &request.session_id);
    let thread = match session_thread(context, &request.session_id) {
        Ok(thread) => thread,
        Err(error) => return respond_err(responder, error),
    };
    let (request_id, task) = match thread
        .update(cx, |thread, cx| {
            thread
                .request_permission(request, cx)
                .map_err(|error| acp::Error::invalid_params().data(error.to_string()))
        })
        .flatten_acp()
    {
        Ok(request) => request,
        Err(error) => return respond_err(responder, error),
    };
    cx.spawn(async move |cx| {
        match cancellation
            .run_until_cancelled(async { Ok(task.await) })
            .await
        {
            Ok(outcome) => {
                responder
                    .respond(acp_v2::RequestPermissionResponse::new(outcome))
                    .log_err();
            }
            Err(error) => {
                if error.code == ErrorCode::RequestCancelled {
                    thread
                        .update(cx, |thread, cx| {
                            thread.cancel_permission_request(request_id, cx)
                        })
                        .log_err();
                }
                respond_err(responder, error);
            }
        }
    })
    .detach();
}

fn handle_v2_create_elicitation(
    args: acp_v2::CreateElicitationRequest,
    responder: Responder<acp_v2::CreateElicitationResponse>,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let cancellation = responder.cancellation();
    if cancellation.is_cancelled() {
        return respond_err(responder, acp::Error::request_cancelled());
    }
    match args.scope() {
        acp_v2::ElicitationScope::Session(scope) => {
            let thread = match session_thread(ctx, &scope.session_id) {
                Ok(t) => t,
                Err(e) => return respond_err(responder, e),
            };

            let (elicitation_id, task) = match thread
                .update(cx, |thread, cx| {
                    thread
                        .request_elicitation_with_id(args, cx)
                        .map_err(acp_thread::elicitation::error_to_v1)
                })
                .flatten_acp()
            {
                Ok(task) => task,
                Err(e) => return respond_err(responder, e),
            };

            cx.spawn(async move |cx| {
                let result: Result<_, acp::Error> = cancellation
                    .run_until_cancelled(async { Ok(task.await) })
                    .await;

                match result {
                    Ok(response) => {
                        responder.respond(response).log_err();
                    }
                    Err(e) => {
                        if e.code == ErrorCode::RequestCancelled {
                            thread
                                .update(cx, |thread, cx| {
                                    thread.cancel_elicitation(&elicitation_id, cx)
                                })
                                .log_err();
                        }
                        respond_err(responder, e);
                    }
                }
            })
            .detach();
        }
        acp_v2::ElicitationScope::Request(scope) => {
            if let Some(scopes) = &ctx.active_request_scopes
                && !scopes.borrow().contains(&scope.request_id)
            {
                return respond_err(
                    responder,
                    acp::Error::invalid_params()
                        .data("Elicitation parent request is not owned by an active caller"),
                );
            }
            let store = ctx.request_elicitations.clone();
            let (elicitation_id, task) = match store.update(cx, |store, cx| {
                store
                    .request_elicitation_with_id(args, cx)
                    .map_err(acp_thread::elicitation::error_to_v1)
            }) {
                Ok(task) => task,
                Err(e) => return respond_err(responder, e),
            };
            let store = store.downgrade();

            cx.spawn(async move |cx| {
                let result: Result<_, acp::Error> = cancellation
                    .run_until_cancelled(async { Ok(task.await) })
                    .await;

                match result {
                    Ok(response) => {
                        responder.respond(response).log_err();
                    }
                    Err(e) => {
                        if e.code == ErrorCode::RequestCancelled {
                            store
                                .update(cx, |store, cx| {
                                    store.cancel_elicitation(&elicitation_id, cx)
                                })
                                .log_err();
                        }
                        respond_err(responder, e);
                    }
                }
            })
            .detach();
        }
        _ => {
            respond_err(
                responder,
                acp::Error::invalid_params().data("unknown elicitation scope"),
            );
        }
    }
}

fn handle_v2_complete_elicitation(
    args: acp_v2::CompleteElicitationNotification,
    cx: &mut AsyncApp,
    ctx: &ClientContext,
) {
    let threads = ctx
        .sessions
        .borrow()
        .values()
        .map(|session| session.thread.clone())
        .collect::<Vec<_>>();
    let elicitation_id = args.elicitation_id;

    // Complete accepted notifications in queue order before disconnect cleanup can run.
    for thread in threads {
        if let Some(thread) = thread.upgrade() {
            thread.update(cx, |thread, cx| {
                thread.complete_url_elicitation(&elicitation_id, cx);
            });
        }
    }
    ctx.request_elicitations.update(cx, |store, cx| {
        store.complete_url_elicitation(&elicitation_id, cx);
    })
}

#[cfg(all(test, unix))]
mod selection_tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use serde_json::json;

    fn set_beta(enabled: bool, cx: &mut App) {
        cx.update_flags(
            false,
            if enabled {
                vec!["acp-beta".into()]
            } else {
                vec![]
            },
        );
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .feature_flags
                    .get_or_insert_default()
                    .insert("acp-beta".into(), if enabled { "on" } else { "off" }.into());
            });
        });
        assert_eq!(cx.has_flag::<AcpBetaFeatureFlag>(), enabled);
    }

    #[gpui::test]
    async fn beta_selection_uses_fresh_v1_fallback_only_for_version_mismatch(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let mut store = settings::SettingsStore::test(cx);
            store.register_setting::<feature_flags::FeatureFlagsSettings>();
            cx.set_global(store);
            release_channel::init("0.0.0".parse().expect("version"), cx);
        });
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("working directory");
        let project = Project::example([directory.path()], &mut cx.to_async()).await;
        let v1 = r#""result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"selected-v1","version":"1"}}"#;
        let v2 = r#""result":{"protocolVersion":2,"info":{"name":"selected-v2","version":"1"},"capabilities":{"session":{}}}"#;
        for (beta, response, exit_after_response, expected, requests) in [
            (false, v2, false, Some(false), "1"),
            (true, v2, false, Some(true), "2"),
            (true, v1, false, Some(false), "2 1"),
            (
                true,
                r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"requested":2,"supported":[1]}}"#,
                false,
                Some(false),
                "2 1",
            ),
            (
                true,
                r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"requested":2,"supported":[1]}}"#,
                true,
                Some(false),
                "2 1",
            ),
            (
                true,
                r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"requested":2,"supported":[3]}}"#,
                true,
                None,
                "2",
            ),
            (
                true,
                r#""error":{"code":-32022,"message":"Unsupported protocol version","data":{"requested":2,"supported":[1,2]}}"#,
                false,
                None,
                "2",
            ),
            (
                true,
                r#""error":{"code":-32602,"message":"Bad initialize parameters"}"#,
                false,
                None,
                "2",
            ),
            (
                true,
                r#""result":{"protocolVersion":1},"error":{"code":-32022,"message":"Invalid envelope","data":{"requested":2,"supported":[1]}}"#,
                true,
                None,
                "2",
            ),
            (
                true,
                r#""result":{"info":{"name":"malformed","version":"1"}}"#,
                false,
                None,
                "2",
            ),
            (
                true,
                r#""result":{"protocolVersion":"malformed","agentCapabilities":{}}"#,
                false,
                None,
                "2",
            ),
            (true, "", true, None, "2"),
        ] {
            cx.update(|cx| {
                set_beta(beta, cx);
            });
            let trace = directory.path().join("connections");
            std::fs::write(&trace, "").expect("clear trace");
            let command = AgentServerCommand {
                path: "/bin/sh".into(),
                args: vec!["-c".into(), r#"
printf 'start %s\n' "$$" >> "$ACP_SELECTION_TRACE"
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*)
            id=${request#*\"id\":}; id=${id%%,*}
            case "$request" in
                *'"protocolVersion":2'*)
                    printf 'initialize 2\n' >> "$ACP_SELECTION_TRACE"
                    if [ -n "$ACP_SELECTION_RESPONSE" ]; then
                        printf '{"jsonrpc":"2.0","id":%s,%s}\n' "$id" "$ACP_SELECTION_RESPONSE"
                    fi
                    if [ "$ACP_SELECTION_EXIT" = 1 ]; then exit 7; fi
                    ;;
                *)
                    printf 'initialize 1\n' >> "$ACP_SELECTION_TRACE"
                    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"selected-v1","version":"1"}}}\n' "$id"
                    ;;
            esac
            ;;
    esac
done
"#.into()],
                env: Some(HashMap::from_iter([
                    ("ACP_SELECTION_TRACE".into(), trace.to_string_lossy().into_owned()),
                    ("ACP_SELECTION_RESPONSE".into(), response.into()),
                    ("ACP_SELECTION_EXIT".into(), if exit_after_response { "1" } else { "0" }.into()),
                ])),
            };
            let connecting = cx.spawn({
                let project = project.clone();
                async move |mut cx| {
                    connect(
                        AgentId::new("selection"),
                        project,
                        command,
                        WeakEntity::new_invalid(),
                        None,
                        HashMap::default(),
                        &mut cx,
                    )
                    .await
                }
            });
            let timeout = cx
                .background_executor
                .timer(std::time::Duration::from_secs(10));
            futures::pin_mut!(connecting, timeout);
            let result = match futures::future::select(connecting, timeout).await {
                futures::future::Either::Left((result, _)) => result,
                _ => panic!("selection timed out: {response}"),
            };
            if let Some(v2) = expected {
                let connection = result.expect("selected connection");
                assert_eq!(
                    connection.clone().downcast::<AcpV2Connection>().is_some(),
                    v2
                );
                cx.update(|cx| set_beta(false, cx));
                assert_eq!(
                    connection.clone().downcast::<AcpV2Connection>().is_some(),
                    v2
                );
                connection.shutdown();
                drop(connection);
            } else {
                let error = result
                    .err()
                    .unwrap_or_else(|| panic!("unexpected successful selection for {response}"));
                if response.starts_with(r#""error":{"code":-32022"#) {
                    let protocol_error = error
                        .downcast_ref::<agent_client_protocol::Error>()
                        .expect("structured mismatch retained");
                    assert_eq!(
                        protocol_error.code,
                        agent_client_protocol::Error::new(-32022, "").code
                    );
                    assert!(protocol_error.data.is_some());
                }
            }
            let trace = std::fs::read_to_string(&trace).expect("connection trace");
            let actual = trace
                .lines()
                .filter_map(|line| line.strip_prefix("initialize "))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(actual, requests, "{response}");
            let processes = trace
                .lines()
                .filter_map(|line| line.strip_prefix("start "))
                .collect::<Vec<_>>();
            assert_eq!(processes.len(), requests.split_whitespace().count());
            if let [first, second] = processes.as_slice() {
                assert_ne!(first, second, "fallback must launch a fresh process");
            }
        }
    }

    #[test]
    fn version_mismatch_requires_valid_v1_only_support() {
        for data in [
            json!({"requested": 1, "supported": [1]}),
            json!({"requested": 2, "supported": [1, 2]}),
            json!({"requested": 2, "supported": [0, 1]}),
            json!({"requested": 2, "supported": [1, "2"]}),
            json!({"requested": 2}),
        ] {
            assert!(!v1_only_mismatch(
                &agent_client_protocol::Error::new(-32022, "").data(data)
            ));
        }
        assert!(!v1_only_mismatch(
            &agent_client_protocol::Error::invalid_params()
                .data(json!({"requested": 2, "supported": [1]}))
        ));
    }

    #[test]
    fn fallback_failure_keeps_the_final_error_authoritative() {
        let negotiation_data = json!({"requested": 2, "supported": [1]});
        let final_data = json!({"attempt": "v1"});
        let error = fallback_failure(
            anyhow!(agent_client_protocol::Error::invalid_params().data(final_data.clone())),
            anyhow!(
                agent_client_protocol::Error::new(-32022, "Unsupported")
                    .data(negotiation_data.clone())
            )
            .context(UnsupportedVersion),
        );
        let info = acp_thread::ProtocolErrorInfo::from_error(&error).expect("final protocol error");
        assert_eq!(info.code, -32602);
        assert_eq!(info.data, Some(&final_data));
        assert!(!error.is::<UnsupportedVersion>());
        let context = error
            .downcast_ref::<V1FallbackFailure>()
            .expect("negotiation context");
        assert_eq!(
            context
                .negotiation
                .downcast_ref::<agent_client_protocol::Error>()
                .expect("original negotiation")
                .data,
            Some(negotiation_data)
        );
    }

    #[gpui::test]
    #[cfg_attr(not(feature = "e2e"), ignore)]
    async fn claude_adapter_initialize_smoke(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = settings::SettingsStore::test(cx);
            store.register_setting::<feature_flags::FeatureFlagsSettings>();
            cx.set_global(store);
            release_channel::init("0.0.0".parse().expect("version"), cx);
        });
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("isolated working directory");
        let config = directory.path().join("claude-config");
        std::fs::create_dir(&config).expect("isolated Claude configuration");
        let project = Project::example([directory.path()], &mut cx.to_async()).await;
        for (beta, adapter_v2, expected_v2) in [
            (false, true, false),
            (true, true, true),
            (true, false, false),
        ] {
            cx.update(|cx| set_beta(beta, cx));
            let command = AgentServerCommand {
                path: "npx".into(),
                args: vec![
                    "--yes".into(),
                    "@agentclientprotocol/claude-agent-acp@0.94.0".into(),
                ],
                env: Some(HashMap::from_iter([
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        config.to_string_lossy().into_owned(),
                    ),
                    (
                        "CLAUDE_AGENT_ACP_EXPERIMENTAL_V2".into(),
                        if adapter_v2 { "1" } else { "0" }.into(),
                    ),
                    ("ANTHROPIC_API_KEY".into(), "".into()),
                    ("CLAUDE_CODE_OAUTH_TOKEN".into(), "".into()),
                ])),
            };
            let connecting = cx.spawn({
                let project = project.clone();
                async move |mut cx| {
                    connect(
                        AgentId::new("claude-acp"),
                        project,
                        command,
                        WeakEntity::new_invalid(),
                        None,
                        HashMap::default(),
                        &mut cx,
                    )
                    .await
                }
            });
            let timeout = cx
                .background_executor
                .timer(std::time::Duration::from_secs(30));
            futures::pin_mut!(connecting, timeout);
            let connection = match futures::future::select(connecting, timeout).await {
                futures::future::Either::Left((result, _)) => {
                    result.expect("Claude adapter handshake")
                }
                _ => panic!("Claude adapter handshake timed out"),
            };
            let actual_v2 = connection.clone().downcast::<AcpV2Connection>().is_some();
            assert_eq!(
                actual_v2, expected_v2,
                "beta={beta} adapter_v2={adapter_v2}"
            );
            println!(
                "Claude ACP 0.94.0: beta={beta}, adapter_v2={adapter_v2}, selected_v2={actual_v2}"
            );
            connection.shutdown();
            drop(connection);
            cx.run_until_parked();
        }
    }
}
