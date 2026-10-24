use std::time::Duration;

use acp_thread::{AcpThread, AgentThreadEntry, ThreadStatus, ToolCallStatus};
use agent_client_protocol::schema::v1 as acp;
use agent_ui::{AgentPanel, ConversationView};
use git_ui::working_tree_changes::WorkingTreeChanges;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Task,
    WeakEntity, Window, WindowHandle,
};
use project::{Project, git_store::GitStore};
use project_panel::ProjectPanel;
use terminal::TaskStatus;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{
    ItemNavHistory, MultiWorkspace, Workspace,
    item::{Item, ItemEvent},
};

/// The statuses come from entities spread over every window, which appear and go away on their
/// own, so they are polled instead of tracked through subscriptions.
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// The permanent "Main" tab: the project panel, the project's uncommitted and staged changes
/// with a commit form, and what every agent and terminal is doing, side by side.
pub struct MainPage {
    project_panel: Entity<ProjectPanel>,
    working_tree_changes: Entity<WorkingTreeChanges>,
    activity: Vec<ProjectActivity>,
    nav_history: Option<ItemNavHistory>,
    _refresh_task: Task<()>,
}

#[derive(PartialEq)]
struct ProjectActivity {
    name: SharedString,
    window: WindowHandle<MultiWorkspace>,
    workspace: WeakEntity<Workspace>,
    threads: Vec<ThreadRow>,
    terminals: Vec<TerminalRow>,
}

#[derive(PartialEq)]
struct ThreadRow {
    title: SharedString,
    state: ThreadState,
    conversation_view: WeakEntity<ConversationView>,
    /// Set for a subagent, `None` for the conversation's root thread.
    subagent_session: Option<acp::SessionId>,
}

#[derive(Clone, Copy, PartialEq)]
enum ThreadState {
    Running,
    WaitingForConfirmation,
    Error,
    Done,
}

#[derive(PartialEq)]
struct TerminalRow {
    title: SharedString,
    state: TerminalState,
    view: WeakEntity<TerminalView>,
    in_terminal_panel: bool,
}

#[derive(Clone, PartialEq)]
enum TerminalState {
    TaskRunning,
    TaskSucceeded,
    TaskFailed,
    TaskEnded,
    Running(SharedString),
    Idle,
}

impl MainPage {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project_panel: Entity<ProjectPanel>,
        git_store: Entity<GitStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let working_tree_changes =
            cx.new(|cx| WorkingTreeChanges::new(workspace, git_store, None, window, cx));
        let refresh_task = cx.spawn(async move |this, cx| {
            while this.update(cx, |this, cx| this.refresh(cx)).is_ok() {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        Self {
            project_panel,
            working_tree_changes,
            activity: Vec::new(),
            nav_history: None,
            _refresh_task: refresh_task,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let activity = collect_activity(cx);
        if activity != self.activity {
            self.activity = activity;
            cx.notify();
        }
    }

    fn render_activity(&self) -> AnyElement {
        if self.activity.is_empty() {
            return render_placeholder("No agent threads or terminals are open.");
        }

        let mut row_index = 0;
        v_flex()
            .gap_2()
            .children(self.activity.iter().map(|project| {
                v_flex()
                    .child(render_group_header(project.name.clone()))
                    .children(project.threads.iter().map(|thread| {
                        row_index += 1;
                        render_thread_row(row_index, project, thread)
                    }))
                    .children(project.terminals.iter().map(|terminal| {
                        row_index += 1;
                        render_terminal_row(row_index, project, terminal)
                    }))
            }))
            .into_any_element()
    }

    fn render_section(
        id: &'static str,
        title: &'static str,
        content: AnyElement,
        cx: &App,
    ) -> AnyElement {
        v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(
                div()
                    .id(id)
                    .flex_1()
                    .min_h_0()
                    .p_1()
                    .overflow_y_scroll()
                    .child(content),
            )
            .into_any_element()
    }
}

fn render_placeholder(text: &'static str) -> AnyElement {
    div()
        .p_2()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
        .into_any_element()
}

fn render_group_header(name: SharedString) -> impl IntoElement {
    div()
        .px_2()
        .pt_1()
        .child(Label::new(name).size(LabelSize::XSmall).color(Color::Muted))
}

fn render_thread_row(
    row_index: usize,
    project: &ProjectActivity,
    thread: &ThreadRow,
) -> AnyElement {
    let (icon, icon_color, state_label) = match thread.state {
        ThreadState::Running => (IconName::LoadCircle, Color::Accent, "Running"),
        ThreadState::WaitingForConfirmation => (IconName::Warning, Color::Warning, "Waiting"),
        ThreadState::Error => (IconName::XCircle, Color::Error, "Error"),
        ThreadState::Done => (IconName::Check, Color::Muted, "Done"),
    };
    let window_handle = project.window;
    let workspace = project.workspace.clone();
    let conversation_view = thread.conversation_view.clone();
    let subagent_session = thread.subagent_session.clone();
    ListItem::new(("main-page-thread", row_index))
        .spacing(ListItemSpacing::Dense)
        .indent_level(usize::from(thread.subagent_session.is_some()))
        .start_slot(Icon::new(icon).size(IconSize::Small).color(icon_color))
        .child(
            Label::new(thread.title.clone())
                .size(LabelSize::Small)
                .truncate(),
        )
        .end_slot(
            Label::new(state_label)
                .size(LabelSize::XSmall)
                .color(icon_color),
        )
        .on_click(move |_, _, cx| {
            let workspace = workspace.clone();
            let conversation_view = conversation_view.clone();
            let subagent_session = subagent_session.clone();
            // The thread may be in this window, which can't be updated through its handle while
            // it is handling this click.
            cx.defer(move |cx| {
                show_in_window(window_handle, &workspace, cx, |workspace, window, cx| {
                    open_thread(workspace, conversation_view, subagent_session, window, cx);
                });
            });
        })
        .into_any_element()
}

fn render_terminal_row(
    row_index: usize,
    project: &ProjectActivity,
    terminal: &TerminalRow,
) -> AnyElement {
    let (icon, icon_color, state_label): (IconName, Color, SharedString) = match &terminal.state {
        TerminalState::TaskRunning => (IconName::PlayFilled, Color::Accent, "Running".into()),
        TerminalState::TaskSucceeded => (IconName::Check, Color::Success, "Succeeded".into()),
        TerminalState::TaskFailed => (IconName::XCircle, Color::Error, "Failed".into()),
        TerminalState::TaskEnded => (IconName::Warning, Color::Warning, "Ended".into()),
        TerminalState::Running(program) => (IconName::PlayFilled, Color::Accent, program.clone()),
        TerminalState::Idle => (IconName::Terminal, Color::Muted, "Idle".into()),
    };
    let window_handle = project.window;
    let workspace = project.workspace.clone();
    let view = terminal.view.clone();
    let in_terminal_panel = terminal.in_terminal_panel;
    ListItem::new(("main-page-terminal", row_index))
        .spacing(ListItemSpacing::Dense)
        .start_slot(Icon::new(icon).size(IconSize::Small).color(icon_color))
        .child(
            Label::new(terminal.title.clone())
                .size(LabelSize::Small)
                .truncate(),
        )
        .end_slot(
            Label::new(state_label)
                .size(LabelSize::XSmall)
                .color(icon_color),
        )
        .on_click(move |_, _, cx| {
            let workspace = workspace.clone();
            let view = view.clone();
            cx.defer(move |cx| {
                show_in_window(window_handle, &workspace, cx, |workspace, window, cx| {
                    open_terminal(workspace, view, in_terminal_panel, window, cx);
                });
            });
        })
        .into_any_element()
}

/// Brings `workspace` to the front of its window and runs `f` on it.
fn show_in_window(
    window_handle: WindowHandle<MultiWorkspace>,
    workspace: &WeakEntity<Workspace>,
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>),
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    window_handle
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            multi_workspace.activate(workspace.clone(), None, window, cx);
            workspace.update(cx, |workspace, cx| f(workspace, window, cx));
        })
        .log_err();
}

fn open_thread(
    workspace: &mut Workspace,
    conversation_view: WeakEntity<ConversationView>,
    subagent_session: Option<acp::SessionId>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.focus_panel::<AgentPanel>(window, cx) else {
        return;
    };
    let Some(conversation_view) = conversation_view.upgrade() else {
        return;
    };
    let thread_id = conversation_view.read(cx).parent_id();
    panel.update(cx, |panel, cx| {
        if panel.is_retained_thread(&thread_id) {
            panel.activate_retained_thread(thread_id, true, window, cx);
        }
    });
    if let Some(session_id) = subagent_session {
        conversation_view.update(cx, |conversation_view, cx| {
            conversation_view.navigate_to_thread(session_id, window, cx);
        });
    }
}

fn open_terminal(
    workspace: &mut Workspace,
    view: WeakEntity<TerminalView>,
    in_terminal_panel: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(view) = view.upgrade() else {
        return;
    };
    if !in_terminal_panel {
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let Some(panel) = workspace.focus_panel::<TerminalPanel>(window, cx) else {
        return;
    };
    let pane = panel
        .read(cx)
        .panes()
        .into_iter()
        .find(|pane| pane.read(cx).index_for_item(&view).is_some())
        .cloned();
    if let Some(pane) = pane {
        pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(&view) {
                pane.activate_item(index, true, true, window, cx);
            }
        });
    }
}

fn collect_activity(cx: &App) -> Vec<ProjectActivity> {
    let mut projects = Vec::new();
    for window in cx.windows() {
        let Some(window) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            let workspace_ref = workspace.read(cx);
            let threads = workspace_ref
                .panel::<AgentPanel>(cx)
                .map(|panel| thread_rows(&panel, cx))
                .unwrap_or_default();
            let terminals = terminal_rows(workspace_ref, cx);
            if threads.is_empty() && terminals.is_empty() {
                continue;
            }
            projects.push(ProjectActivity {
                name: project_name(workspace_ref.project(), cx),
                window,
                workspace: workspace.downgrade(),
                threads,
                terminals,
            });
        }
    }
    projects
}

fn project_name(project: &Entity<Project>, cx: &App) -> SharedString {
    let names = project
        .read(cx)
        .worktree_root_names(cx)
        .collect::<Vec<_>>()
        .join(", ");
    if names.is_empty() {
        "Empty Project".into()
    } else {
        names.into()
    }
}

fn thread_rows(panel: &Entity<AgentPanel>, cx: &App) -> Vec<ThreadRow> {
    let mut rows = Vec::new();
    for conversation_view in panel.read(cx).conversation_views() {
        let view = conversation_view.read(cx);
        let Some(root_thread_view) = view.root_thread_view() else {
            continue;
        };
        let root_thread = root_thread_view.read(cx).thread.read(cx);
        if root_thread.is_draft_thread() {
            continue;
        }
        rows.push(ThreadRow {
            title: root_thread.title().unwrap_or_else(|| "New Thread".into()),
            state: thread_state(root_thread, view.root_thread_has_pending_tool_call(cx)),
            conversation_view: conversation_view.downgrade(),
            subagent_session: None,
        });

        for entry in root_thread.entries() {
            let AgentThreadEntry::ToolCall(tool_call) = entry else {
                continue;
            };
            let Some(subagent) = &tool_call.subagent_session_info else {
                continue;
            };
            let label: SharedString = tool_call.label.read(cx).source().clone();
            let (title, state) = match view.thread_view(&subagent.session_id) {
                Some(subagent_view) => {
                    let thread = subagent_view.read(cx).thread.read(cx);
                    (
                        thread.title().unwrap_or(label),
                        thread_state(thread, thread.is_waiting_for_confirmation()),
                    )
                }
                // The subagent's thread is only loaded once the conversation shows it.
                None => (label, tool_call_state(tool_call.status())),
            };
            rows.push(ThreadRow {
                title,
                state,
                conversation_view: conversation_view.downgrade(),
                subagent_session: Some(subagent.session_id.clone()),
            });
        }
    }
    rows
}

fn thread_state(thread: &AcpThread, waiting_for_confirmation: bool) -> ThreadState {
    if waiting_for_confirmation {
        ThreadState::WaitingForConfirmation
    } else if thread.had_error() {
        ThreadState::Error
    } else if thread.status() == ThreadStatus::Generating {
        ThreadState::Running
    } else {
        ThreadState::Done
    }
}

fn tool_call_state(status: ToolCallStatus) -> ThreadState {
    match status {
        ToolCallStatus::Pending | ToolCallStatus::InProgress => ThreadState::Running,
        ToolCallStatus::WaitingForConfirmation => ThreadState::WaitingForConfirmation,
        ToolCallStatus::Failed | ToolCallStatus::Rejected => ThreadState::Error,
        ToolCallStatus::Completed | ToolCallStatus::Canceled => ThreadState::Done,
    }
}

fn terminal_rows(workspace: &Workspace, cx: &App) -> Vec<TerminalRow> {
    let mut rows = Vec::new();
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        for pane in panel.read(cx).panes() {
            for view in pane.read(cx).items_of_type::<TerminalView>() {
                rows.push(terminal_row(&view, true, cx));
            }
        }
    }
    for view in workspace.items_of_type::<TerminalView>(cx) {
        rows.push(terminal_row(&view, false, cx));
    }
    rows
}

fn terminal_row(view: &Entity<TerminalView>, in_terminal_panel: bool, cx: &App) -> TerminalRow {
    let terminal_view = view.read(cx);
    let terminal = terminal_view.terminal().read(cx);
    let title = terminal_view
        .custom_title()
        .filter(|title| !title.trim().is_empty())
        .map(|title| SharedString::from(title.to_string()))
        .unwrap_or_else(|| terminal.title(true).into());
    let state = match terminal.task() {
        Some(task) => match task.status {
            TaskStatus::Running => TerminalState::TaskRunning,
            TaskStatus::Completed { success: true } => TerminalState::TaskSucceeded,
            TaskStatus::Completed { success: false } => TerminalState::TaskFailed,
            TaskStatus::Unknown => TerminalState::TaskEnded,
        },
        None => match terminal.foreground_process_command_name() {
            Some(program) if !is_shell(&program) => TerminalState::Running(program.into()),
            _ => TerminalState::Idle,
        },
    };
    TerminalRow {
        title,
        state,
        view: view.downgrade(),
        in_terminal_panel,
    }
}

/// A plain shell reports no command state, so a terminal counts as busy when the program in
/// the foreground is something other than a shell waiting at its prompt.
fn is_shell(program: &str) -> bool {
    let program = program.to_ascii_lowercase();
    let program = program.strip_suffix(".exe").unwrap_or(&program);
    matches!(
        program,
        "pwsh" | "powershell" | "cmd" | "bash" | "zsh" | "fish" | "sh" | "nu" | "wsl" | "elvish"
    )
}

impl Render for MainPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border;
        let activity = Self::render_section(
            "main-page-activity",
            "Agents & Terminals",
            self.render_activity(),
            cx,
        );

        h_flex()
            .size_full()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.project_panel.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .border_l_1()
                    .border_color(border_color)
                    .child(self.working_tree_changes.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .border_l_1()
                    .border_color(border_color)
                    .child(activity),
            )
    }
}

impl Focusable for MainPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.project_panel.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for MainPage {}

impl Item for MainPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Main".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileTree))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn is_permanent(&self, _cx: &App) -> bool {
        true
    }

    fn set_nav_history(
        &mut self,
        history: ItemNavHistory,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.nav_history = Some(history);
    }

    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Go Back only returns to items that recorded themselves in the history when they were
        // left.
        if let Some(history) = self.nav_history.as_mut() {
            history.push(None::<()>, None, cx);
        }
    }
}
