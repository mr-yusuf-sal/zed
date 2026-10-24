use askpass::AskPassDelegate;
use editor::Editor;
use git::{
    repository::{CommitOptions, RepoPath},
    status::FileStatus,
};
use git_ui_core::{askpass_modal::AskPassModal, notifications::show_error_toast};
use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, FocusHandle, Focusable, PromptLevel, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use project::{
    Project, ProjectPath,
    git_store::{GitStore, GitStoreEvent, Repository, RepositoryEvent, RepositoryId},
};
use ui::{DiffStat, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{git_status_icon, project_diff::ProjectDiff};

/// The uncommitted and staged changes of a project's repositories, with buttons to stage,
/// unstage and discard them, and a form to commit the staged ones and push.
pub struct WorkingTreeChanges {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    /// `None` lists every repository and commits to the active one.
    repository_id: Option<RepositoryId>,
    changes: Vec<RepositoryChanges>,
    commit_title_editor: Entity<Editor>,
    commit_description_editor: Entity<Editor>,
    pending_commit: Option<Task<()>>,
    focus_handle: FocusHandle,
    _git_store_subscription: Subscription,
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
    diff_stat: Option<(usize, usize)>,
    repo_path: RepoPath,
    project_path: Option<ProjectPath>,
}

impl WorkingTreeChanges {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        git_store: Entity<GitStore>,
        repository_id: Option<RepositoryId>,
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
        let git_store_subscription = cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::GraphEvent(..), _) => {}
            GitStoreEvent::ActiveRepositoryChanged(_)
            | GitStoreEvent::RepositoryUpdated(..)
            | GitStoreEvent::RepositoryAdded
            | GitStoreEvent::RepositoryRemoved(_) => this.refresh(cx),
            _ => {}
        });
        let mut this = Self {
            workspace,
            git_store,
            repository_id,
            changes: Vec::new(),
            commit_title_editor,
            commit_description_editor,
            pending_commit: None,
            focus_handle: cx.focus_handle(),
            _git_store_subscription: git_store_subscription,
        };
        this.refresh(cx);
        this
    }

    pub fn set_repository(&mut self, repository_id: Option<RepositoryId>, cx: &mut Context<Self>) {
        if self.repository_id != repository_id {
            self.repository_id = repository_id;
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // The branch shown above the commit form can change without any file changing, so this
        // notifies even when the lists stay the same.
        self.changes = self.collect_changes(cx);
        cx.notify();
    }

    fn collect_changes(&self, cx: &App) -> Vec<RepositoryChanges> {
        let mut repositories = self
            .git_store
            .read(cx)
            .repositories()
            .iter()
            .filter(|(id, _)| self.repository_id.is_none_or(|wanted| wanted == **id))
            .map(|(_, repository)| repository.clone())
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
                        diff_stat: entry
                            .diff_stat
                            .map(|stat| (stat.added as usize, stat.deleted as usize)),
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

    fn target_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        let git_store = self.git_store.read(cx);
        match self.repository_id {
            Some(repository_id) => git_store.repositories().get(&repository_id).cloned(),
            None => git_store.active_repository(),
        }
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
        let diff_repository = repository.repository.clone();
        let repo_path = file.repo_path.clone();
        let (id, stat_id, icon, tooltip) = if staged {
            (
                "staged-change",
                "staged-change-stat",
                IconName::Dash,
                "Unstage File",
            )
        } else {
            (
                "unstaged-change",
                "unstaged-change-stat",
                IconName::Plus,
                "Stage File",
            )
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
                h_flex()
                    .gap_1()
                    .children(file.diff_stat.map(|(added, deleted)| {
                        DiffStat::new((stat_id, row_index), added, deleted)
                    }))
                    .when(!staged, |this| {
                        let repository_handle = repository_handle.clone();
                        let file = file.clone();
                        this.child(
                            IconButton::new(("discard-change", row_index), IconName::Undo)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(if file.status.is_created() {
                                    "Trash File"
                                } else {
                                    "Discard Changes"
                                }))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.discard(
                                        &repository_handle,
                                        vec![file.clone()],
                                        window,
                                        cx,
                                    );
                                })),
                        )
                    })
                    .child(
                        IconButton::new((id, row_index), icon)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(tooltip))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.set_staged(
                                    &repository_handle,
                                    vec![repo_path.clone()],
                                    !staged,
                                    cx,
                                );
                            })),
                    ),
            )
            .on_click(move |_, window, cx| {
                let workspace = workspace.clone();
                let project_path = project_path.clone();
                let diff_repository = diff_repository.clone();
                // Opening the diff deactivates the tab showing this list, which updates it, and
                // the click is handled while that tab may still be updating.
                window.defer(cx, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| match project_path {
                            Some(project_path) => ProjectDiff::deploy_at_project_path(
                                workspace,
                                project_path,
                                window,
                                cx,
                            ),
                            None => {
                                ProjectDiff::deploy_at(workspace, None, window, cx);
                                // `deploy_at` shows the active repository, which may not be the
                                // one this file belongs to.
                                let project_diff =
                                    workspace.items_of_type::<ProjectDiff>(cx).next();
                                if let Some(repository) = diff_repository.upgrade()
                                    && let Some(project_diff) = project_diff
                                {
                                    project_diff.update(cx, |project_diff, cx| {
                                        project_diff.set_repo(Some(repository), cx);
                                    });
                                }
                            }
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

    /// Puts tracked files back to HEAD, staged changes included, and trashes new ones.
    fn discard(
        &mut self,
        repository: &WeakEntity<Repository>,
        files: Vec<ChangedFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = repository.upgrade() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        if files.is_empty() {
            return;
        }

        let mut detail = files
            .iter()
            .take(5)
            .map(|file| file.file_name.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if files.len() > 5 {
            detail.push_str(&format!("\nand {} more…", files.len() - 5));
        }
        let (created, tracked): (Vec<_>, Vec<_>) =
            files.into_iter().partition(|file| file.status.is_created());
        let (message, confirm) = match (tracked.as_slice(), created.as_slice()) {
            ([file], []) => (format!("Discard changes to {}?", file.file_name), "Discard"),
            ([], [file]) => (format!("Trash {}?", file.file_name), "Trash"),
            (tracked, []) => (
                format!("Discard changes to {} files?", tracked.len()),
                "Discard",
            ),
            ([], created) => (format!("Trash {} files?", created.len()), "Trash"),
            (tracked, created) => (
                format!(
                    "Discard changes to {} files and trash {} files?",
                    tracked.len(),
                    created.len()
                ),
                "Discard and Trash",
            ),
        };
        let detail = (tracked.len() + created.len() > 1).then_some(detail);
        let prompt = window.prompt(
            PromptLevel::Warning,
            &message,
            detail.as_deref(),
            &[confirm, "Cancel"],
            cx,
        );

        let project = workspace.read(cx).project().clone();
        let workspace = workspace.downgrade();
        cx.spawn(async move |_, cx| {
            if prompt.await.ok() != Some(0) {
                return;
            }
            if let Err(error) = discard_files(repository, project, tracked, created, cx).await {
                cx.update(|cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        show_error_toast(workspace, "checkout", error, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_commit.is_some() {
            return;
        }
        let Some(repository) = self.target_repository(cx) else {
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
        let repository = self.target_repository(cx);
        let branch_name = repository.as_ref().and_then(|repository| {
            repository
                .read(cx)
                .branch
                .as_ref()
                .map(|branch| SharedString::from(branch.name().to_string()))
        });
        // `git::Push` pushes the git panel's repository, which is the active one.
        let can_push = repository.as_ref().is_some_and(|repository| {
            self.git_store
                .read(cx)
                .active_repository()
                .is_some_and(|active| active.read(cx).id == repository.read(cx).id)
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
                        Button::new("push", "Push")
                            .start_icon(Icon::new(IconName::ArrowUp).size(IconSize::Small))
                            .disabled(!can_push)
                            .when(repository.is_some() && !can_push, |button| {
                                button.tooltip(Tooltip::text(
                                    "Switch the active repository to this one to push it",
                                ))
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                // The git panel owns pushing: it picks the remote, asks for
                                // credentials and reports the result.
                                this.focus_handle.dispatch_action(&git::Push, window, cx);
                            })),
                    )
                    .child(
                        Button::new("commit", "Commit")
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

async fn discard_files(
    repository: Entity<Repository>,
    project: Entity<Project>,
    tracked: Vec<ChangedFile>,
    created: Vec<ChangedFile>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    if !tracked.is_empty() {
        let open_buffers = project.read_with(cx, |project, cx| {
            tracked
                .iter()
                .filter_map(|file| project.get_open_buffer(file.project_path.as_ref()?, cx))
                .collect::<Vec<_>>()
        });
        let repo_paths = tracked.iter().map(|file| file.repo_path.clone()).collect();
        repository
            .update(cx, |repository, cx| {
                repository.checkout_files("HEAD", repo_paths, cx)
            })
            .await?;
        // A buffer with unsaved edits keeps them over the restored file, and saving it would
        // bring the discarded changes back.
        let reloads = cx.update(|cx| {
            open_buffers
                .iter()
                .filter_map(|buffer| {
                    buffer.update(cx, |buffer, cx| {
                        buffer.is_dirty().then(|| buffer.reload(cx))
                    })
                })
                .collect::<Vec<_>>()
        });
        futures::future::join_all(reloads).await;
    }

    if !created.is_empty() {
        let staged = created
            .iter()
            .filter(|file| file.status.staging().has_staged())
            .map(|file| file.repo_path.clone())
            .collect::<Vec<_>>();
        if !staged.is_empty() {
            repository
                .update(cx, |repository, cx| repository.unstage_entries(staged, cx))
                .await?;
        }
        let trash_tasks = project.update(cx, |project, cx| {
            created
                .iter()
                .filter_map(|file| project.trash_file(file.project_path.clone()?, cx))
                .collect::<Vec<_>>()
        });
        for result in futures::future::join_all(trash_tasks).await {
            result?;
        }
    }
    Ok(())
}

fn render_placeholder(text: &'static str) -> AnyElement {
    div()
        .p_2()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
        .into_any_element()
}

fn render_repository_header(
    repository_index: usize,
    staged: bool,
    repository: &RepositoryChanges,
    files: &[ChangedFile],
    cx: &mut Context<WorkingTreeChanges>,
) -> AnyElement {
    let repository_handle = repository.repository.clone();
    let repo_paths = files
        .iter()
        .map(|file| file.repo_path.clone())
        .collect::<Vec<_>>();
    let (id, label) = if staged {
        ("unstage-all", "Unstage All")
    } else {
        ("stage-all", "Stage All")
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
            h_flex()
                .when(!staged, |this| {
                    let repository_handle = repository_handle.clone();
                    let files = files.to_vec();
                    this.child(
                        Button::new(("discard-all", repository_index), "Discard All")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.discard(&repository_handle, files.clone(), window, cx);
                            })),
                    )
                })
                .child(
                    Button::new((id, repository_index), label)
                        .label_size(LabelSize::XSmall)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_staged(&repository_handle, repo_paths.clone(), !staged, cx);
                        })),
                ),
        )
        .into_any_element()
}

impl Focusable for WorkingTreeChanges {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkingTreeChanges {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let unstaged_changes = Self::render_section(
            "working-tree-unstaged-changes",
            "Uncommitted Changes",
            self.render_changes(false, cx),
            cx,
        );
        let staged_changes = Self::render_section(
            "working-tree-staged-changes",
            "Staged Changes",
            self.render_changes(true, cx),
            cx,
        );
        v_flex()
            .track_focus(&self.focus_handle)
            // Enter in the commit title commits, instead of reaching the view this list is in.
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.commit(window, cx);
            }))
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
            .child(self.render_commit_form(cx))
    }
}
