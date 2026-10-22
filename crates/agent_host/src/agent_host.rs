//! Runs an ACP agent server in a process of its own, so that the agent's turns
//! carry on when Zed quits and Zed can pick them up again when it reopens.
//!
//! Zed starts its own executable with `--agent-host <socket>`. That host
//! spawns the agent, owns its stdio, and relays it over the socket to whichever
//! Zed is connected. While no Zed is connected the host answers the agent's file
//! system requests itself and holds every other request (permission prompts,
//! questions) until a Zed reconnects and loads the session it belongs to. Once
//! no turn is running and no Zed is connected, the host stops the agent and
//! exits.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _,
    channel::{mpsc, oneshot},
    future::Either,
    io::{BufReader, WriteHalf},
    stream::BoxStream,
};
use net::async_net::{UnixListener, UnixStream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use smol::{LocalExecutor, Task, Timer};
use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use util::process::Child;

/// How long a host with no Zed connected and no turn running waits before it
/// stops the agent. Long enough to survive Zed being restarted.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const HOST_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const HOST_STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

// The host prefixes every line it sends to Zed with one of these tags.
const ATTACHED_TAG: char = 'a';
// Only sent by hosts from builds that served one connection at a time, which
// can still be running when a newer Zed connects.
const BUSY_TAG: char = 'b';
const STDOUT_TAG: char = 'o';
const STDERR_TAG: char = 'e';
const EXIT_TAG: char = 'x';

const SESSION_SETUP_METHODS: [&str; 4] = [
    "session/new",
    "session/load",
    "session/resume",
    "session/fork",
];
const PROMPT_METHOD: &str = "session/prompt";
const CANCEL_REQUEST_METHOD: &str = "$/cancel_request";

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Allows agents to be launched through a host process. Only the Zed binary
/// handles `--agent-host`, so test binaries and other embedders of the agent
/// crates must keep spawning agents directly.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, Option<String>)>,
    pub current_dir: Option<PathBuf>,
}

impl HostedCommand {
    pub fn from_std(command: &std::process::Command) -> Self {
        Self {
            program: command.get_program().to_string_lossy().into_owned(),
            args: command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
            env: command
                .get_envs()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.map(|value| value.to_string_lossy().into_owned()),
                    )
                })
                .collect(),
            current_dir: command.get_current_dir().map(Path::to_path_buf),
        }
    }

    fn socket_path(&self) -> PathBuf {
        // The environment is left out of the key: a value in it that differs
        // between launches must not keep a restarted Zed from finding the host
        // that is still running its sessions.
        let mut hasher = Sha256::new();
        hasher.update(self.program.as_bytes());
        for argument in &self.args {
            hasher.update([0]);
            hasher.update(argument.as_bytes());
        }
        hasher.update([0]);
        if let Some(current_dir) = &self.current_dir {
            hasher.update(current_dir.to_string_lossy().as_bytes());
        }
        let digest = hasher.finalize();
        let name: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        paths::temp_dir()
            .join("agent_hosts")
            .join(format!("{name}.sock"))
    }

    fn to_std(&self) -> std::process::Command {
        let mut command = util::command::new_std_command(&self.program);
        command.args(&self.args);
        for (key, value) in &self.env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        command
    }
}

#[derive(Serialize, Deserialize)]
struct Hello {
    command: HostedCommand,
}

pub enum HostMessage {
    Stdout(String),
    Stderr(String),
    Exited(Option<i32>),
}

pub struct HostConnection {
    pub messages: BoxStream<'static, io::Result<HostMessage>>,
    pub writer: WriteHalf<UnixStream>,
}

/// Connects to the host running `command`, starting one when none is running.
/// Returns `None` when that host is already serving another connection, since
/// a host relays a single Zed at a time.
pub async fn connect(command: &HostedCommand) -> Result<Option<HostConnection>> {
    let socket_path = command.socket_path();
    let stream = match UnixStream::connect(&socket_path).await {
        Ok(stream) => stream,
        Err(_) => {
            spawn_host(&socket_path)?;
            let started_at = Instant::now();
            loop {
                Timer::after(HOST_STARTUP_POLL_INTERVAL).await;
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => break stream,
                    Err(error) if started_at.elapsed() > HOST_STARTUP_TIMEOUT => {
                        return Err(error).context("agent host did not start listening");
                    }
                    Err(_) => {}
                }
            }
        }
    };

    let (reader, mut writer) = stream.split();
    let mut hello = serde_json::to_string(&Hello {
        command: command.clone(),
    })?;
    hello.push('\n');
    writer.write_all(hello.as_bytes()).await?;

    let mut lines = BufReader::new(reader).lines();
    let first_line = lines
        .next()
        .await
        .context("agent host closed the connection")??;
    match first_line.chars().next() {
        Some(ATTACHED_TAG) => {}
        Some(BUSY_TAG) => return Ok(None),
        _ => bail!("unexpected agent host greeting: {first_line}"),
    }

    let messages = lines
        .map(|line| {
            let line = line?;
            let mut characters = line.chars();
            let tag = characters.next();
            let payload = characters.as_str().to_string();
            match tag {
                Some(STDOUT_TAG) => Ok(HostMessage::Stdout(payload)),
                Some(STDERR_TAG) => Ok(HostMessage::Stderr(payload)),
                Some(EXIT_TAG) => Ok(HostMessage::Exited(payload.parse().ok())),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected agent host message: {line}"),
                )),
            }
        })
        .boxed();
    Ok(Some(HostConnection { messages, writer }))
}

fn spawn_host(socket_path: &Path) -> Result<()> {
    if let Some(directory) = socket_path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    let executable = std::env::current_exe().context("locating the Zed executable")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--agent-host")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    util::set_pre_exec_to_start_new_session(&mut command);

    #[cfg(windows)]
    let spawned = {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const ERROR_ACCESS_DENIED: i32 = 5;

        // Breaking away keeps the host alive when Zed runs inside a job that
        // kills its processes on close. A job that forbids breaking away fails
        // the spawn, and the host then shares Zed's fate as it would without it.
        command.creation_flags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        );
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
                command.spawn()
            }
            result => result,
        }
    };
    #[cfg(not(windows))]
    let spawned = command.spawn();

    let mut child = spawned.context("starting the agent host")?;
    // Reaps the host when it exits before Zed does.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// The entry point of `zed --agent-host <socket>`.
pub fn run(socket_path: &Path) -> Result<()> {
    let executor = Rc::new(LocalExecutor::new());
    smol::block_on(executor.run(serve(executor.clone(), socket_path.to_path_buf())))
}

async fn serve(executor: Rc<LocalExecutor<'static>>, socket_path: PathBuf) -> Result<()> {
    let Some(listener) = bind(&socket_path).await? else {
        return Ok(());
    };

    let (exit_sender, exit_receiver) = oneshot::channel();
    let host = Rc::new(RefCell::new(Host {
        exit: Some(exit_sender),
        ..Host::default()
    }));

    let _idle_watch = executor.spawn(watch_idle(host.clone()));
    let accept_loop = {
        let executor = executor.clone();
        let host = host.clone();
        async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let connection = handle_connection(executor.clone(), host.clone(), stream);
                        executor
                            .spawn(async move {
                                if let Err(error) = connection.await {
                                    log::error!("agent host connection failed: {error:#}");
                                }
                            })
                            .detach();
                    }
                    Err(error) => {
                        log::error!("agent host failed to accept a connection: {error}");
                        Timer::after(IDLE_CHECK_INTERVAL).await;
                    }
                }
            }
        }
    };
    futures::future::select(Box::pin(accept_loop), exit_receiver).await;

    let clients = std::mem::take(&mut host.borrow_mut().clients);
    let writers = clients.into_values().map(|client| {
        drop(client.sender);
        client.writer
    });
    futures::future::select(
        futures::future::join_all(writers),
        Timer::after(EXIT_FLUSH_TIMEOUT),
    )
    .await;
    std::fs::remove_file(&socket_path).ok();
    Ok(())
}

/// Binds the host socket, or returns `None` when another host is already
/// listening on it.
async fn bind(socket_path: &Path) -> Result<Option<UnixListener>> {
    if let Some(directory) = socket_path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    if let Ok(listener) = UnixListener::bind(socket_path) {
        return Ok(Some(listener));
    }
    if UnixStream::connect(socket_path).await.is_ok() {
        return Ok(None);
    }
    // Left behind by a host that did not get to remove it.
    match std::fs::remove_file(socket_path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(error)
                .with_context(|| format!("removing stale socket {}", socket_path.display()));
        }
        _ => {}
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    Ok(Some(listener))
}

#[derive(Default)]
struct Host {
    agent: Option<Agent>,
    /// Every connected Zed, by the generation it attached with. One Zed can
    /// connect more than once, from each of its projects on the same folder.
    clients: HashMap<u64, Client>,
    /// The connection that set up each session, which its updates and
    /// requests go to.
    session_owners: HashMap<String, u64>,
    next_generation: u64,
    next_request_id: u64,
    client_requests: HashMap<u64, ClientRequest>,
    agent_requests: Vec<AgentRequest>,
    exit: Option<oneshot::Sender<()>>,
}

struct Agent {
    stdin: mpsc::UnboundedSender<String>,
    kill: Option<oneshot::Sender<()>>,
}

struct Client {
    sender: mpsc::UnboundedSender<String>,
    writer: Task<()>,
}

/// A request from Zed that the agent has not answered yet. Its id is replaced
/// by one unique to the host, because a reconnected Zed numbers its requests
/// from the start again.
struct ClientRequest {
    generation: u64,
    id: Value,
    method: String,
    session_id: Option<String>,
}

/// A request from the agent that Zed has not answered yet.
struct AgentRequest {
    key: String,
    method: String,
    session_id: Option<String>,
    message: Value,
    delivered_to: Option<u64>,
}

async fn handle_connection(
    executor: Rc<LocalExecutor<'static>>,
    host: Rc<RefCell<Host>>,
    stream: UnixStream,
) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut lines = BufReader::new(reader).lines();
    let hello_line = lines
        .next()
        .await
        .context("connection closed before its hello")??;
    let hello: Hello = serde_json::from_str(&hello_line).context("parsing hello")?;

    if host.borrow().agent.is_none() {
        if let Err(error) = spawn_agent(&executor, &host, &hello.command) {
            let message = format!("{error:#}").replace(['\r', '\n'], " ");
            writer
                .write_all(
                    format!("{ATTACHED_TAG}\n{STDERR_TAG}{message}\n{EXIT_TAG}\n").as_bytes(),
                )
                .await?;
            host.borrow_mut().request_exit();
            return Ok(());
        }
    }

    let (sender, mut receiver) = mpsc::unbounded::<String>();
    let writer_task = executor.spawn(async move {
        while let Some(mut line) = receiver.next().await {
            line.push('\n');
            if let Err(error) = writer.write_all(line.as_bytes()).await {
                log::warn!("agent host failed to write to Zed: {error}");
                break;
            }
        }
    });
    let generation = host.borrow_mut().attach(sender, writer_task);

    while let Some(line) = lines.next().await {
        match line {
            Ok(line) => host.borrow_mut().handle_client_line(generation, line),
            Err(error) => {
                log::warn!("agent host failed to read from Zed: {error}");
                break;
            }
        }
    }
    host.borrow_mut().detach(generation);
    Ok(())
}

fn spawn_agent(
    executor: &Rc<LocalExecutor<'static>>,
    host: &Rc<RefCell<Host>>,
    command: &HostedCommand,
) -> Result<()> {
    let mut child = Child::spawn(
        command.to_std(),
        Stdio::piped(),
        Stdio::piped(),
        Stdio::piped(),
    )?;
    let mut stdin = child.stdin.take().context("taking the agent's stdin")?;
    let stdout = child.stdout.take().context("taking the agent's stdout")?;
    let stderr = child.stderr.take().context("taking the agent's stderr")?;

    let (stdin_sender, mut stdin_receiver) = mpsc::unbounded::<String>();
    executor
        .spawn(async move {
            while let Some(mut line) = stdin_receiver.next().await {
                line.push('\n');
                if let Err(error) = stdin.write_all(line.as_bytes()).await {
                    log::warn!("agent host failed to write to the agent: {error}");
                    break;
                }
            }
        })
        .detach();

    let stdout_task = executor.spawn({
        let host = host.clone();
        async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next().await {
                host.borrow_mut().handle_agent_line(line);
            }
        }
    });
    let stderr_task = executor.spawn({
        let host = host.clone();
        async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next().await {
                host.borrow().broadcast(STDERR_TAG, &line);
            }
        }
    });

    let (kill_sender, kill_receiver) = oneshot::channel::<()>();
    executor
        .spawn({
            let host = host.clone();
            async move {
                let status =
                    match futures::future::select(Box::pin(child.status()), kill_receiver).await {
                        Either::Left((status, _)) => status.ok(),
                        Either::Right(_) => {
                            child.kill().ok();
                            None
                        }
                    };
                // Descendants of the agent can hold its output open after it
                // exits, so its last lines are only waited for briefly.
                let drained = futures::future::join(stdout_task, stderr_task);
                futures::future::select(Box::pin(drained), Timer::after(EXIT_FLUSH_TIMEOUT)).await;
                let mut host = host.borrow_mut();
                let code = status.and_then(|status| status.code());
                host.broadcast(
                    EXIT_TAG,
                    &code.map(|code| code.to_string()).unwrap_or_default(),
                );
                host.request_exit();
            }
        })
        .detach();

    host.borrow_mut().agent = Some(Agent {
        stdin: stdin_sender,
        kill: Some(kill_sender),
    });
    Ok(())
}

async fn watch_idle(host: Rc<RefCell<Host>>) {
    let mut idle_since: Option<Instant> = None;
    loop {
        Timer::after(IDLE_CHECK_INTERVAL).await;
        if !host.borrow().is_idle() {
            idle_since = None;
            continue;
        }
        let idle_since = *idle_since.get_or_insert_with(Instant::now);
        if idle_since.elapsed() >= IDLE_TIMEOUT {
            let mut host = host.borrow_mut();
            match host.agent.as_mut().and_then(|agent| agent.kill.take()) {
                // The agent's exit then ends the host.
                Some(kill) => {
                    kill.send(()).ok();
                }
                None => host.request_exit(),
            }
            return;
        }
    }
}

impl Host {
    fn attach(&mut self, sender: mpsc::UnboundedSender<String>, writer: Task<()>) -> u64 {
        self.next_generation += 1;
        let generation = self.next_generation;
        sender.unbounded_send(ATTACHED_TAG.to_string()).ok();
        self.clients.insert(generation, Client { sender, writer });
        self.deliver_held_requests(generation, None);
        generation
    }

    fn detach(&mut self, generation: u64) {
        if self.clients.remove(&generation).is_none() {
            return;
        }
        self.session_owners.retain(|_, owner| *owner != generation);

        let mut answered_locally = Vec::new();
        self.agent_requests.retain_mut(|request| {
            if request.delivered_to != Some(generation) {
                return true;
            }
            if request.method.starts_with("fs/") {
                answered_locally.push(request.message.clone());
                return false;
            }
            request.delivered_to = None;
            true
        });
        for request in answered_locally {
            self.answer_locally(&request);
        }
        // Requests of no session can go to any Zed that is still connected.
        if let Some(generation) = self.target(None) {
            self.deliver_held_requests(generation, None);
        }
    }

    /// The connection that a message of `session_id` goes to: the one that set
    /// the session up, or for a message of no session the latest one.
    fn target(&self, session_id: Option<&str>) -> Option<u64> {
        match session_id {
            Some(session_id) => self
                .session_owners
                .get(session_id)
                .copied()
                .filter(|generation| self.clients.contains_key(generation)),
            None => self.clients.keys().max().copied(),
        }
    }

    fn is_idle(&self) -> bool {
        self.clients.is_empty()
            && !self
                .client_requests
                .values()
                .any(|request| request.method == PROMPT_METHOD)
    }

    fn request_exit(&mut self) {
        if let Some(exit) = self.exit.take() {
            exit.send(()).ok();
        }
    }

    fn send_to(&self, generation: u64, tag: char, line: &str) {
        if let Some(client) = self.clients.get(&generation) {
            client.sender.unbounded_send(format!("{tag}{line}")).ok();
        }
    }

    fn broadcast(&self, tag: char, line: &str) {
        for client in self.clients.values() {
            client.sender.unbounded_send(format!("{tag}{line}")).ok();
        }
    }

    fn send_to_agent(&self, line: String) {
        if let Some(agent) = &self.agent {
            agent.stdin.unbounded_send(line).ok();
        }
    }

    fn handle_client_line(&mut self, generation: u64, line: String) {
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else {
            self.send_to_agent(line);
            return;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned().filter(|id| !id.is_null());

        match (method, id) {
            (Some(method), Some(id)) => {
                let host_id = self.next_request_id;
                self.next_request_id += 1;
                let session_id = params_session_id(&message);
                // Loading a session replays it before the response, so its
                // updates must already go to the connection that asked.
                if SESSION_SETUP_METHODS.contains(&method.as_str())
                    && let Some(session_id) = &session_id
                {
                    self.session_owners.insert(session_id.clone(), generation);
                }
                self.client_requests.insert(
                    host_id,
                    ClientRequest {
                        generation,
                        id,
                        method,
                        session_id,
                    },
                );
                message["id"] = host_id.into();
            }
            (Some(method), None) if method == CANCEL_REQUEST_METHOD => {
                let request_id = message
                    .get("params")
                    .and_then(|params| params.get("requestId"))
                    .cloned();
                let host_id = self.client_requests.iter().find_map(|(host_id, request)| {
                    (request.generation == generation && Some(&request.id) == request_id.as_ref())
                        .then_some(*host_id)
                });
                // Passed on unmapped, the id could name another request of the
                // agent, since the host numbers the requests it forwards.
                let Some(host_id) = host_id else {
                    return;
                };
                message["params"]["requestId"] = host_id.into();
            }
            (None, Some(id)) => {
                let key = id.to_string();
                let Some(index) = self.agent_requests.iter().position(|request| {
                    request.key == key && request.delivered_to == Some(generation)
                }) else {
                    // The host answered this request itself after the Zed that
                    // was asked disconnected, or another Zed answered it.
                    return;
                };
                self.agent_requests.remove(index);
            }
            _ => {}
        }
        self.send_to_agent(message.to_string());
    }

    fn handle_agent_line(&mut self, line: String) {
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else {
            self.broadcast(STDOUT_TAG, &line);
            return;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned().filter(|id| !id.is_null());

        match (method, id) {
            (Some(method), Some(id)) => {
                let session_id = params_session_id(&message);
                let delivered_to = self.target(session_id.as_deref());
                match delivered_to {
                    Some(generation) => self.send_to(generation, STDOUT_TAG, &line),
                    // No Zed that knows the session is connected to answer it.
                    None if method.starts_with("fs/") => {
                        self.answer_locally(&message);
                        return;
                    }
                    None => {}
                }
                self.agent_requests.push(AgentRequest {
                    key: id.to_string(),
                    session_id,
                    method,
                    message,
                    delivered_to,
                });
            }
            (Some(method), None) if method == CANCEL_REQUEST_METHOD => {
                let key = message
                    .get("params")
                    .and_then(|params| params.get("requestId"))
                    .map(Value::to_string);
                let Some(index) = self
                    .agent_requests
                    .iter()
                    .position(|request| Some(&request.key) == key.as_ref())
                else {
                    return;
                };
                // A request Zed was never shown is dropped here; one it was
                // shown needs the cancellation to dismiss its prompt.
                match self.agent_requests[index].delivered_to {
                    Some(generation) => self.send_to(generation, STDOUT_TAG, &line),
                    None => {
                        self.agent_requests.remove(index);
                    }
                }
            }
            (None, Some(id)) => {
                let Some(request) = id
                    .as_u64()
                    .and_then(|host_id| self.client_requests.remove(&host_id))
                else {
                    return;
                };
                if !self.clients.contains_key(&request.generation) {
                    return;
                }
                let loaded_session = if SESSION_SETUP_METHODS.contains(&request.method.as_str())
                    && message.get("result").is_some()
                {
                    message
                        .get("result")
                        .and_then(|result| result.get("sessionId"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(request.session_id)
                } else {
                    None
                };
                message["id"] = request.id;
                self.send_to(request.generation, STDOUT_TAG, &message.to_string());
                // After the response, so that Zed knows the session by the time
                // the requests that waited for it arrive.
                if let Some(session_id) = loaded_session {
                    self.session_owners
                        .insert(session_id.clone(), request.generation);
                    self.deliver_held_requests(request.generation, Some(&session_id));
                }
            }
            _ => match params_session_id(&message)
                .as_deref()
                .and_then(|session_id| self.target(Some(session_id)))
            {
                Some(generation) => self.send_to(generation, STDOUT_TAG, &line),
                // A Zed ignores updates of a session it has not loaded.
                None => self.broadcast(STDOUT_TAG, &line),
            },
        }
    }

    /// Delivers to `generation` the held requests of `session_id`, or those
    /// that belong to no session when it is `None`.
    fn deliver_held_requests(&mut self, generation: u64, session_id: Option<&str>) {
        if !self.clients.contains_key(&generation) {
            return;
        }
        let mut lines = Vec::new();
        for request in &mut self.agent_requests {
            if request.delivered_to.is_none() && request.session_id.as_deref() == session_id {
                request.delivered_to = Some(generation);
                lines.push(request.message.to_string());
            }
        }
        for line in lines {
            self.send_to(generation, STDOUT_TAG, &line);
        }
    }

    fn answer_locally(&self, request: &Value) {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "fs/read_text_file" => read_text_file(&params),
            "fs/write_text_file" => write_text_file(&params),
            _ => Err(anyhow!("{method} is not available while Zed is closed")),
        };
        let response = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": format!("{error:#}") },
            }),
        };
        self.send_to_agent(response.to_string());
    }
}

fn params_session_id(message: &Value) -> Option<String> {
    message
        .get("params")
        .and_then(|params| params.get("sessionId"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn read_text_file(params: &Value) -> Result<Value> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .context("missing path")?;
    let content = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let line = params.get("line").and_then(Value::as_u64);
    let limit = params.get("limit").and_then(Value::as_u64);
    let content = if line.is_some() || limit.is_some() {
        let start = line.unwrap_or(1).saturating_sub(1) as usize;
        let limit = limit.map_or(usize::MAX, |limit| limit as usize);
        content
            .lines()
            .skip(start)
            .take(limit)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        content
    };
    Ok(json!({ "content": content }))
}

fn write_text_file(params: &Value) -> Result<Value> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .context("missing path")?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .context("missing content")?;
    if let Some(directory) = Path::new(path).parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {path}"))?;
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_host() -> (Host, mpsc::UnboundedReceiver<String>) {
        let (stdin, agent_receiver) = mpsc::unbounded();
        let host = Host {
            agent: Some(Agent { stdin, kill: None }),
            ..Host::default()
        };
        (host, agent_receiver)
    }

    fn attach(host: &mut Host) -> (u64, mpsc::UnboundedReceiver<String>) {
        let (sender, mut receiver) = mpsc::unbounded();
        let generation = host.attach(sender, smol::spawn(async {}));
        assert_eq!(next_line(&mut receiver), ATTACHED_TAG.to_string());
        (generation, receiver)
    }

    fn next_line(receiver: &mut mpsc::UnboundedReceiver<String>) -> String {
        receiver.try_recv().unwrap_or_default()
    }

    fn next_message(receiver: &mut mpsc::UnboundedReceiver<String>) -> Value {
        let line = next_line(receiver);
        let line = line.strip_prefix(STDOUT_TAG).unwrap_or(&line);
        serde_json::from_str(line).unwrap_or(Value::Null)
    }

    #[test]
    fn turn_survives_reconnect() {
        let directory = tempfile::tempdir().expect("create directory");
        let file_path = directory.path().join("file.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\n").expect("write file");

        let (mut host, mut agent) = test_host();
        let (first, mut first_client) = attach(&mut host);
        host.handle_client_line(
            first,
            json!({"jsonrpc": "2.0", "id": 0, "method": PROMPT_METHOD, "params": {"sessionId": "s"}})
                .to_string(),
        );
        assert_eq!(next_message(&mut agent)["id"], json!(0));

        host.detach(first);
        assert!(!host.is_idle(), "a running turn keeps the host alive");

        host.handle_agent_line(
            json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission", "params": {"sessionId": "s"}})
                .to_string(),
        );
        host.handle_agent_line(
            json!({"jsonrpc": "2.0", "id": 8, "method": "fs/read_text_file", "params": {"sessionId": "s", "path": file_path, "line": 2, "limit": 1}})
                .to_string(),
        );
        let read_response = next_message(&mut agent);
        assert_eq!(read_response["id"], json!(8));
        assert_eq!(read_response["result"]["content"], json!("two"));
        assert_eq!(next_line(&mut first_client), "");

        let (second, mut second_client) = attach(&mut host);
        assert_eq!(
            next_line(&mut second_client),
            "",
            "the permission request waits until its session is loaded"
        );
        host.handle_client_line(
            second,
            json!({"jsonrpc": "2.0", "id": 0, "method": "session/load", "params": {"sessionId": "s"}})
                .to_string(),
        );
        let load_request = next_message(&mut agent);
        assert_eq!(
            load_request["id"],
            json!(1),
            "a reused id must not reach the agent"
        );

        host.handle_agent_line(json!({"jsonrpc": "2.0", "id": 1, "result": {}}).to_string());
        assert_eq!(next_message(&mut second_client)["id"], json!(0));
        let permission_request = next_message(&mut second_client);
        assert_eq!(permission_request["id"], json!(7));
        assert_eq!(
            permission_request["method"],
            json!("session/request_permission")
        );

        host.handle_client_line(
            second,
            json!({"jsonrpc": "2.0", "id": 7, "result": {"outcome": {"outcome": "cancelled"}}})
                .to_string(),
        );
        assert_eq!(next_message(&mut agent)["id"], json!(7));

        host.handle_agent_line(json!({"jsonrpc": "2.0", "id": 0, "result": {}}).to_string());
        assert_eq!(
            next_line(&mut second_client),
            "",
            "the answer to the first Zed's prompt is not sent to the second"
        );
        host.detach(second);
        assert!(host.is_idle());
    }

    #[test]
    fn connections_share_the_agent() {
        let (mut host, mut agent) = test_host();
        let (first, mut first_client) = attach(&mut host);
        let (second, mut second_client) = attach(&mut host);

        host.handle_client_line(
            first,
            json!({"jsonrpc": "2.0", "id": 0, "method": "session/load", "params": {"sessionId": "a"}})
                .to_string(),
        );
        host.handle_client_line(
            second,
            json!({"jsonrpc": "2.0", "id": 0, "method": "session/load", "params": {"sessionId": "b"}})
                .to_string(),
        );
        assert_eq!(next_message(&mut agent)["id"], json!(0));
        assert_eq!(
            next_message(&mut agent)["id"],
            json!(1),
            "both connections numbering from zero must not collide"
        );

        host.handle_agent_line(
            json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "b"}})
                .to_string(),
        );
        assert_eq!(next_line(&mut first_client), "");
        assert_eq!(
            next_message(&mut second_client)["params"]["sessionId"],
            json!("b")
        );

        host.handle_agent_line(json!({"jsonrpc": "2.0", "id": 1, "result": {}}).to_string());
        assert_eq!(next_line(&mut first_client), "");
        assert_eq!(next_message(&mut second_client)["id"], json!(0));
        host.handle_agent_line(json!({"jsonrpc": "2.0", "id": 0, "result": {}}).to_string());
        assert_eq!(next_message(&mut first_client)["id"], json!(0));

        host.handle_agent_line(
            json!({"jsonrpc": "2.0", "id": 9, "method": "session/request_permission", "params": {"sessionId": "a"}})
                .to_string(),
        );
        assert_eq!(next_message(&mut first_client)["id"], json!(9));
        assert_eq!(next_line(&mut second_client), "");

        host.detach(first);
        assert!(!host.is_idle(), "the second connection is still attached");
        let (third, mut third_client) = attach(&mut host);
        assert_eq!(
            next_line(&mut third_client),
            "",
            "the permission request waits until its session is loaded again"
        );
        host.handle_client_line(
            third,
            json!({"jsonrpc": "2.0", "id": 0, "method": "session/load", "params": {"sessionId": "a"}})
                .to_string(),
        );
        assert_eq!(next_message(&mut agent)["id"], json!(2));
        host.handle_agent_line(json!({"jsonrpc": "2.0", "id": 2, "result": {}}).to_string());
        assert_eq!(next_message(&mut third_client)["id"], json!(0));
        assert_eq!(next_message(&mut third_client)["id"], json!(9));
        assert_eq!(next_line(&mut second_client), "");
    }
}
