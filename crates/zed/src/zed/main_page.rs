use std::{any::Any, sync::Arc, time::Duration};

use acp_thread::{AcpThread, AgentThreadEntry, ThreadStatus, ToolCallStatus};
use agent_client_protocol::schema::v1 as acp;
use agent_ui::{AgentPanel, ConversationView};
use git::repository::LogSource;
use git_ui::{
    git_graph::{self, GitGraph},
    working_tree_changes::WorkingTreeChanges,
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Subscription,
    Task, WeakEntity, Window, WindowHandle,
};
use project::{Project, git_store::GitStore};
use project_panel::ProjectPanel;
use quarry::{connection_store::ConnectionStore, panel::QuarryPanel};
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
/// with a commit form, and what every agent and terminal is doing, side by side. The git graph
/// and Quarry open in its place instead of in tabs of their own.
pub struct MainPage {
    workspace: WeakEntity<Workspace>,
    view: MainView,
    project_panel: Entity<ProjectPanel>,
    working_tree_changes: Entity<WorkingTreeChanges>,
    /// Kept while another view is shown, so coming back finds them as they were left.
    git_graph: Option<Entity<GitGraph>>,
    quarry: Option<Entity<QuarryPanel>>,
    activity: Vec<ProjectActivity>,
    nav_history: Option<ItemNavHistory>,
    _refresh_task: Task<()>,
    _project_panel_focus: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainView {
    Overview,
    GitGraph,
    Quarry,
}

impl MainView {
    /// The navigation history keeps one entry per item and row, so each view gets a row of its
    /// own for Go Back to return to it rather than drop it in favor of the latest one.
    fn history_row(self) -> u32 {
        match self {
            MainView::Overview => 0,
            MainView::GitGraph => 1,
            MainView::Quarry => 2,
        }
    }
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
            cx.new(|cx| WorkingTreeChanges::new(workspace.clone(), git_store, None, window, cx));
        let project_panel_focus_handle = project_panel.read(cx).focus_handle(cx);
        // The project panel is only on screen in the overview, so whatever focuses it while the
        // tab shows something else, like Reveal in Project Panel, means to see it.
        let project_panel_focus =
            cx.on_focus_in(&project_panel_focus_handle, window, |this, window, cx| {
                if this.view != MainView::Overview && this.is_active(cx) {
                    let back_to = this.view;
                    this.set_view(MainView::Overview, Some(back_to), window, cx);
                }
            });
        let refresh_task = cx.spawn(async move |this, cx| {
            while this.update(cx, |this, cx| this.refresh(cx)).is_ok() {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        Self {
            workspace,
            view: MainView::Overview,
            project_panel,
            working_tree_changes,
            git_graph: None,
            quarry: None,
            activity: Vec::new(),
            nav_history: None,
            _refresh_task: refresh_task,
            _project_panel_focus: project_panel_focus,
        }
    }

    fn is_active(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .is_some_and(|item| item.item_id() == cx.entity_id())
    }

    /// Switches what the tab shows. `back_to` is recorded in the navigation history, so Go Back
    /// returns to it.
    fn set_view(
        &mut self,
        view: MainView,
        back_to: Option<MainView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if view == self.view {
            return;
        }
        if let Some(back_to) = back_to.filter(|back_to| *back_to != view)
            && let Some(history) = self.nav_history.as_mut()
        {
            history.push(Some(back_to), Some(back_to.history_row()), cx);
        }
        self.leave_view(window, cx);
        self.view = view;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn leave_view(&self, window: &mut Window, cx: &mut Context<Self>) {
        // The graph saves the size of its split when it is left.
        if self.view == MainView::GitGraph
            && let Some(git_graph) = &self.git_graph
        {
            git_graph.update(cx, |git_graph, cx| git_graph.deactivated(window, cx));
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

/// Registered in place of the git graph's and Quarry's own handlers, which open them in tabs of
/// their own. Those are still used for a workspace without the Main tab.
pub fn register_actions(workspace: &mut Workspace) {
    workspace
        .register_action(|workspace, _: &git_graph::Open, window, cx| {
            show_git_graph(workspace, None, true, window, cx);
        })
        .register_action(|workspace, action: &git_graph::OpenAtCommit, window, cx| {
            show_git_graph(workspace, Some(action.sha.clone()), false, window, cx);
        })
        .register_action(|workspace, _: &quarry::panel::ToggleFocus, window, cx| {
            show_quarry(workspace, window, cx);
        });
}

fn show_git_graph(
    workspace: &mut Workspace,
    sha: Option<String>,
    toggle: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let main_page = workspace.items_of_type::<MainPage>(cx).next();
    if toggle
        && let Some(main_page) = &main_page
        && hide_if_shown(workspace, main_page, MainView::GitGraph, window, cx)
    {
        return;
    }
    let Some((repo_id, git_store)) = git_graph::active_repository_target(workspace, cx) else {
        return;
    };
    let Some(main_page) = main_page else {
        git_graph::open_or_reuse_graph(
            workspace,
            repo_id,
            git_store,
            LogSource::All,
            sha,
            window,
            cx,
        );
        return;
    };
    let workspace_handle = workspace.weak_handle();
    let graph = main_page.update(cx, |main_page, cx| {
        main_page
            .git_graph
            .get_or_insert_with(|| {
                cx.new(|cx| {
                    GitGraph::new(
                        repo_id,
                        git_store,
                        workspace_handle,
                        Some(LogSource::All),
                        window,
                        cx,
                    )
                })
            })
            .clone()
    });
    git_graph::retarget_project_graph(&graph, repo_id, sha, cx);
    show(workspace, &main_page, MainView::GitGraph, window, cx);
}

fn show_quarry(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(main_page) = workspace.items_of_type::<MainPage>(cx).next() else {
        quarry::panel::open(workspace, window, cx);
        return;
    };
    if hide_if_shown(workspace, &main_page, MainView::Quarry, window, cx) {
        return;
    }
    if main_page.read(cx).quarry.is_none() {
        let Some(store) = ConnectionStore::global(cx) else {
            log::error!("the Quarry connection store is unavailable");
            return;
        };
        let workspace_handle = workspace.weak_handle();
        main_page.update(cx, |main_page, cx| {
            main_page.quarry =
                Some(cx.new(|cx| QuarryPanel::new(workspace_handle, store, window, cx)));
        });
    }
    show(workspace, &main_page, MainView::Quarry, window, cx);
}

fn is_active_item(workspace: &Workspace, main_page: &Entity<MainPage>, cx: &App) -> bool {
    workspace
        .active_item(cx)
        .is_some_and(|item| item.item_id() == main_page.entity_id())
}

/// The shortcut that brings up a view in the Main tab takes it away again when the tab is
/// already showing it.
fn hide_if_shown(
    workspace: &mut Workspace,
    main_page: &Entity<MainPage>,
    view: MainView,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if main_page.read(cx).view != view || !is_active_item(workspace, main_page, cx) {
        return false;
    }
    show(workspace, main_page, MainView::Overview, window, cx);
    true
}

fn show(
    workspace: &mut Workspace,
    main_page: &Entity<MainPage>,
    view: MainView,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Go Back from a view opened over another tab lands on the overview, not on whatever the
    // Main tab was left showing.
    let back_to = if is_active_item(workspace, main_page, cx) {
        main_page.read(cx).view
    } else {
        MainView::Overview
    };
    // Activating first records the tab being left before the entry that leads back to the
    // overview, so Go Back reaches the overview first.
    workspace.activate_item(main_page, true, false, window, cx);
    main_page.update(cx, |main_page, cx| {
        main_page.set_view(view, Some(back_to), window, cx);
    });
    window.focus(&main_page.focus_handle(cx), cx);
}

impl MainPage {
    fn render_overview(&self, cx: &mut Context<Self>) -> AnyElement {
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
            .into_any_element()
    }
}

impl Render for MainPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match (self.view, &self.git_graph, &self.quarry) {
            (MainView::GitGraph, Some(git_graph), _) => git_graph.clone().into_any_element(),
            (MainView::Quarry, _, Some(quarry)) => quarry.clone().into_any_element(),
            _ => self.render_overview(cx),
        }
    }
}

impl Focusable for MainPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match (self.view, &self.git_graph, &self.quarry) {
            (MainView::GitGraph, Some(git_graph), _) => git_graph.focus_handle(cx),
            (MainView::Quarry, _, Some(quarry)) => quarry.focus_handle(cx),
            _ => self.project_panel.read(cx).focus_handle(cx),
        }
    }
}

impl EventEmitter<ItemEvent> for MainPage {}

impl Item for MainPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        match (self.view, &self.git_graph, &self.quarry) {
            (MainView::GitGraph, Some(git_graph), _) => {
                git_graph.read(cx).tab_content_text(detail, cx)
            }
            (MainView::Quarry, _, Some(_)) => "Quarry".into(),
            _ => "Main".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        let icon = match self.view {
            MainView::Overview => IconName::FileTree,
            MainView::GitGraph => IconName::GitGraph,
            MainView::Quarry => IconName::Server,
        };
        Some(Icon::new(icon))
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

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Go Back only returns to items that recorded themselves in the history when they were
        // left, and it comes back to the view the tab was showing.
        if let Some(history) = self.nav_history.as_mut() {
            history.push(Some(self.view), Some(self.view.history_row()), cx);
        }
        self.leave_view(window, cx);
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(view) = data.downcast_ref::<MainView>().copied() else {
            return false;
        };
        if view == self.view {
            return false;
        }
        self.set_view(view, None, window, cx);
        window.focus(&self.focus_handle(cx), cx);
        true
    }
}
