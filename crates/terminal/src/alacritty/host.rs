//! The alacritty side of a terminal host: the PTY a host process owns, the
//! screen it keeps for it, and the replay of that screen into the emulator of a
//! Zed that attaches.

use std::{
    collections::VecDeque,
    io::{self, ErrorKind, Read as _, Write as _},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Instant,
};

use alacritty_terminal::{
    event::{Event as AlacTermEvent, EventListener, OnResize, WindowSize},
    grid::Dimensions,
    index::{Column, Line},
    term::{
        Config, Term, TermMode,
        cell::{Cell as AlacCell, Flags, Hyperlink as AlacHyperlink},
    },
    tty::{self, EventedPty as _, EventedReadWrite as _},
};
use futures::channel::mpsc::UnboundedSender;
use parking_lot::Mutex;
use polling::{Event as PollingEvent, Events, PollMode, Poller};
use vte::ansi::{Color, NamedColor, Processor, StdSyncHandler};

use super::AlacrittyTermLock;
use crate::terminal_host::ScreenSize;

const READ_BUFFER_SIZE: usize = 0x10_0000;

impl Dimensions for ScreenSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

fn window_size(size: ScreenSize) -> WindowSize {
    WindowSize {
        num_lines: size.lines as u16,
        num_cols: size.columns as u16,
        cell_width: size.cell_width,
        cell_height: size.cell_height,
    }
}

pub(crate) enum PtyCommand {
    Input(Vec<u8>),
    Resize(ScreenSize),
    Kill,
}

/// What a host sends to the Zed attached to its terminal.
pub(crate) enum HostOutput {
    Attached { pid: u32 },
    Snapshot { size: ScreenSize, bytes: Vec<u8> },
    Output(Vec<u8>),
    Exited(Option<i32>),
}

pub(crate) struct HostClient {
    pub(crate) generation: u64,
    pub(crate) output: UnboundedSender<HostOutput>,
}

pub(crate) struct HostPtyOptions {
    pub(crate) shell: Option<(String, Vec<String>)>,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) env: Vec<(String, String)>,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) escape_args: bool,
    pub(crate) scrolling_history: usize,
    pub(crate) size: ScreenSize,
}

enum ListenerEvent {
    PtyWrite(String),
    Title(Option<String>),
}

#[derive(Clone)]
struct HostListener {
    events: Arc<Mutex<Vec<ListenerEvent>>>,
}

impl EventListener for HostListener {
    fn send_event(&self, event: AlacTermEvent) {
        let event = match event {
            AlacTermEvent::PtyWrite(text) => ListenerEvent::PtyWrite(text),
            AlacTermEvent::Title(title) => ListenerEvent::Title(Some(title)),
            AlacTermEvent::ResetTitle => ListenerEvent::Title(None),
            _ => return,
        };
        self.events.lock().push(event);
    }
}

/// The host's copy of the terminal's screen, kept so that a Zed attaching
/// later sees what the shell has drawn while none was attached.
struct HostScreen {
    term: Term<HostListener>,
    parser: Processor<StdSyncHandler>,
    listener_events: Arc<Mutex<Vec<ListenerEvent>>>,
    title: Option<String>,
    client: Option<HostClient>,
}

impl HostScreen {
    fn advance(&mut self, bytes: &[u8], pending_writes: &mut VecDeque<Vec<u8>>) {
        self.parser.advance(&mut self.term, bytes);
        self.handle_listener_events(pending_writes);
        if let Some(client) = &self.client
            && client
                .output
                .unbounded_send(HostOutput::Output(bytes.to_vec()))
                .is_err()
        {
            self.client = None;
        }
    }

    fn handle_listener_events(&mut self, pending_writes: &mut VecDeque<Vec<u8>>) {
        let events = std::mem::take(&mut *self.listener_events.lock());
        for event in events {
            match event {
                // An attached Zed answers the shell's queries from its own
                // emulator, which sees the same bytes; answering here too would
                // send the shell every answer twice.
                ListenerEvent::PtyWrite(text) => {
                    if self.client.is_none() {
                        pending_writes.push_back(text.into_bytes());
                    }
                }
                ListenerEvent::Title(title) => self.title = title,
            }
        }
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    fn stop_sync(&mut self, pending_writes: &mut VecDeque<Vec<u8>>) {
        self.parser.stop_sync(&mut self.term);
        self.handle_listener_events(pending_writes);
    }
}

/// A shell running in a PTY owned by the host process.
pub(crate) struct HostPty {
    screen: Arc<Mutex<HostScreen>>,
    commands: mpsc::Sender<PtyCommand>,
    poller: Arc<Poller>,
    pid: u32,
}

impl HostPty {
    /// Starts the shell. `on_exit` runs on the PTY thread once the shell has
    /// exited and an attached Zed has been told.
    pub(crate) fn spawn(
        options: HostPtyOptions,
        on_exit: impl FnOnce(Option<i32>) + Send + 'static,
    ) -> io::Result<Self> {
        let pty_options = tty::Options {
            shell: options
                .shell
                .map(|(program, args)| tty::Shell::new(program, args)),
            working_directory: options.working_directory,
            drain_on_exit: true,
            env: options.env.into_iter().collect(),
            #[cfg(not(windows))]
            child_signal_mask: None,
            #[cfg(windows)]
            escape_args: options.escape_args,
        };
        let pty = tty::new(&pty_options, window_size(options.size), 0)?;
        let pid = child_pid(&pty);

        let listener_events = Arc::new(Mutex::new(Vec::new()));
        let config = Config {
            scrolling_history: options.scrolling_history,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &options.size,
            HostListener {
                events: listener_events.clone(),
            },
        );
        let screen = Arc::new(Mutex::new(HostScreen {
            term,
            parser: Processor::new(),
            listener_events,
            title: None,
            client: None,
        }));

        let poller = Arc::new(Poller::new()?);
        let (commands, command_receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("terminal host pty".to_string())
            .spawn({
                let screen = screen.clone();
                let poller = poller.clone();
                move || {
                    let code = run_pty(pty, pid, &poller, &command_receiver, &screen);
                    if let Some(client) = screen.lock().client.take() {
                        client.output.unbounded_send(HostOutput::Exited(code)).ok();
                    }
                    on_exit(code);
                }
            })?;

        Ok(Self {
            screen,
            commands,
            poller,
            pid,
        })
    }

    pub(crate) fn send(&self, command: PtyCommand) {
        if self.commands.send(command).is_ok()
            && let Err(error) = self.poller.notify()
        {
            log::error!("failed to wake the terminal host pty thread: {error}");
        }
    }

    pub(crate) fn resize(&self, size: ScreenSize) {
        self.screen.lock().term.resize(size);
        self.send(PtyCommand::Resize(size));
    }

    /// Makes `client` the Zed this terminal's output goes to, replacing any
    /// earlier one. It is sent the screen as it is now, and from then on every
    /// byte the shell writes after it.
    pub(crate) fn attach(&self, client: HostClient) {
        let mut screen = self.screen.lock();
        let size = ScreenSize {
            columns: screen.term.columns(),
            lines: screen.term.screen_lines(),
            cell_width: 0,
            cell_height: 0,
        };
        let bytes = snapshot(&screen.term, screen.title.as_deref());
        let attached = client
            .output
            .unbounded_send(HostOutput::Attached { pid: self.pid })
            .and_then(|()| {
                client
                    .output
                    .unbounded_send(HostOutput::Snapshot { size, bytes })
            });
        if attached.is_ok() {
            screen.client = Some(client);
        }
    }

    pub(crate) fn is_attached(&self, generation: u64) -> bool {
        self.screen
            .lock()
            .client
            .as_ref()
            .is_some_and(|client| client.generation == generation)
    }

    pub(crate) fn detach(&self, generation: u64) {
        let mut screen = self.screen.lock();
        if screen
            .client
            .as_ref()
            .is_some_and(|client| client.generation == generation)
        {
            screen.client = None;
        }
    }

    /// Drops the attached Zed's output channel, so its connection can finish
    /// writing what was queued and close.
    pub(crate) fn release_client(&self) {
        self.screen.lock().client = None;
    }
}

#[cfg(windows)]
fn child_pid(pty: &tty::Pty) -> u32 {
    use windows::Win32::{Foundation::HANDLE, System::Threading::GetProcessId};

    let child = pty.child_watcher();
    child
        .pid()
        .map(u32::from)
        .unwrap_or_else(|| unsafe { GetProcessId(HANDLE(child.raw_handle() as _)) })
}

#[cfg(unix)]
fn child_pid(pty: &tty::Pty) -> u32 {
    pty.child().id()
}

/// Drives the PTY until its shell exits or the host is told to kill it, and
/// returns the shell's exit code when it is known.
fn run_pty(
    mut pty: tty::Pty,
    pid: u32,
    poller: &Arc<Poller>,
    commands: &mpsc::Receiver<PtyCommand>,
    screen: &Mutex<HostScreen>,
) -> Option<i32> {
    let mut interest = PollingEvent::readable(0);
    if let Err(error) = unsafe { pty.register(poller, interest, PollMode::Level) } {
        log::error!("failed to register the terminal host pty: {error}");
        return None;
    }

    let mut events = Events::with_capacity(NonZeroUsize::new(1024).unwrap_or(NonZeroUsize::MIN));
    let mut buffer = vec![0u8; READ_BUFFER_SIZE];
    let mut pending_writes = VecDeque::new();
    let mut killed = false;

    let code = 'event_loop: loop {
        let timeout = screen
            .lock()
            .sync_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        events.clear();
        if let Err(error) = poller.wait(&mut events, timeout) {
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            log::error!("terminal host pty polling failed: {error}");
            break None;
        }

        if screen
            .lock()
            .sync_deadline()
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            screen.lock().stop_sync(&mut pending_writes);
        }

        loop {
            match commands.try_recv() {
                Ok(PtyCommand::Input(bytes)) => pending_writes.push_back(bytes),
                Ok(PtyCommand::Resize(size)) => pty.on_resize(window_size(size)),
                Ok(PtyCommand::Kill) | Err(mpsc::TryRecvError::Disconnected) => {
                    killed = true;
                    break 'event_loop None;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }

        for event in events.iter() {
            match event.key {
                tty::PTY_CHILD_EVENT_TOKEN => {
                    if let Some(tty::ChildEvent::Exited(status)) = pty.next_child_event() {
                        if let Err(error) =
                            read_available(&mut pty, &mut buffer, screen, &mut pending_writes)
                        {
                            log::warn!("failed to drain the terminal host pty: {error}");
                        }
                        break 'event_loop status.and_then(|status| status.code());
                    }
                }
                tty::PTY_READ_WRITE_TOKEN => {
                    if event.is_interrupt() {
                        continue;
                    }
                    if event.readable
                        && let Err(error) =
                            read_available(&mut pty, &mut buffer, screen, &mut pending_writes)
                    {
                        // On Linux, reading a PTY whose client hung up fails with
                        // `EIO`; the child's exit event follows.
                        #[cfg(target_os = "linux")]
                        if error.raw_os_error() == Some(libc::EIO) {
                            continue;
                        }
                        log::error!("failed to read the terminal host pty: {error}");
                        break 'event_loop None;
                    }
                    if event.writable
                        && let Err(error) = write_pending(&mut pty, &mut pending_writes)
                    {
                        log::error!("failed to write to the terminal host pty: {error}");
                        break 'event_loop None;
                    }
                }
                _ => {}
            }
        }

        let needs_write = !pending_writes.is_empty();
        if needs_write != interest.writable {
            interest.writable = needs_write;
            if let Err(error) = pty.reregister(poller, interest, PollMode::Level) {
                log::error!("failed to reregister the terminal host pty: {error}");
                break None;
            }
        }
    };

    if let Err(error) = pty.deregister(poller) {
        log::warn!("failed to deregister the terminal host pty: {error}");
    }
    if killed {
        kill_process(pid);
    }
    code
}

fn read_available(
    pty: &mut tty::Pty,
    buffer: &mut [u8],
    screen: &Mutex<HostScreen>,
    pending_writes: &mut VecDeque<Vec<u8>>,
) -> io::Result<()> {
    loop {
        let count = match pty.reader().read(buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        };
        screen.lock().advance(&buffer[..count], pending_writes);
    }
}

fn write_pending(pty: &mut tty::Pty, pending_writes: &mut VecDeque<Vec<u8>>) -> io::Result<()> {
    while let Some(bytes) = pending_writes.front_mut() {
        match pty.writer().write(bytes) {
            Ok(0) => return Ok(()),
            Ok(count) if count < bytes.len() => {
                bytes.drain(..count);
            }
            Ok(_) => {
                pending_writes.pop_front();
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn kill_process(pid: u32) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    if let Some(process) = system.process(pid) {
        process.kill();
    }
}

/// Resizes `term` to the size a host's snapshot was taken at and replays the
/// snapshot into it. The emulator keeps that size until Zed resizes it to the
/// terminal's bounds, which also resizes the host.
pub(crate) fn replay_snapshot(term: &AlacrittyTermLock, size: ScreenSize, bytes: &[u8]) {
    let mut term = term.lock();
    term.resize(size);
    Processor::<StdSyncHandler>::new().advance(&mut *term, bytes);
}

/// Encodes `term`'s scrollback, screen, cursor and input modes as the escape
/// sequences that draw them, for an emulator of the same size that has
/// nothing on it yet.
///
/// While a full-screen program has the alternate screen up, only that screen
/// is sent: alacritty keeps the primary screen out of reach until the program
/// leaves it.
fn snapshot<L: EventListener>(term: &Term<L>, title: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    let mode = *term.mode();
    let grid = term.grid();

    if mode.contains(TermMode::ALT_SCREEN) {
        out.extend_from_slice(b"\x1b[?1049h");
    }

    let history = grid.history_size() as i32;
    let screen_lines = grid.screen_lines() as i32;
    let columns = grid.columns();
    let mut pen = Pen::default();
    let mut previous_row_wraps = false;
    for line in -history..screen_lines {
        let row = &grid[Line(line)];
        let wraps = columns > 0 && row[Column(columns - 1)].flags.contains(Flags::WRAPLINE);
        let end = if wraps {
            columns
        } else {
            (0..columns)
                .rev()
                .find(|&column| !is_blank(&row[Column(column)]))
                .map_or(0, |column| column + 1)
        };

        // A wrapped row continues by itself once its next character is
        // printed; any other row starts on a new line.
        if line > -history && !(previous_row_wraps && end > 0) {
            pen.switch_to(&Pen::default(), &mut out);
            out.extend_from_slice(b"\r\n");
        }

        for column in 0..end {
            let cell = &row[Column(column)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            pen.switch_to(&Pen::of(cell), &mut out);
            push_char(cell.c, &mut out);
            for character in cell.zerowidth().into_iter().flatten() {
                push_char(*character, &mut out);
            }
        }
        previous_row_wraps = wraps;
    }

    pen.switch_to(&Pen::default(), &mut out);
    let cursor = grid.cursor.point;
    out.extend_from_slice(
        format!("\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes(),
    );
    pen.switch_to(&Pen::of(&grid.cursor.template), &mut out);

    let modes: [(TermMode, &[u8]); 10] = [
        (TermMode::APP_CURSOR, b"\x1b[?1h"),
        (TermMode::APP_KEYPAD, b"\x1b="),
        (TermMode::MOUSE_REPORT_CLICK, b"\x1b[?1000h"),
        (TermMode::MOUSE_DRAG, b"\x1b[?1002h"),
        (TermMode::MOUSE_MOTION, b"\x1b[?1003h"),
        (TermMode::FOCUS_IN_OUT, b"\x1b[?1004h"),
        (TermMode::UTF8_MOUSE, b"\x1b[?1005h"),
        (TermMode::SGR_MOUSE, b"\x1b[?1006h"),
        (TermMode::BRACKETED_PASTE, b"\x1b[?2004h"),
        (TermMode::INSERT, b"\x1b[4h"),
    ];
    for (flag, sequence) in modes {
        if mode.contains(flag) {
            out.extend_from_slice(sequence);
        }
    }
    if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
        out.extend_from_slice(b"\x1b[20h");
    }
    // Turned off only after the rows above, which rely on wrapping.
    if !mode.contains(TermMode::LINE_WRAP) {
        out.extend_from_slice(b"\x1b[?7l");
    }
    if !mode.contains(TermMode::SHOW_CURSOR) {
        out.extend_from_slice(b"\x1b[?25l");
    }
    let keyboard_flags = (mode & TermMode::KITTY_KEYBOARD_PROTOCOL).bits()
        >> TermMode::DISAMBIGUATE_ESC_CODES.bits().trailing_zeros();
    if keyboard_flags != 0 {
        out.extend_from_slice(format!("\x1b[>{keyboard_flags}u").as_bytes());
    }

    if let Some(title) = title {
        let title: String = title
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        out.extend_from_slice(format!("\x1b]2;{title}\x07").as_bytes());
    }
    out
}

fn is_blank(cell: &AlacCell) -> bool {
    matches!(cell.c, ' ' | '\t')
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.extra.is_none()
}

fn push_char(character: char, out: &mut Vec<u8>) {
    // alacritty records a tab in the first cell it skips, for copying; the
    // cells after it are spaces of their own.
    let character = if character == '\t' { ' ' } else { character };
    let mut encoded = [0; 4];
    out.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
}

const PEN_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

/// The graphic rendition and hyperlink the next character is printed with.
#[derive(Clone, PartialEq)]
struct Pen {
    foreground: Color,
    background: Color,
    flags: Flags,
    underline_color: Option<Color>,
    hyperlink: Option<AlacHyperlink>,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            foreground: Color::Named(NamedColor::Foreground),
            background: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline_color: None,
            hyperlink: None,
        }
    }
}

impl Pen {
    fn of(cell: &AlacCell) -> Self {
        Self {
            foreground: cell.fg,
            background: cell.bg,
            flags: cell.flags & PEN_FLAGS,
            underline_color: cell.underline_color(),
            hyperlink: cell.hyperlink(),
        }
    }

    fn switch_to(&mut self, next: &Pen, out: &mut Vec<u8>) {
        if self.foreground != next.foreground
            || self.background != next.background
            || self.flags != next.flags
            || self.underline_color != next.underline_color
        {
            let mut sequence = String::from("\x1b[0");
            let attributes = [
                (Flags::BOLD, ";1"),
                (Flags::DIM, ";2"),
                (Flags::ITALIC, ";3"),
                (Flags::UNDERLINE, ";4"),
                (Flags::DOUBLE_UNDERLINE, ";4:2"),
                (Flags::UNDERCURL, ";4:3"),
                (Flags::DOTTED_UNDERLINE, ";4:4"),
                (Flags::DASHED_UNDERLINE, ";4:5"),
                (Flags::INVERSE, ";7"),
                (Flags::HIDDEN, ";8"),
                (Flags::STRIKEOUT, ";9"),
            ];
            for (flag, parameter) in attributes {
                if next.flags.contains(flag) {
                    sequence.push_str(parameter);
                }
            }
            push_color(&mut sequence, next.foreground, 30, 90, 38);
            push_color(&mut sequence, next.background, 40, 100, 48);
            if let Some(color) = next.underline_color {
                push_extended_color(&mut sequence, color, 58);
            }
            sequence.push('m');
            out.extend_from_slice(sequence.as_bytes());
        }

        if self.hyperlink != next.hyperlink {
            match &next.hyperlink {
                Some(hyperlink) => out.extend_from_slice(
                    format!("\x1b]8;id={};{}\x1b\\", hyperlink.id(), hyperlink.uri()).as_bytes(),
                ),
                None => out.extend_from_slice(b"\x1b]8;;\x1b\\"),
            }
        }

        *self = next.clone();
    }
}

/// Appends the SGR parameters for `color`, leaving the default colors to the
/// reset every sequence starts with.
fn push_color(sequence: &mut String, color: Color, normal: usize, bright: usize, extended: u8) {
    match color {
        Color::Named(named) => {
            let index = named as usize;
            if index < 8 {
                sequence.push_str(&format!(";{}", normal + index));
            } else if index < 16 {
                sequence.push_str(&format!(";{}", bright + index - 8));
            }
        }
        color => push_extended_color(sequence, color, extended),
    }
}

fn push_extended_color(sequence: &mut String, color: Color, parameter: u8) {
    match color {
        Color::Indexed(index) => sequence.push_str(&format!(";{parameter};5;{index}")),
        Color::Spec(rgb) => {
            sequence.push_str(&format!(";{parameter};2;{};{};{}", rgb.r, rgb.g, rgb.b))
        }
        Color::Named(named) => {
            let index = named as usize;
            if index < 16 {
                sequence.push_str(&format!(";{parameter};5;{index}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alacritty_terminal::event::VoidListener;

    fn term_with(size: ScreenSize, bytes: &[u8]) -> Term<VoidListener> {
        let config = Config {
            scrolling_history: 100,
            ..Config::default()
        };
        let mut term = Term::new(config, &size, VoidListener);
        Processor::<StdSyncHandler>::new().advance(&mut term, bytes);
        term
    }

    fn rows(term: &Term<VoidListener>) -> Vec<String> {
        let grid = term.grid();
        (-(grid.history_size() as i32)..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line)];
                (0..grid.columns())
                    .map(|column| row[Column(column)].c)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn snapshot_reproduces_scrollback_screen_and_cursor() {
        let size = ScreenSize {
            columns: 10,
            lines: 3,
            cell_width: 0,
            cell_height: 0,
        };
        let source = term_with(
            size,
            b"one\r\ntwo\r\n\x1b[1;31mthree\x1b[0m\r\nfour wraps around\r\nfive",
        );
        let replayed = term_with(size, &snapshot(&source, None));

        assert_eq!(rows(&replayed), rows(&source));
        assert_eq!(replayed.grid().cursor.point, source.grid().cursor.point);
        let three = &replayed.grid()[Line(-(replayed.grid().history_size() as i32) + 2)];
        assert_eq!(three[Column(0)].fg, Color::Named(NamedColor::Red));
        assert!(three[Column(0)].flags.contains(Flags::BOLD));
    }

    #[test]
    fn snapshot_restores_input_modes() {
        let size = ScreenSize {
            columns: 10,
            lines: 3,
            cell_width: 0,
            cell_height: 0,
        };
        let source = term_with(size, b"\x1b[?2004h\x1b[?1h\x1b[?1049hfull screen");
        let replayed = term_with(size, &snapshot(&source, None));

        assert!(replayed.mode().contains(TermMode::BRACKETED_PASTE));
        assert!(replayed.mode().contains(TermMode::APP_CURSOR));
        assert!(replayed.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(rows(&replayed), rows(&source));
    }
}
