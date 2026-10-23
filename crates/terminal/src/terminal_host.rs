//! Runs a terminal's shell in a process of its own, so that the shell keeps
//! running when Zed quits and Zed can show it again when it reopens.
//!
//! Zed starts its own executable with `--terminal-host <socket>` for each such
//! terminal. The host owns the PTY and the shell in it, and keeps an emulator
//! of its own fed with everything the shell writes. A Zed that connects is sent
//! that emulator's screen as escape sequences, then every byte the shell writes
//! from then on. When the shell exits, or Zed asks the host to kill it, the host
//! exits too.

use std::{
    borrow::Cow,
    cell::RefCell,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    rc::Rc,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashSet;
use futures::{
    AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _,
    channel::mpsc::{self, UnboundedReceiver, UnboundedSender},
    io::{ReadHalf, WriteHalf},
};
use gpui::{BackgroundExecutor, Task};
use net::async_net::{UnixListener, UnixStream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use smol::{LocalExecutor, Timer};
use vte::ansi::{Processor, StdSyncHandler};

use crate::{
    PtyEvent, TerminalBackendEvent, TerminalBounds,
    alacritty::{
        AlacrittyTermLock, HostClient, HostOutput, HostPty, HostPtyOptions, PtyCommand,
        replay_snapshot,
    },
};

const HOST_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const HOST_STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long a host waits for the Zed that started it to ask for its shell.
const SPAWN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Guards against a corrupt length allocating without bound.
const MAX_FRAME_LENGTH: usize = 256 * 1024 * 1024;

// Every frame is a tag, a little-endian `u32` payload length, and the payload.
// Sent by Zed:
const HELLO_TAG: u8 = b'h';
const INPUT_TAG: u8 = b'i';
const RESIZE_TAG: u8 = b'r';
const KILL_TAG: u8 = b'k';
// Sent by the host:
const ATTACHED_TAG: u8 = b'a';
const SNAPSHOT_TAG: u8 = b's';
const OUTPUT_TAG: u8 = b'o';
const EXITED_TAG: u8 = b'x';
const ERROR_TAG: u8 = b'e';

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Sessions this process started or attached to. A terminal's session is saved
/// some time after its host starts, so these may not be saved yet.
static OWN_SESSIONS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Lets interactive local terminals run their shells in a host process. Only
/// the Zed binary handles `--terminal-host`, so test binaries and other
/// embedders of the terminal crate must keep their PTYs in process.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScreenSize {
    pub(crate) columns: usize,
    pub(crate) lines: usize,
    pub(crate) cell_width: u16,
    pub(crate) cell_height: u16,
}

impl ScreenSize {
    pub(crate) fn from_bounds(bounds: TerminalBounds) -> Self {
        Self {
            columns: bounds.num_columns(),
            lines: bounds.num_lines(),
            cell_width: f32::from(bounds.cell_width()) as u16,
            cell_height: f32::from(bounds.line_height()) as u16,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SpawnRequest {
    pub(crate) program: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) escape_args: bool,
    pub(crate) scrolling_history: usize,
    pub(crate) size: ScreenSize,
}

#[derive(Serialize, Deserialize)]
enum Hello {
    Spawn(SpawnRequest),
    Attach,
}

#[derive(Serialize, Deserialize)]
struct Attached {
    pid: u32,
}

pub(crate) enum Session {
    Spawn(SpawnRequest),
    Attach(String),
}

/// A connection to a terminal host whose greeting has been read.
pub(crate) struct HostConnection {
    pub(crate) session_id: String,
    pub(crate) pid: u32,
    reader: ReadHalf<UnixStream>,
    writer: WriteHalf<UnixStream>,
}

fn hosts_directory() -> PathBuf {
    paths::temp_dir().join("terminal_hosts")
}

fn socket_path(session_id: &str) -> PathBuf {
    hosts_directory().join(format!("{session_id}.sock"))
}

/// Starts a host for a new shell, or connects to the host of an existing one.
pub(crate) async fn connect(session: Session) -> Result<HostConnection> {
    let (session_id, hello, stream) = match session {
        Session::Spawn(request) => {
            let session_id = uuid::Uuid::new_v4().simple().to_string();
            OWN_SESSIONS.lock().insert(session_id.clone());
            let socket_path = socket_path(&session_id);
            spawn_host(&socket_path)?;
            let started_at = Instant::now();
            let stream = loop {
                Timer::after(HOST_STARTUP_POLL_INTERVAL).await;
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => break stream,
                    Err(error) if started_at.elapsed() > HOST_STARTUP_TIMEOUT => {
                        return Err(error).context("terminal host did not start listening");
                    }
                    Err(_) => {}
                }
            };
            (session_id, Hello::Spawn(request), stream)
        }
        Session::Attach(session_id) => {
            OWN_SESSIONS.lock().insert(session_id.clone());
            let stream = UnixStream::connect(socket_path(&session_id))
                .await
                .with_context(|| format!("no terminal host is running session {session_id}"))?;
            (session_id, Hello::Attach, stream)
        }
    };

    let (mut reader, mut writer) = stream.split();
    writer
        .write_all(&frame(HELLO_TAG, &serde_json::to_vec(&hello)?))
        .await?;
    let (tag, payload) = read_frame(&mut reader)
        .await?
        .context("terminal host closed the connection")?;
    match tag {
        ATTACHED_TAG => {
            let attached: Attached = serde_json::from_slice(&payload)?;
            Ok(HostConnection {
                session_id,
                pid: attached.pid,
                reader,
                writer,
            })
        }
        ERROR_TAG => Err(anyhow!("{}", String::from_utf8_lossy(&payload))),
        _ => bail!("unexpected terminal host greeting {tag}"),
    }
}

/// Ends the hosts whose sessions are neither in `saved_sessions` nor used by
/// this process. Nothing would attach to them again, and a host only ends when
/// its shell does, so without this they would run until the machine restarts.
pub async fn end_unknown_sessions(saved_sessions: &HashSet<String>) {
    let directory = hosts_directory();
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            log::error!("failed to list {}: {error}", directory.display());
            return;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => {
                log::warn!("failed to list {}: {error}", directory.display());
                continue;
            }
        };
        if path.extension() != Some("sock".as_ref()) {
            continue;
        }
        let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if saved_sessions.contains(session_id) || OWN_SESSIONS.lock().contains(session_id) {
            continue;
        }
        match UnixStream::connect(&path).await {
            Ok(stream) => match end_session(stream).await {
                Ok(()) => log::info!("ended the unused terminal host session {session_id}"),
                Err(error) => {
                    log::warn!("failed to end terminal host session {session_id}: {error:#}")
                }
            },
            // Left behind by a host that did not get to remove it.
            Err(_) => {
                if let Err(error) = std::fs::remove_file(&path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    log::warn!("failed to remove stale socket {}: {error}", path.display());
                }
            }
        }
    }
}

async fn end_session(stream: UnixStream) -> Result<()> {
    let (mut reader, mut writer) = stream.split();
    writer
        .write_all(&frame(HELLO_TAG, &serde_json::to_vec(&Hello::Attach)?))
        .await?;
    let (tag, payload) = read_frame(&mut reader)
        .await?
        .context("terminal host closed the connection")?;
    match tag {
        ATTACHED_TAG => {
            writer.write_all(&frame(KILL_TAG, &[])).await?;
            writer.close().await?;
            Ok(())
        }
        // The host's shell has already exited, and the host ends on its own.
        ERROR_TAG => {
            log::debug!(
                "terminal host refused to attach: {}",
                String::from_utf8_lossy(&payload)
            );
            Ok(())
        }
        _ => bail!("unexpected terminal host greeting {tag}"),
    }
}

/// Zed's end of a terminal whose shell runs in a host process.
pub(crate) struct HostedPty {
    session_id: String,
    frames: UnboundedSender<Vec<u8>>,
    _reader: Task<()>,
}

impl HostedPty {
    /// Feeds the host's output into `term` and reports it through `events`.
    pub(crate) fn start(
        connection: HostConnection,
        term: Arc<AlacrittyTermLock>,
        events: UnboundedSender<PtyEvent>,
        executor: &BackgroundExecutor,
    ) -> Self {
        let HostConnection {
            session_id,
            reader,
            mut writer,
            ..
        } = connection;
        let (frames, mut frame_receiver) = mpsc::unbounded::<Vec<u8>>();
        // Left to run after the terminal lets go of the host, so that a kill
        // queued just before is still delivered.
        executor
            .spawn(async move {
                while let Some(frame) = frame_receiver.next().await {
                    if let Err(error) = writer.write_all(&frame).await {
                        log::warn!("failed to write to the terminal host: {error}");
                        return;
                    }
                }
                writer.close().await.ok();
            })
            .detach();
        let reader = executor.spawn(read_host_output(reader, term, events));
        Self {
            session_id,
            frames,
            _reader: reader,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn write(&self, input: &Cow<'static, [u8]>) {
        if !input.is_empty() {
            self.send(INPUT_TAG, input);
        }
    }

    pub(crate) fn resize(&self, size: ScreenSize) {
        match serde_json::to_vec(&size) {
            Ok(payload) => self.send(RESIZE_TAG, &payload),
            Err(error) => log::error!("failed to encode a terminal host resize: {error}"),
        }
    }

    /// Asks the host to kill the shell, which ends the host too.
    pub(crate) fn kill(&self) {
        self.send(KILL_TAG, &[]);
    }

    fn send(&self, tag: u8, payload: &[u8]) {
        self.frames.unbounded_send(frame(tag, payload)).ok();
    }
}

async fn read_host_output(
    mut reader: ReadHalf<UnixStream>,
    term: Arc<AlacrittyTermLock>,
    events: UnboundedSender<PtyEvent>,
) {
    let mut processor = Processor::<StdSyncHandler>::new();
    let exit = loop {
        let (tag, payload) = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break TerminalBackendEvent::Exit,
            Err(error) => {
                log::warn!("lost the connection to the terminal host: {error}");
                break TerminalBackendEvent::Exit;
            }
        };
        match tag {
            SNAPSHOT_TAG => {
                let Some((size, bytes)) = decode_snapshot(&payload) else {
                    log::error!("terminal host sent a malformed snapshot");
                    continue;
                };
                replay_snapshot(&term, size, bytes);
                events.unbounded_send(PtyEvent::HostAttached(size)).ok();
            }
            OUTPUT_TAG => {
                processor.advance(&mut *term.lock(), &payload);
                events
                    .unbounded_send(PtyEvent::Event(TerminalBackendEvent::Wakeup, 0))
                    .ok();
            }
            EXITED_TAG => {
                let code: Option<i32> = serde_json::from_slice(&payload).unwrap_or_default();
                break match code {
                    Some(code) => TerminalBackendEvent::ChildExit(exit_status(code)),
                    None => TerminalBackendEvent::Exit,
                };
            }
            tag => log::warn!("unexpected terminal host message {tag}"),
        }
    };
    events.unbounded_send(PtyEvent::Event(exit, 0)).ok();
}

#[cfg(windows)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt as _;
    ExitStatus::from_raw(code as u32)
}

#[cfg(unix)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    ExitStatus::from_raw(code << 8)
}

fn encode_snapshot(size: ScreenSize, bytes: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + bytes.len());
    payload.extend_from_slice(&(size.columns as u32).to_le_bytes());
    payload.extend_from_slice(&(size.lines as u32).to_le_bytes());
    payload.extend_from_slice(bytes);
    payload
}

fn decode_snapshot(payload: &[u8]) -> Option<(ScreenSize, &[u8])> {
    let columns = u32::from_le_bytes(payload.get(0..4)?.try_into().ok()?);
    let lines = u32::from_le_bytes(payload.get(4..8)?.try_into().ok()?);
    let size = ScreenSize {
        columns: columns as usize,
        lines: lines as usize,
        cell_width: 0,
        cell_height: 0,
    };
    Some((size, payload.get(8..)?))
}

fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(tag);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Reads one frame, or returns `None` when the connection closed between
/// frames.
async fn read_frame(
    reader: &mut (impl futures::AsyncRead + Unpin),
) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match reader.read_exact(&mut header).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAX_FRAME_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("terminal host frame of {length} bytes"),
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(Some((header[0], payload)))
}

fn spawn_host(socket_path: &Path) -> Result<()> {
    if let Some(directory) = socket_path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("creating {}", directory.display()))?;
    }
    let executable = std::env::current_exe().context("locating the Zed executable")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--terminal-host")
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

    let mut child = spawned.context("starting the terminal host")?;
    // Reaps the host when it exits before Zed does.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// The entry point of `zed --terminal-host <socket>`.
pub fn run(socket_path: &Path) -> Result<()> {
    // Zed starts the host in a new process group, which on Windows also makes
    // the process ignore Ctrl+C, and the shell inherits that: ^C from the
    // ConPTY would reach it and do nothing. Normal handling is restored before
    // the shell is started.
    #[cfg(windows)]
    if let Err(error) =
        unsafe { windows::Win32::System::Console::SetConsoleCtrlHandler(None, false) }
    {
        log::error!("failed to re-enable Ctrl+C for the terminal host's shell: {error}");
    }
    let executor = Rc::new(LocalExecutor::new());
    smol::block_on(executor.run(serve(executor.clone(), socket_path.to_path_buf())))
}

#[derive(Default)]
struct Host {
    pty: Option<Rc<HostPty>>,
    /// Writes to the attached Zed. Replaced, which cancels it, when another
    /// Zed attaches.
    client_writer: Option<smol::Task<()>>,
    next_generation: u64,
    exit: Option<UnboundedSender<()>>,
}

async fn serve(executor: Rc<LocalExecutor<'static>>, socket_path: PathBuf) -> Result<()> {
    let Some(listener) = bind(&socket_path).await? else {
        return Ok(());
    };

    let (exit_sender, mut exit_receiver) = mpsc::unbounded();
    let host = Rc::new(RefCell::new(Host {
        exit: Some(exit_sender),
        ..Host::default()
    }));

    // A host whose Zed went away before asking for the shell has nothing to
    // run.
    executor
        .spawn({
            let host = host.clone();
            async move {
                Timer::after(SPAWN_REQUEST_TIMEOUT).await;
                let mut host = host.borrow_mut();
                if host.pty.is_none() {
                    host.request_exit();
                }
            }
        })
        .detach();

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
                                    log::error!("terminal host connection failed: {error:#}");
                                }
                            })
                            .detach();
                    }
                    Err(error) => {
                        log::error!("terminal host failed to accept a connection: {error}");
                        Timer::after(ACCEPT_RETRY_INTERVAL).await;
                    }
                }
            }
        }
    };
    futures::future::select(Box::pin(accept_loop), exit_receiver.next()).await;

    // Lets the attached Zed's writer send the exit before the process ends.
    let (pty, client_writer) = {
        let mut host = host.borrow_mut();
        (host.pty.take(), host.client_writer.take())
    };
    if let Some(pty) = pty {
        pty.release_client();
    }
    if let Some(client_writer) = client_writer {
        futures::future::select(client_writer, Timer::after(EXIT_FLUSH_TIMEOUT)).await;
    }
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

async fn handle_connection(
    executor: Rc<LocalExecutor<'static>>,
    host: Rc<RefCell<Host>>,
    stream: UnixStream,
) -> Result<()> {
    let (mut reader, mut writer) = stream.split();
    let (tag, payload) = read_frame(&mut reader)
        .await?
        .context("connection closed before its hello")?;
    if tag != HELLO_TAG {
        bail!("expected a hello, got {tag}");
    }
    let hello: Hello = serde_json::from_slice(&payload).context("parsing hello")?;

    let existing_pty = host.borrow().pty.clone();
    let pty = match (existing_pty, hello) {
        (Some(pty), _) => pty,
        (None, Hello::Spawn(request)) => match spawn_pty(&host, request) {
            Ok(pty) => pty,
            Err(error) => {
                let message = format!("{error:#}");
                writer
                    .write_all(&frame(ERROR_TAG, message.as_bytes()))
                    .await?;
                host.borrow_mut().request_exit();
                return Ok(());
            }
        },
        (None, Hello::Attach) => {
            writer
                .write_all(&frame(ERROR_TAG, b"the terminal's shell has exited"))
                .await?;
            host.borrow_mut().request_exit();
            return Ok(());
        }
    };

    let (output, outputs) = mpsc::unbounded();
    let client_writer = executor.spawn(write_outputs(writer, outputs));
    let generation = {
        let mut host = host.borrow_mut();
        host.next_generation += 1;
        host.client_writer = Some(client_writer);
        host.next_generation
    };
    pty.attach(HostClient { generation, output });

    loop {
        let (tag, payload) = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                log::warn!("terminal host failed to read from Zed: {error}");
                break;
            }
        };
        // A Zed that attached later took the terminal over.
        if !pty.is_attached(generation) {
            break;
        }
        match tag {
            INPUT_TAG => pty.send(PtyCommand::Input(payload)),
            RESIZE_TAG => match serde_json::from_slice(&payload) {
                Ok(size) => pty.resize(size),
                Err(error) => log::warn!("terminal host got a malformed resize: {error}"),
            },
            KILL_TAG => pty.send(PtyCommand::Kill),
            tag => log::warn!("unexpected message {tag} from Zed"),
        }
    }
    pty.detach(generation);
    Ok(())
}

fn spawn_pty(host: &Rc<RefCell<Host>>, request: SpawnRequest) -> Result<Rc<HostPty>> {
    let exit = host
        .borrow()
        .exit
        .clone()
        .context("the terminal host is exiting")?;
    let options = HostPtyOptions {
        shell: request.program.map(|program| (program, request.args)),
        working_directory: request.working_directory,
        env: request.env,
        escape_args: request.escape_args,
        scrolling_history: request.scrolling_history,
        size: request.size,
    };
    let pty = HostPty::spawn(options, move |_| {
        exit.unbounded_send(()).ok();
    })
    .context("starting the shell")?;
    let pty = Rc::new(pty);
    host.borrow_mut().pty = Some(pty.clone());
    Ok(pty)
}

impl Host {
    fn request_exit(&mut self) {
        if let Some(exit) = self.exit.take() {
            exit.unbounded_send(()).ok();
        }
    }
}

async fn write_outputs(
    mut writer: WriteHalf<UnixStream>,
    mut outputs: UnboundedReceiver<HostOutput>,
) {
    while let Some(output) = outputs.next().await {
        let frame = match output {
            HostOutput::Attached { pid } => match serde_json::to_vec(&Attached { pid }) {
                Ok(payload) => frame(ATTACHED_TAG, &payload),
                Err(error) => {
                    log::error!("failed to encode the terminal host greeting: {error}");
                    return;
                }
            },
            HostOutput::Snapshot { size, bytes } => {
                frame(SNAPSHOT_TAG, &encode_snapshot(size, &bytes))
            }
            HostOutput::Output(bytes) => frame(OUTPUT_TAG, &bytes),
            HostOutput::Exited(code) => match serde_json::to_vec(&code) {
                Ok(payload) => frame(EXITED_TAG, &payload),
                Err(error) => {
                    log::error!("failed to encode the shell's exit: {error}");
                    return;
                }
            },
        };
        if let Err(error) = writer.write_all(&frame).await {
            log::warn!("terminal host failed to write to Zed: {error}");
            return;
        }
    }
    writer.close().await.ok();
}
