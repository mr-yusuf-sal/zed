use std::time::Duration;

use acp_thread::{AcpThread, AgentThreadEntry, ThreadStatus, ToolCallStatus};
use agent_client_protocol::schema::v1 as acp;
use agent_ui::{AgentPanel, ConversationView};
use askpass::AskPassDelegate;
use editor::Editor;
use git::{
    repository::{CommitOptions, RepoPath},
    status::FileStatus,
};
use git_ui::{git_status_icon, project_diff::ProjectDiff};
use git_ui_core::{askpass_modal::AskPassModal, notifications::show_error_toast};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, PromptLevel, Render,
    SharedString, Task, WeakEntity, Window, WindowHandle, canvas, px,
};
use project::{Project, ProjectPath, git_store::Repository};
use project_panel::ProjectPanel;
use terminal::TaskStatus;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    ItemNavHistory, MultiWorkspace, Workspace,
    item::{Item, ItemEvent},
};

/// The statuses come from entities spread over every window, which appear and go away on their
/// own, so they are polled instead of tracked through subscriptions.
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const THREE_COLUMN_MIN_WIDTH: Pixels = px(1100.);

/// The permanent "Main" tab: what every agent and terminal is doing, the project's uncommitted
/// and staged changes with a commit form, and the project panel, side by side.
pub struct MainPage {
    workspace: WeakEntity<Workspace>,
    project_panel: Entity<ProjectPanel>,
    activity: Vec<ProjectActivity>,
    changes: Vec<RepositoryChanges>,
    commit_title_editor: Entity<Editor>,
    commit_description_editor: Entity<Editor>,
    pending_commit: Option<Task<()>>,
    three_columns: bool,
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

#[derive(PartialEq)]
struct RepositoryChanges {
    name: SharedString,
    repository: WeakEntity<Repository>,
    unstaged: Vec<ChangedFile>,
    staged: Vec<ChangedFile>,
}

impl RepositoryChanges {
    fn files(&self, staged: bool) -> &[ChangedFile] {
        if staged { &self.staged } else { &self.unstaged }
    }
}

#[derive(Clone, PartialEq)]
struct ChangedFile {
    file_name: SharedString,
    directory: Option<SharedString>,
    status: FileStatus,
    repo_path: RepoPath,
    project_path: Option<ProjectPath>,
}

impl MainPage {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project_panel: Entity<ProjectPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let commit_title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Commit title", window, cx);
            editor
        });
        let commit_description_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 8, window, cx);
            editor.set_placeholder_text("Description (optional)", window, cx);
            editor
        });
        let refresh_task = cx.spawn(async move |this, cx| {
            while this.update(cx, |this, cx| this.refresh(cx)).is_ok() {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
            }
        });
        Self {
            workspace,
            project_panel,
            activity: Vec::new(),
            changes: Vec::new(),
            commit_title_editor,
            commit_description_editor,
            pending_commit: None,
            three_columns: false,
            nav_history: None,
            _refresh_task: refresh_task,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let activity = collect_activity(cx);
        let changes = self
            .workspace
            .upgrade()
            .map(|workspace| collect_changes(workspace.read(cx).project(), cx))
            .unwrap_or_default();
        if activity != self.activity || changes != self.changes {
            self.activity = activity;
            self.changes = changes;
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

    fn render_changes(&self, staged: bool, cx: &mut Context<Self>) -> AnyElement {
        if self.changes.is_empty() {
            return render_placeholder("This project has no git repository.");
        }
        if self
            .changes
            .iter()
            .all(|repository| repository.files(staged).is_empty())
        {
            return render_placeholder(if staged {
                "No staged changes."
            } else {
                "No uncommitted changes."
            });
        }

        let mut row_index = 0;
        let mut repositories = Vec::new();
        for (repository_index, repository) in self.changes.iter().enumerate() {
            let files = repository.files(staged);
            if files.is_empty() {
                continue;
            }
            let mut rows = Vec::new();
            for file in files {
                row_index += 1;
                rows.push(self.render_changed_file(row_index, staged, repository, file, cx));
            }
            repositories.push(
                v_flex()
                    .child(render_repository_header(
                        repository_index,
                        staged,
                        repository,
                        files,
                        cx,
                    ))
                    .children(rows),
            );
        }
        v_flex().gap_2().children(repositories).into_any_element()
    }

    fn render_changed_file(
        &self,
        row_index: usize,
        staged: bool,
        repository: &RepositoryChanges,
        file: &ChangedFile,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = self.workspace.clone();
        let project_path = file.project_path.clone();
        let repository_handle = repository.repository.clone();
        let repo_path = file.repo_path.clone();
        let (id, icon, tooltip) = if staged {
            ("main-page-staged-change", IconName::Dash, "Unstage File")
        } else {
            ("main-page-change", IconName::Plus, "Stage File")
        };
        ListItem::new((id, row_index))
            .spacing(ListItemSpacing::Dense)
            .start_slot(git_status_icon(file.status))
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(Label::new(file.file_name.clone()).size(LabelSize::Small))
                    .children(file.directory.clone().map(|directory| {
                        Label::new(directory)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .end_slot(
                IconButton::new((id, row_index), icon)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.set_staged(&repository_handle, vec![repo_path.clone()], !staged, cx);
                    })),
            )
            .on_click(move |_, window, cx| {
                let workspace = workspace.clone();
                let project_path = project_path.clone();
                // Opening the diff deactivates this tab, which updates this page, and the click
                // is handled while the page may still be updating.
                window.defer(cx, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| match project_path {
                            Some(project_path) => ProjectDiff::deploy_at_project_path(
                                workspace,
                                project_path,
                                window,
                                cx,
                            ),
                            None => ProjectDiff::deploy_at(workspace, None, window, cx),
                        })
                        .log_err();
                });
            })
            .into_any_element()
    }

    fn set_staged(
        &mut self,
        repository: &WeakEntity<Repository>,
        repo_paths: Vec<RepoPath>,
        stage: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = repository.upgrade() else {
            return;
        };
        let task = repository.update(cx, |repository, cx| {
            if stage {
                repository.stage_entries(repo_paths, cx)
            } else {
                repository.unstage_entries(repo_paths, cx)
            }
        });
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            if let Err(error) = task.await {
                cx.update(|cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        let action = if stage { "add" } else { "reset" };
                        show_error_toast(workspace, action, error, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn active_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        let workspace = self.workspace.upgrade()?;
        workspace.read(cx).project().read(cx).active_repository(cx)
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_commit.is_some() {
            return;
        }
        let Some(repository) = self.active_repository(cx) else {
            return;
        };
        let title = self.commit_title_editor.read(cx).text(cx);
        let title = title.trim();
        if title.is_empty() {
            self.commit_title_editor.focus_handle(cx).focus(window, cx);
            return;
        }
        let has_staged_changes = repository
            .read(cx)
            .cached_status()
            .any(|entry| entry.status.staging().has_staged());
        if !has_staged_changes {
            let prompt = window.prompt(
                PromptLevel::Warning,
                "No staged changes to commit",
                None,
                &["OK"],
                cx,
            );
            cx.spawn(async move |_, _| {
                prompt.await.log_err();
            })
            .detach();
            return;
        }

        let description = self.commit_description_editor.read(cx).text(cx);
        let description = description.trim();
        let message = if description.is_empty() {
            title.to_string()
        } else {
            format!("{title}\n\n{description}")
        };
        let askpass = self.askpass_delegate("git commit", window, cx);
        let commit = repository.update(cx, |repository, cx| {
            repository.commit(message.into(), None, CommitOptions::default(), askpass, cx)
        });
        self.pending_commit = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match commit.await {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            };
            this.update_in(cx, |this, window, cx| {
                this.pending_commit = None;
                match result {
                    Ok(()) => {
                        this.commit_title_editor
                            .update(cx, |editor, cx| editor.clear(window, cx));
                        this.commit_description_editor
                            .update(cx, |editor, cx| editor.clear(window, cx));
                    }
                    Err(error) => {
                        if let Some(workspace) = this.workspace.upgrade() {
                            show_error_toast(workspace, "commit", error, cx);
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn askpass_delegate(
        &self,
        operation: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AskPassDelegate {
        let workspace = self.workspace.clone();
        let window = window.window_handle();
        AskPassDelegate::new_with_cancellation(
            &mut cx.to_async(),
            move |prompt, tx, cancellation, cx| {
                window
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                AskPassModal::new(
                                    operation.into(),
                                    prompt.into(),
                                    tx,
                                    cancellation,
                                    window,
                                    cx,
                                )
                            });
                        })
                    })
                    .log_err();
            },
        )
    }

    fn render_commit_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let border_color = cx.theme().colors().border_variant;
        let editor_background = cx.theme().colors().editor_background;
        let repository = self.active_repository(cx);
        let branch_name = repository.as_ref().and_then(|repository| {
            repository
                .read(cx)
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        });
        let editor_container = |editor: Entity<Editor>| {
            div()
                .px_1p5()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(border_color)
                .bg(editor_background)
                .child(editor)
        };
        let committing = self.pending_commit.is_some();
        v_flex()
            .p_2()
            .gap_1p5()
            .border_t_1()
            .border_color(border_color)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Label::new("Commit")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .children(branch_name.map(|branch_name| {
                        Label::new(branch_name)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .child(editor_container(self.commit_title_editor.clone()))
            .child(editor_container(self.commit_description_editor.clone()))
            .child(
                h_flex()
                    .gap_1()
                    .justify_end()
                    .child(
                        Button::new("main-page-push", "Push")
                            .start_icon(Icon::new(IconName::ArrowUp).size(IconSize::Small))
                            .disabled(repository.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                // The git panel owns pushing: it picks the remote, asks for
                                // credentials and reports the result.
                                this.project_panel.focus_handle(cx).dispatch_action(
                                    &git::Push,
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("main-page-commit", "Commit")
                            .style(ButtonStyle::Filled)
                            .start_icon(Icon::new(IconName::Check).size(IconSize::Small))
                            .loading(committing)
                            .disabled(repository.is_none() || committing)
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    ),
            )
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

fn render_repository_header(
    repository_index: usize,
    staged: bool,
    repository: &RepositoryChanges,
    files: &[ChangedFile],
    cx: &mut Context<MainPage>,
) -> AnyElement {
    let repository_handle = repository.repository.clone();
    let repo_paths = files
        .iter()
        .map(|file| file.repo_path.clone())
        .collect::<Vec<_>>();
    let (id, label) = if staged {
        ("main-page-unstage-all", "Unstage All")
    } else {
        ("main-page-stage-all", "Stage All")
    };
    h_flex()
        .pl_2()
        .pr_1()
        .pt_1()
        .justify_between()
        .child(
            Label::new(format!("{} ({})", repository.name, files.len()))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Button::new((id, repository_index), label)
                .label_size(LabelSize::XSmall)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_staged(&repository_handle, repo_paths.clone(), !staged, cx);
                })),
        )
        .into_any_element()
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

fn collect_changes(project: &Entity<Project>, cx: &App) -> Vec<RepositoryChanges> {
    let mut repositories = project
        .read(cx)
        .repositories(cx)
        .values()
        .cloned()
        .collect::<Vec<_>>();
    repositories.sort_by_key(|repository| repository.read(cx).work_directory_abs_path.clone());

    repositories
        .iter()
        .map(|repository_handle| {
            let repository = repository_handle.read(cx);
            let name = repository
                .work_directory_abs_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| repository.work_directory_abs_path.display().to_string());
            let files = repository
                .cached_status()
                .filter(|entry| entry.status.has_changes())
                .map(|entry| ChangedFile {
                    file_name: entry
                        .repo_path
                        .file_name()
                        .map(|name| name.to_string())
                        .unwrap_or_else(|| entry.repo_path.as_unix_str().to_string())
                        .into(),
                    directory: entry
                        .repo_path
                        .parent()
                        .map(|parent| parent.as_unix_str().to_string())
                        .filter(|parent| !parent.is_empty())
                        .map(SharedString::from),
                    status: entry.status,
                    repo_path: entry.repo_path.clone(),
                    project_path: repository.repo_path_to_project_path(&entry.repo_path, cx),
                })
                .collect::<Vec<_>>();
            // A partially staged file belongs to both lists.
            let staged = files
                .iter()
                .filter(|file| file.status.staging().has_staged())
                .cloned()
                .collect();
            let unstaged = files
                .into_iter()
                .filter(|file| file.status.staging().has_unstaged())
                .collect();
            RepositoryChanges {
                name: name.into(),
                repository: repository_handle.downgrade(),
                unstaged,
                staged,
            }
        })
        .collect()
}

impl Render for MainPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let measure = canvas(
            move |bounds, _, cx| {
                let three_columns = bounds.size.width >= THREE_COLUMN_MIN_WIDTH;
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        if this.three_columns != three_columns {
                            this.three_columns = three_columns;
                            cx.notify();
                        }
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        let border_color = cx.theme().colors().border;
        let activity = Self::render_section(
            "main-page-activity",
            "Agents & Terminals",
            self.render_activity(),
            cx,
        );
        let unstaged_changes = Self::render_section(
            "main-page-changes",
            "Uncommitted Changes",
            self.render_changes(false, cx),
            cx,
        );
        let staged_changes = Self::render_section(
            "main-page-staged-changes",
            "Staged Changes",
            self.render_changes(true, cx),
            cx,
        );
        let changes = v_flex()
            .size_full()
            .child(div().flex_1().min_h_0().child(unstaged_changes))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(staged_changes),
            )
            .child(self.render_commit_form(cx));

        h_flex()
            .relative()
            .size_full()
            .child(measure)
            .map(|this| {
                if self.three_columns {
                    this.child(div().flex_1().min_w_0().h_full().child(activity))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .border_l_1()
                                .border_color(border_color)
                                .child(changes),
                        )
                } else {
                    this.child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(div().flex_1().min_h_0().child(activity))
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .border_t_1()
                                    .border_color(border_color)
                                    .child(changes),
                            ),
                    )
                }
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .border_l_1()
                    .border_color(border_color)
                    .child(self.project_panel.clone()),
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
