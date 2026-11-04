use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::HashMap;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer,
    SplittableEditor, hover_markdown_style, multibuffer_context_lines,
};
use futures::StreamExt as _;
use futures_lite::future::yield_now;
use git::repository::{CommitDetails, LogOrder, LogSource, RepoPath};
use git::status::{FileStatus, StatusCode, TrackedStatus};
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, ParsedGitRemote,
    parse_git_remote_url,
};
use gpui::{
    AnyElement, App, AppContext as _, AsyncWindowContext, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    PromptLevel, Render, ScrollHandle, StatefulInteractiveElement as _, Styled, Subscription, Task,
    WeakEntity, Window, actions,
};
use language::{
    Buffer, Capability, DiskState, File, LanguageRegistry, LineEnding, OffsetRangeExt as _,
    ReplicaId, Rope, TextBuffer,
};
use markdown::{Markdown, MarkdownElement};
use multi_buffer::PathKey;
use project::{
    Project, ProjectPath, WorktreeId,
    git_store::{
        CommitDiff, CommitFile, GitGraphEvent, Repository, RepositoryEvent, UnshallowState,
    },
};
use settings::{DiffViewStyle, Settings};
use std::{
    any::{Any, TypeId},
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
};
use theme::ActiveTheme;
use ui::{ContextMenu, DiffStat, Disclosure, Divider, Tooltip, WithScrollbar, prelude::*};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath, truncate_and_trailoff};
use workspace::item::TabTooltipContent;
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace, item::ItemEvent, notifications::NotifyTaskExt, pane::SaveIntent,
    searchable::SearchableItemHandle,
};

use crate::commit_tooltip::CommitAvatar;
use crate::git_panel::GitPanel;

actions!(
    git,
    [
        ApplyCurrentStash,
        PopCurrentStash,
        DropCurrentStash,
        OpenFileAtHead,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ApplyCurrentStash, window, cx| {
            CommitView::apply_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &DropCurrentStash, window, cx| {
            CommitView::remove_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &PopCurrentStash, window, cx| {
            CommitView::pop_stash(workspace, window, cx);
        });
    })
    .detach();
}

pub struct CommitView {
    commit: CommitDetails,
    /// Set when the view compares two commits: it then shows what changed from
    /// this commit to `commit` instead of what `commit` changed against its parent.
    older_commit: Option<CommitDetails>,
    editor: Entity<SplittableEditor>,
    message: Entity<Markdown>,
    message_expanded: bool,
    message_scroll_handle: ScrollHandle,
    stash: Option<usize>,
    multibuffer: Entity<MultiBuffer>,
    repository: Entity<Repository>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    remote: Option<GitRemote>,
    is_shallow_boundary: bool,
    file_filter: Option<RepoPath>,
    _load_diff_task: Task<Result<()>>,
}

pub(crate) struct GitBlob {
    pub(crate) path: RepoPath,
    pub(crate) worktree_id: WorktreeId,
    pub(crate) is_deleted: bool,
    pub(crate) is_binary: bool,
    pub(crate) display_name: String,
}

pub(crate) fn worktree_id_for_repo_path(
    repository: &Repository,
    project: &Project,
    path: &RepoPath,
    cx: &App,
) -> Option<WorktreeId> {
    repository
        .repo_path_to_project_path(path, cx)
        .map(|project_path| project_path.worktree_id)
        .or_else(|| {
            let (worktree, _) = project.find_worktree(&repository.work_directory_abs_path, cx)?;
            Some(worktree.read(cx).id())
        })
        .or_else(|| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
        })
}

struct CommitDiffAddon {
    file_statuses: HashMap<language::BufferId, FileStatus>,
    commit_view: WeakEntity<CommitView>,
}

impl Addon for CommitDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        _cx: &App,
    ) -> Option<FileStatus> {
        self.file_statuses.get(&buffer_id).copied()
    }

    fn extend_buffer_header_context_menu(
        &self,
        menu: ContextMenu,
        buffer: &language::BufferSnapshot,
        _window: &mut Window,
        cx: &mut App,
    ) -> ContextMenu {
        let file_to_open = buffer.file().and_then(|file| {
            let commit_view = self.commit_view.upgrade()?;
            let commit_view = commit_view.read(cx);
            let project_path = commit_view
                .repository
                .read(cx)
                .repo_path_to_project_path(&RepoPath::from_rel_path(file.path()), cx)?;
            let exists_at_head = commit_view
                .workspace
                .upgrade()?
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&project_path, cx)
                .is_some();
            exists_at_head.then(|| file.clone())
        });

        menu.when_some(file_to_open, |menu, file| {
            let commit_view = self.commit_view.clone();
            menu.entry(
                "Open File in Project",
                Some(Box::new(OpenFileAtHead)),
                move |window, cx| {
                    commit_view
                        .update(cx, |view, cx| view.open_file_at_head(&file, window, cx))
                        .log_err();
                },
            )
        })
    }
}

const FILE_NAMESPACE_SORT_PREFIX: u64 = 1;

impl CommitView {
    pub fn open(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_with_options(
            commit_sha,
            None,
            repo,
            workspace,
            stash,
            file_filter,
            false,
            None,
            window,
            cx,
        )
    }

    /// Opens a view of everything that differs between the two commits' trees,
    /// whatever history lies between them.
    pub fn open_comparison(
        older_sha: String,
        newer_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_with_options(
            newer_sha,
            Some(older_sha),
            repo,
            workspace,
            None,
            None,
            true,
            None,
            window,
            cx,
        )
    }

    /// Opens the commit with `commit_diff`, the diff its caller already loaded
    /// through `Repository::load_commit_diff`, instead of reading every changed
    /// file's contents from git again.
    pub fn open_with_diff(
        commit_sha: String,
        commit_diff: CommitDiff,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_with_options(
            commit_sha,
            None,
            repo,
            workspace,
            None,
            file_filter,
            false,
            Some(commit_diff),
            window,
            cx,
        )
    }

    fn open_with_options(
        commit_sha: String,
        older_sha: Option<String>,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        ignore_shallow_boundary: bool,
        preloaded_diff: Option<CommitDiff>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let revision = match &older_sha {
            Some(older_sha) => format!("{older_sha}..{commit_sha}"),
            None => commit_sha.clone(),
        };
        let commit_diff = match preloaded_diff {
            Some(diff) => Some(Task::ready(Ok(diff))),
            None => repo
                .update(cx, |repo, cx| {
                    repo.load_commit_diff(revision.clone(), ignore_shallow_boundary, cx)
                })
                .ok(),
        };
        let commit_details = repo
            .update(cx, |repo, cx| repo.show_commit(commit_sha.clone(), cx))
            .ok();
        let older_commit_details = older_sha.map(|older_sha| {
            repo.update(cx, |repo, cx| repo.show_commit(older_sha, cx))
                .ok()
        });

        window
            .spawn(cx, async move |cx| {
                let commit_diff = commit_diff?;
                let commit_details = commit_details?;
                let (commit_diff, commit_details) = futures::join!(commit_diff, commit_details);
                let mut commit_diff = commit_diff.log_err()?;
                let commit_details = commit_details.log_err()?;
                let older_commit = match older_commit_details {
                    Some(older_commit_details) => Some(older_commit_details?.await.log_err()?),
                    None => None,
                };

                // Filter to specific file if requested
                if let Some(ref filter_path) = file_filter {
                    commit_diff.files.retain(|f| &f.path == filter_path);
                }

                let repo = repo.upgrade()?;

                workspace
                    .update_in(cx, |workspace, window, cx| {
                        let project = workspace.project();
                        let workspace_entity = cx.entity();
                        let workspace_handle = cx.weak_entity();
                        let commit_view = cx.new(|cx| {
                            CommitView::new(
                                commit_details,
                                older_commit,
                                commit_diff,
                                repo,
                                project.clone(),
                                workspace_entity,
                                workspace_handle,
                                stash,
                                file_filter,
                                window,
                                cx,
                            )
                        });

                        let pane = workspace.active_pane();
                        pane.update(cx, |pane, cx| {
                            let ix = pane.items().position(|item| {
                                let commit_view = item.downcast::<CommitView>();
                                commit_view.is_some_and(|view| view.read(cx).revision() == revision)
                            });
                            if let Some(ix) = ix {
                                let existing = pane
                                    .items()
                                    .filter_map(|item| item.downcast::<CommitView>())
                                    .find(|view| view.read(cx).revision() == revision)
                                    .unwrap();

                                pane.remove_item(existing.item_id(), false, false, window, cx);
                                pane.add_item(
                                    Box::new(commit_view),
                                    true,
                                    true,
                                    Some(ix),
                                    window,
                                    cx,
                                );
                            } else {
                                pane.add_item(Box::new(commit_view), true, true, None, window, cx);
                            }
                        })
                    })
                    .log_err()
            })
            .detach();
    }

    fn new(
        commit: CommitDetails,
        older_commit: Option<CommitDetails>,
        commit_diff: CommitDiff,
        repository: Entity<Repository>,
        project: Entity<Project>,
        workspace_entity: Entity<Workspace>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let is_shallow_boundary = commit_diff.is_shallow_boundary;
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let message = cx.new(|cx| {
            Markdown::new(
                commit.message.clone(),
                Some(language_registry.clone()),
                None,
                cx,
            )
        });

        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace_entity.clone(),
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);

            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
                editor.set_show_diff_review_button(true, cx);
            });

            editor
        });
        let short_revision = Arc::<str>::from(short_revision(&commit, older_commit.as_ref()));

        let repository_clone = repository.clone();
        let project_clone = project.clone();

        let weak_editor = editor.downgrade();

        let load_diff_task = cx.spawn_in(window, async move |this, cx| {
            let mut binary_buffer_ids: HashSet<language::BufferId> = HashSet::default();
            let mut file_statuses: HashMap<language::BufferId, FileStatus> = HashMap::default();

            for file in commit_diff.files {
                let loaded = add_commit_file(
                    &weak_editor,
                    file,
                    &short_revision,
                    None,
                    FILE_NAMESPACE_SORT_PREFIX,
                    &repository_clone,
                    &project_clone,
                    &language_registry,
                    cx,
                )
                .await?;
                file_statuses.insert(loaded.buffer_id, loaded.status);
                if loaded.is_binary {
                    binary_buffer_ids.insert(loaded.buffer_id);
                }
            }

            this.update(cx, |this, cx| {
                let commit_view = cx.weak_entity();
                this.editor.update(cx, |editor, cx| {
                    editor.rhs_editor().update(cx, |editor, _cx| {
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                });
                if !binary_buffer_ids.is_empty() {
                    this.editor.update(cx, |editor, cx| {
                        editor.rhs_editor().update(cx, |editor, cx| {
                            editor.fold_buffers(binary_buffer_ids, cx);
                        });
                    });
                }
            })?;

            anyhow::Ok(())
        });

        let snapshot = repository.read(cx).snapshot();
        let remote_url = snapshot
            .remote_upstream_url
            .as_ref()
            .or(snapshot.remote_origin_url.as_ref());

        let remote = remote_url.and_then(|url| {
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            parse_git_remote_url(provider_registry, url).map(|(host, parsed)| GitRemote {
                host,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        Self {
            commit,
            older_commit,
            editor,
            message,
            message_expanded: false,
            message_scroll_handle: ScrollHandle::new(),
            multibuffer,
            stash,
            repository,
            project,
            workspace,
            remote,
            is_shallow_boundary,
            file_filter,
            _load_diff_task: load_diff_task,
        }
    }

    fn render_shallow_boundary_notice(&self, cx: &App) -> impl IntoElement {
        let commit_sha = self.commit.sha.to_string();
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        let stash = self.stash;
        let file_filter = self.file_filter.clone();
        let unshallow_state = self.repository.read(cx).unshallow_state();
        let can_fetch = !self.project.read(cx).is_via_collab()
            && unshallow_state != UnshallowState::Unshallowed;
        let fetch_in_flight = unshallow_state == UnshallowState::InProgress;
        v_flex()
            .flex_grow(1.)
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Label::new("This commit is at the boundary of a shallow clone.")
                    .color(Color::Muted),
            )
            .child(
                Label::new(
                    "Its parent history was not fetched, so the changes it introduced cannot be shown.",
                )
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(can_fetch, |this| {
                        let commit_sha = commit_sha.clone();
                        let repository = repository.clone();
                        let workspace = workspace.clone();
                        let file_filter = file_filter.clone();
                        this.child(
                            Button::new(
                                "fetch-unshallow",
                                if fetch_in_flight {
                                    "Fetching…"
                                } else {
                                    "Fetch Missing History"
                                },
                            )
                                .style(ButtonStyle::Filled)
                                .disabled(fetch_in_flight)
                                .tooltip(Tooltip::text(
                                    "Run `git fetch --unshallow` to download the full history, then show this commit's changes.",
                                ))
                                .on_click(move |_, window, cx| {
                                    let fetch = crate::commit_tooltip::fetch_unshallow(
                                        repository.clone(),
                                        workspace.clone(),
                                        window,
                                        cx,
                                    );
                                    let commit_sha = commit_sha.clone();
                                    let repository = repository.downgrade();
                                    let workspace = workspace.clone();
                                    let file_filter = file_filter.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            fetch.await?;
                                            cx.update(|window, cx| {
                                                Self::open_with_options(
                                                    commit_sha,
                                                    None,
                                                    repository,
                                                    workspace,
                                                    stash,
                                                    file_filter,
                                                    false,
                                                    None,
                                                    window,
                                                    cx,
                                                )
                                            })
                                        })
                                        .detach_and_log_err(cx);
                                }),
                        )
                    })
                    .child(
                        Button::new(
                            "load-shallow-snapshot",
                            if file_filter.is_some() {
                                "Load File Snapshot"
                            } else {
                                "Load Full Snapshot"
                            },
                        )
                            .style(ButtonStyle::Outlined)
                            .tooltip(Tooltip::text(if file_filter.is_some() {
                                "Show this file's full contents at this commit as added."
                            } else {
                                "Show every file at this commit as added. This can be slow in large repositories."
                            }))
                            .on_click(move |_, window, cx| {
                                Self::open_with_options(
                                    commit_sha.clone(),
                                    None,
                                    repository.downgrade(),
                                    workspace.clone(),
                                    stash,
                                    file_filter.clone(),
                                    true,
                                    None,
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
    }

    fn render_commit_avatar(
        &self,
        sha: &SharedString,
        size: impl Into<gpui::AbsoluteLength>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        CommitAvatar::new(
            sha,
            Some(self.commit.author_email.clone()),
            self.remote.as_ref(),
        )
        .size(size)
        .render(window, cx)
    }

    fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    fn open_file_at_head(
        &mut self,
        file: &Arc<dyn language::File>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rel_path = file.path().clone();
        let worktree_id = file.worktree_id(cx);
        let repo_path = RepoPath::from_rel_path(&rel_path);
        let project_path = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&repo_path, cx)
            .unwrap_or(project::ProjectPath {
                worktree_id,
                path: rel_path,
            });

        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(project_path, None, false, false, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn open_file_at_head_action(
        &mut self,
        _: &OpenFileAtHead,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .editor
            .read(cx)
            .focused_editor()
            .read(cx)
            .active_buffer(cx)
            .and_then(|buffer| buffer.read(cx).file().cloned())
        else {
            return;
        };
        self.open_file_at_head(&file, window, cx);
    }

    /// What this view diffs, in the form `Repository::load_commit_diff` takes it.
    fn revision(&self) -> String {
        match &self.older_commit {
            Some(older_commit) => format!("{}..{}", older_commit.sha, self.commit.sha),
            None => self.commit.sha.to_string(),
        }
    }

    /// The width of the header's leading column, which matches the editor gutter so
    /// that the header's text lines up with the diff below it.
    fn header_leading_column_width(
        &self,
        content_width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Pixels {
        let gutter_width = self.editor.update(cx, |editor, cx| {
            let editor = editor.rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let style = editor.style(cx);
                let font_id = window.text_system().resolve_font(&style.text.font());
                let font_size = style.text.font_size.to_pixels(window.rem_size());
                snapshot
                    .gutter_dimensions(font_id, font_size, style, window, cx)
                    .full_width()
            })
        });
        let min_side_padding = rems_from_px(6_f32).to_pixels(window.rem_size());
        gutter_width.max(content_width + min_side_padding)
    }

    fn render_comparison_header(
        &self,
        older_commit: &CommitDetails,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let icon_size = IconSize::Medium;
        let leading_column_width = self.header_leading_column_width(
            icon_size.rems().to_pixels(window.rem_size()),
            window,
            cx,
        );
        let commit_row = |commit: &CommitDetails| {
            h_flex()
                .gap_1p5()
                .min_w_0()
                .child(
                    Label::new(short_sha(&commit.sha).to_string())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
                .child(
                    Label::new(commit_subject(&commit.message).to_string())
                        .size(LabelSize::Small)
                        .truncate(),
                )
        };

        h_flex()
            .w_full()
            .py_2p5()
            .pr_2p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .flex_none()
                    .w(leading_column_width)
                    .justify_center()
                    .child(
                        Icon::new(IconName::Diff)
                            .size(icon_size)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(Label::new(format!(
                        "Changes from {} to {}",
                        short_sha(&older_commit.sha),
                        short_sha(&self.commit.sha)
                    )))
                    .child(commit_row(older_commit))
                    .child(commit_row(&self.commit)),
            )
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let commit = &self.commit;
        let author_name = commit.author_name.clone();
        let author_email = commit.author_email.clone();
        let commit_sha = commit.sha.clone();
        let commit_date = time::OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let date_string = time_format::format_localized_timestamp(
            commit_date,
            time::OffsetDateTime::now_utc(),
            local_offset,
            time_format::TimestampFormat::MediumAbsolute,
        );

        let avatar_size = rems_from_px(40_f32);
        let avatar_container_width =
            self.header_leading_column_width(avatar_size.to_pixels(window.rem_size()), window, cx);

        let clipboard_has_sha = cx
            .read_from_clipboard()
            .and_then(|entry| entry.text())
            .map_or(false, |clipboard_text| {
                clipboard_text.trim() == commit_sha.as_ref()
            });

        let (copy_icon, copy_icon_color) = if clipboard_has_sha {
            (IconName::Check, Color::Success)
        } else {
            (IconName::Copy, Color::Muted)
        };

        let has_more = self.commit.message.trim().contains('\n');
        let is_expanded = self.message_expanded;
        let expand_tooltip = if is_expanded {
            "Fold Commit Description"
        } else {
            "Expand Commit Description"
        };

        v_flex()
            .w_full()
            .py_2p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .pr_2p5()
                    .w_full()
                    .flex_wrap()
                    .justify_between()
                    .child(
                        h_flex()
                            .child(
                                h_flex()
                                    .flex_none()
                                    .w(avatar_container_width)
                                    .justify_center()
                                    .child(self.render_commit_avatar(
                                        &commit.sha,
                                        avatar_size,
                                        window,
                                        cx,
                                    )),
                            )
                            .child(
                                v_flex()
                                    .child(h_flex().gap_1().child(Label::new(author_name)).when(
                                        has_more,
                                        |this| {
                                            this.child(
                                                Disclosure::new(
                                                    "commit-message-disclosure",
                                                    is_expanded,
                                                )
                                                .closed_icon(IconName::ExpandVertical)
                                                .opened_icon(IconName::FoldVertical)
                                                .tooltip(Tooltip::text(expand_tooltip))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.message_expanded = !this.message_expanded;
                                                    cx.notify();
                                                })),
                                            )
                                        },
                                    ))
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .child(
                                                Label::new(date_string)
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            )
                                            .child(
                                                Label::new("•")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .alpha(0.5),
                                            )
                                            .child(
                                                Label::new(author_email)
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            ),
                                    ),
                            ),
                    )
                    .when(self.stash.is_none(), |this| {
                        this.child(
                            Button::new("sha", "Commit SHA")
                                .start_icon(
                                    Icon::new(copy_icon)
                                        .size(IconSize::Small)
                                        .color(copy_icon_color),
                                )
                                .tooltip({
                                    let commit_sha = commit_sha.clone();
                                    move |_, cx| {
                                        Tooltip::with_meta(
                                            "Copy Commit SHA",
                                            None,
                                            commit_sha.clone(),
                                            cx,
                                        )
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        commit_sha.to_string(),
                                    ));
                                }),
                        )
                    }),
            )
            .children(self.render_commit_message(avatar_container_width, window, cx))
    }

    fn render_commit_message(
        &self,
        avatar_spacer: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let message = self.commit.message.trim();
        if message.is_empty() {
            return None;
        }

        let markdown_style = hover_markdown_style(window, cx);

        let is_expanded = self.message_expanded;

        let has_more = message.contains('\n');
        let collapsed = has_more && !is_expanded;
        let collapsed_height = window.line_height();
        let max_expanded_height = window.line_height() * 12.;

        Some(
            h_flex()
                .w_full()
                .pr_2p5()
                .child(h_flex().flex_none().w(avatar_spacer))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .id("commit-message")
                                .size_full()
                                .text_sm()
                                .when(collapsed, |this| this.h(collapsed_height).overflow_hidden())
                                .when(!collapsed, |this| {
                                    this.max_h(max_expanded_height)
                                        .overflow_y_scroll()
                                        .track_scroll(&self.message_scroll_handle)
                                })
                                .child(MarkdownElement::new(self.message.clone(), markdown_style)),
                        )
                        .vertical_scrollbar_for(&self.message_scroll_handle, window, cx),
                ),
        )
    }

    fn apply_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Apply",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, not applying"));
                    }
                    Ok(repo.stash_apply(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn pop_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Pop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, pop aborted"));
                    }
                    Ok(repo.stash_pop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn remove_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Drop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, drop aborted"));
                    }
                    Ok(repo.stash_drop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await??,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn stash_action<AsyncFn>(
        workspace: &mut Workspace,
        str_action: &str,
        window: &mut Window,
        cx: &mut App,
        callback: AsyncFn,
    ) where
        AsyncFn: AsyncFnOnce(
                Entity<Repository>,
                &SharedString,
                usize,
                Entity<CommitView>,
                WeakEntity<Workspace>,
                &mut AsyncWindowContext,
            ) -> anyhow::Result<()>
            + 'static,
    {
        let Some(commit_view) = workspace.active_item_as::<CommitView>(cx) else {
            return;
        };
        let Some(stash) = commit_view.read(cx).stash else {
            return;
        };
        let sha = commit_view.read(cx).commit.sha.clone();
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("{} stash@{{{}}}?", str_action, stash),
            None,
            &[str_action, "Cancel"],
            cx,
        );

        let workspace_weak = workspace.weak_handle();
        let commit_view_entity = commit_view;

        window
            .spawn(cx, async move |cx| {
                if answer.await != Ok(0) {
                    return anyhow::Ok(());
                }

                let Some(workspace) = workspace_weak.upgrade() else {
                    return Ok(());
                };

                let repo = workspace.update(cx, |workspace, cx| {
                    workspace
                        .panel::<GitPanel>(cx)
                        .and_then(|p| p.read(cx).active_repository.clone())
                });

                let Some(repo) = repo else {
                    return Ok(());
                };

                callback(repo, &sha, stash, commit_view_entity, workspace_weak, cx).await?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace.weak_handle(), window, cx);
    }

    async fn close_commit_view(
        commit_view: Entity<CommitView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> anyhow::Result<()> {
        workspace
            .update_in(cx, |workspace, window, cx| {
                let active_pane = workspace.active_pane();
                let commit_view_id = commit_view.entity_id();
                active_pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(commit_view_id, SaveIntent::Skip, window, cx)
                })
            })?
            .await?;
        anyhow::Ok(())
    }
}

impl language::File for GitBlob {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        self.path.as_ref()
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

pub(crate) async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<language::LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language =
        cx.update(|_, cx| language_registry.language_for_file(&blob, Some(&text), cx))?;
    let language = if let Some(language_id) = language {
        language_registry
            .load_language(language_id)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite, cx);
        buffer.set_language_registry(language_registry.clone());
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|_, cx| buffer.read(cx).language().cloned())?;
    let buffer = cx.update(|_, cx| buffer.read(cx).snapshot())?;

    let diff =
        cx.new(|cx| BufferDiff::new(&buffer.text, language, Some(language_registry.clone()), cx));

    diff.update(cx, |diff, cx| {
        diff.set_base_text(
            old_text.map(|old_text| Arc::from(old_text.as_str())),
            buffer.text.clone(),
            cx,
        )
    })
    .await;

    Ok(diff)
}

struct LoadedCommitFile {
    buffer_id: language::BufferId,
    status: FileStatus,
    is_binary: bool,
}

/// Builds the buffer and diff of one file a commit changed and adds its hunks to
/// the view under `sort_prefix`, which keeps the same path from different commits
/// in separate excerpts.
async fn add_commit_file(
    editor: &WeakEntity<SplittableEditor>,
    file: CommitFile,
    revision_label: &str,
    commit_subject: Option<&str>,
    sort_prefix: u64,
    repository: &Entity<Repository>,
    project: &Entity<Project>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<LoadedCommitFile> {
    let is_created = file.old_text.is_none();
    let is_deleted = file.new_text.is_none();
    let raw_new_text = file.new_text.unwrap_or_default();
    let raw_old_text = file.old_text;

    let is_binary = file.is_binary;

    let new_text = if is_binary {
        "(binary file not shown)".to_string()
    } else {
        raw_new_text
    };
    let old_text = if is_binary { None } else { raw_old_text };
    let worktree_id = repository
        .read_with(cx, |repository, cx| {
            worktree_id_for_repo_path(repository, project.read(cx), &file.path, cx)
        })
        .context("project has no worktrees")?;
    let file_name = file
        .path
        .file_name()
        .map(|name| name.to_string())
        .unwrap_or_else(|| file.path.display(PathStyle::local()).to_string());
    let display_name = match commit_subject {
        Some(subject) => format!("{revision_label} - {file_name} · {subject}"),
        None => format!("{revision_label} - {file_name}"),
    };
    let rel_path: &Arc<RelPath> = file.path.as_ref();
    let path = PathKey::with_sort_prefix(sort_prefix, rel_path.clone());

    let file = Arc::new(GitBlob {
        path: file.path.clone(),
        is_deleted,
        is_binary,
        worktree_id,
        display_name,
    }) as Arc<dyn language::File>;

    let buffer = build_buffer(new_text, file, language_registry, cx).await?;
    let buffer_id = cx.update(|_, cx| buffer.read(cx).remote_id())?;

    let status_code = if is_created {
        StatusCode::Added
    } else if is_deleted {
        StatusCode::Deleted
    } else {
        StatusCode::Modified
    };
    let status = FileStatus::Tracked(TrackedStatus {
        index_status: status_code,
        worktree_status: StatusCode::Unmodified,
    });

    let buffer_diff = if is_binary {
        cx.update(|_, cx| {
            let snapshot = buffer.read(cx).snapshot();
            cx.new(|cx| {
                BufferDiff::new_unchanged(
                    &snapshot,
                    snapshot.language().cloned(),
                    Some(language_registry.clone()),
                    cx,
                )
            })
        })?
    } else {
        build_buffer_diff(old_text, &buffer, language_registry, cx).await?
    };

    let excerpt_ranges = cx.update(|_, cx| {
        let snapshot = buffer.read(cx).snapshot();
        if is_binary {
            vec![language::Point::zero()..snapshot.max_point()]
        } else {
            let diff_snapshot = buffer_diff.read(cx).snapshot(cx);
            let mut hunks = diff_snapshot.hunks(&snapshot).peekable();
            if hunks.peek().is_none() {
                vec![language::Point::zero()..snapshot.max_point()]
            } else {
                hunks
                    .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                    .collect::<Vec<_>>()
            }
        }
    })?;

    // Batch the insertion of excerpts and yield between batches, to avoid blocking the main thread when a single file has many hunks.
    const EXCERPT_BATCH_SIZE: usize = 10;
    let total = excerpt_ranges.len();
    let mut batch_end = 0;
    while batch_end < total {
        let is_first_batch = batch_end == 0;
        batch_end = (batch_end + EXCERPT_BATCH_SIZE).min(total);
        let ranges = excerpt_ranges[..batch_end].to_vec();
        editor.update_in(cx, |editor, window, cx| {
            editor.update_excerpts_for_path(
                path.clone(),
                buffer.clone(),
                ranges,
                multibuffer_context_lines(cx),
                buffer_diff.clone(),
                cx,
            );
            if is_first_batch && editor.diff_view_style() == DiffViewStyle::Split {
                editor.split(window, cx);
            }
        })?;
        if batch_end < total {
            yield_now().await;
        }
    }

    Ok(LoadedCommitFile {
        buffer_id,
        status,
        is_binary,
    })
}

/// How many earlier commits of the file a file history view appends. Each one
/// loads that commit's whole diff, so the full history of a file with thousands
/// of commits would take long and fill the view with excerpts.
const FILE_HISTORY_COMMIT_LIMIT: usize = 50;

/// Appends, newest first, what the commits before `commit_sha` changed in
/// `path`, taken from the same path history the git graph shows for a file.
/// Without `commit_sha` it starts at the newest commit that changed the file.
async fn add_file_history(
    this: &WeakEntity<FileHistoryView>,
    editor: &WeakEntity<SplittableEditor>,
    path: RepoPath,
    commit_sha: Option<&str>,
    repository: &Entity<Repository>,
    project: &Entity<Project>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let log_source = LogSource::Path(path.clone());
    let log_order = LogOrder::default();
    let (loaded_tx, mut loaded_rx) = futures::channel::mpsc::unbounded::<()>();
    // Subscribing in the same update that starts or reads the load means the
    // event that ends it cannot be missed.
    let (subscription, is_loading) = cx.update(|_, cx| {
        let subscription = cx.subscribe(repository, {
            let log_source = log_source.clone();
            move |_, event: &RepositoryEvent, _| {
                if let RepositoryEvent::GraphEvent(
                    (event_source, event_order),
                    GitGraphEvent::FullyLoaded | GitGraphEvent::LoadingError,
                ) = event
                    && *event_source == log_source
                    && *event_order == log_order
                {
                    loaded_tx.unbounded_send(()).ok();
                }
            }
        });
        let is_loading = repository.update(cx, |repository, cx| {
            repository
                .graph_data(log_source.clone(), log_order, 0..0, cx)
                .is_loading
        });
        (subscription, is_loading)
    })?;
    if is_loading {
        loaded_rx.next().await;
    }
    drop(subscription);

    let earlier_shas = cx.update(|_, cx| -> Result<Vec<String>> {
        let graph_data = repository
            .read(cx)
            .get_graph_data(log_source, log_order)
            .context("file history was not loaded")?;
        if let Some(error) = &graph_data.error {
            anyhow::bail!("failed to load file history: {error}");
        }
        let shas = graph_data
            .commit_data
            .iter()
            .map(|commit| commit.sha.to_string());
        Ok(match commit_sha {
            Some(commit_sha) => shas
                .skip_while(|sha| sha != commit_sha)
                .skip(1)
                .take(FILE_HISTORY_COMMIT_LIMIT)
                .collect(),
            None => shas.take(FILE_HISTORY_COMMIT_LIMIT).collect(),
        })
    })??;

    for (index, sha) in earlier_shas.into_iter().enumerate() {
        let sort_prefix = FILE_NAMESPACE_SORT_PREFIX + 1 + index as u64;
        add_commit_section(
            this,
            editor,
            sha,
            None,
            &path,
            sort_prefix,
            repository,
            project,
            language_registry,
            cx,
        )
        .await
        .log_err();
    }

    Ok(())
}

/// Adds what the commit changed in `path` as one section of the view.
/// `preloaded_file` is that change when the caller already read it from git.
async fn add_commit_section(
    this: &WeakEntity<FileHistoryView>,
    editor: &WeakEntity<SplittableEditor>,
    sha: String,
    preloaded_file: Option<CommitFile>,
    path: &RepoPath,
    sort_prefix: u64,
    repository: &Entity<Repository>,
    project: &Entity<Project>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let commit_details =
        repository.update(cx, |repository, cx| repository.show_commit(sha.clone(), cx));
    let file = match preloaded_file {
        Some(file) => Some(file),
        None => {
            let commit_diff = repository
                .update(cx, |repository, cx| {
                    repository.load_commit_diff(sha.clone(), false, cx)
                })
                .await?;
            // `--follow` also lists the commits from before a rename, whose diffs
            // have the file under its old path, so those find nothing here.
            commit_diff
                .files
                .into_iter()
                .find(|file| &file.path == path)
        }
    };
    let Some(file) = file else {
        return Ok(());
    };
    let commit_details = commit_details.await?;
    let subject = truncate_and_trailoff(commit_subject(&commit_details.message), 50);
    let loaded = add_commit_file(
        editor,
        file,
        short_sha(&sha),
        Some(&subject),
        sort_prefix,
        repository,
        project,
        language_registry,
        cx,
    )
    .await?;
    editor.update(cx, |editor, cx| {
        editor.rhs_editor().update(cx, |editor, cx| {
            if let Some(addon) = editor.addon_mut::<CommitDiffAddon>() {
                addon.file_statuses.insert(loaded.buffer_id, loaded.status);
            }
            if loaded.is_binary {
                editor.fold_buffers([loaded.buffer_id], cx);
            }
        });
    })?;
    this.update(cx, |this, cx| {
        this.sections.push(FileHistorySection {
            buffer_id: loaded.buffer_id,
            commit: Some(commit_details),
        });
        this.update_current_section(cx);
        cx.notify();
    })?;
    Ok(())
}

/// One file's changes over time: either its uncommitted changes or one
/// commit's changes to it, followed by what the commits before changed in it.
/// The header shows the commit of the section at the top of the scroll.
pub struct FileHistoryView {
    path: RepoPath,
    repository: Entity<Repository>,
    /// The commit the view starts at, or `None` when it starts at the
    /// uncommitted changes.
    start_sha: Option<SharedString>,
    workspace: WeakEntity<Workspace>,
    remote: Option<GitRemote>,
    editor: Entity<SplittableEditor>,
    /// In the order they appear in the editor.
    sections: Vec<FileHistorySection>,
    current_section: usize,
    _subscriptions: Vec<Subscription>,
    _load_task: Task<()>,
}

struct FileHistorySection {
    buffer_id: language::BufferId,
    /// `None` for the uncommitted changes.
    commit: Option<CommitDetails>,
}

impl FileHistoryView {
    /// Opens the file's uncommitted changes with its earlier commits below.
    pub fn open(
        path: RepoPath,
        repository: Entity<Repository>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::open_at(path, repository, None, None, workspace, window, cx);
    }

    /// Opens what the commit changed in the file with its earlier commits below.
    /// `preloaded_file` is that change when the caller already read it from git.
    pub fn open_at_commit(
        path: RepoPath,
        repository: Entity<Repository>,
        sha: SharedString,
        preloaded_file: Option<CommitFile>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::open_at(
            path,
            repository,
            Some(sha),
            preloaded_file,
            workspace,
            window,
            cx,
        );
    }

    fn open_at(
        path: RepoPath,
        repository: Entity<Repository>,
        start_sha: Option<SharedString>,
        preloaded_file: Option<CommitFile>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let repository_id = repository.read(cx).id;
        let existing = workspace.items_of_type::<Self>(cx).find(|view| {
            let view = view.read(cx);
            view.path == path
                && view.start_sha == start_sha
                && view.repository.read(cx).id == repository_id
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let workspace_entity = cx.entity();
        let view = cx.new(|cx| {
            Self::new(
                path,
                repository,
                start_sha,
                preloaded_file,
                project,
                workspace_entity,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn new(
        path: RepoPath,
        repository: Entity<Repository>,
        start_sha: Option<SharedString>,
        preloaded_file: Option<CommitFile>,
        project: Entity<Project>,
        workspace_entity: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let workspace = workspace_entity.downgrade();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project.clone(),
                workspace_entity,
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
                // The commits' buffers are not on disk, so their status comes
                // from this addon, which has no commit view to open files from.
                editor.register_addon(CommitDiffAddon {
                    file_statuses: HashMap::default(),
                    commit_view: WeakEntity::new_invalid(),
                });
            });
            editor
        });
        let rhs_editor = editor.read(cx).rhs_editor().clone();
        let scroll_subscription = cx.subscribe(&rhs_editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::ScrollPositionChanged { .. }) {
                this.update_current_section(cx);
            }
        });

        let snapshot = repository.read(cx).snapshot();
        let remote = snapshot
            .remote_upstream_url
            .as_ref()
            .or(snapshot.remote_origin_url.as_ref())
            .and_then(|url| {
                let provider_registry = GitHostingProviderRegistry::default_global(cx);
                parse_git_remote_url(provider_registry, url).map(|(host, parsed)| GitRemote {
                    host,
                    owner: parsed.owner.into(),
                    repo: parsed.repo.into(),
                })
            });

        let weak_editor = editor.downgrade();
        let load_task = cx.spawn_in(window, {
            let path = path.clone();
            let repository = repository.clone();
            let start_sha = start_sha.clone();
            async move |this, cx| {
                match &start_sha {
                    Some(start_sha) => {
                        add_commit_section(
                            &this,
                            &weak_editor,
                            start_sha.to_string(),
                            preloaded_file,
                            &path,
                            FILE_NAMESPACE_SORT_PREFIX,
                            &repository,
                            &project,
                            &language_registry,
                            cx,
                        )
                        .await
                        .log_err();
                    }
                    None => {
                        add_uncommitted_changes(&this, &weak_editor, &path, &project, cx)
                            .await
                            .log_err();
                    }
                }
                add_file_history(
                    &this,
                    &weak_editor,
                    path,
                    start_sha.as_deref(),
                    &repository,
                    &project,
                    &language_registry,
                    cx,
                )
                .await
                .log_err();
            }
        });
        Self {
            path,
            repository,
            start_sha,
            workspace,
            remote,
            editor,
            sections: Vec::new(),
            current_section: 0,
            _subscriptions: vec![scroll_subscription],
            _load_task: load_task,
        }
    }

    fn file_name(&self) -> SharedString {
        self.path
            .file_name()
            .map(|name| SharedString::from(name.to_string()))
            .unwrap_or_else(|| SharedString::from(self.path.as_unix_str().to_string()))
    }

    /// Points the header at the section whose sticky buffer header is at the
    /// top of the editor. Between two sections, where no header is sticky, the
    /// header keeps showing the last one.
    fn update_current_section(&mut self, cx: &mut Context<Self>) {
        let rhs_editor = self.editor.read(cx).rhs_editor().clone();
        let top_buffer_id = rhs_editor.update(cx, |editor, cx| {
            let scroll_top = editor.scroll_position(cx).y;
            editor
                .display_snapshot(cx)
                .sticky_header_excerpt(scroll_top)
                .map(|header| header.excerpt.buffer_id())
        });
        let Some(index) = top_buffer_id.and_then(|buffer_id| {
            self.sections
                .iter()
                .position(|section| section.buffer_id == buffer_id)
        }) else {
            return;
        };
        if index != self.current_section {
            self.current_section = index;
            cx.notify();
        }
    }

    fn render_section_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let section_count = self.sections.len();
        let position = Label::new(if section_count == 0 {
            "Loading…".to_string()
        } else {
            format!("{} of {}", self.current_section + 1, section_count)
        })
        .size(LabelSize::Small)
        .color(Color::Muted);
        let path_row = h_flex()
            .w_full()
            .gap_2()
            .justify_between()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Diff)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(self.path.as_unix_str().to_string()).truncate()),
            )
            .child(position);

        let commit = self
            .sections
            .get(self.current_section)
            .map(|section| section.commit.as_ref());
        let details = match commit {
            Some(Some(commit)) => self.render_commit_details(commit, window, cx),
            Some(None) => h_flex()
                .gap_2()
                .child(
                    Icon::new(IconName::Pencil)
                        .size(IconSize::Small)
                        .color(Color::Modified),
                )
                .child(Label::new("Uncommitted Changes"))
                .into_any_element(),
            None => div().into_any_element(),
        };

        v_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(path_row)
            .child(details)
    }

    fn render_commit_details(
        &self,
        commit: &CommitDetails,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let commit_date = time::OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let date_string = time_format::format_localized_timestamp(
            commit_date,
            time::OffsetDateTime::now_utc(),
            local_offset,
            time_format::TimestampFormat::MediumAbsolute,
        );
        let avatar = CommitAvatar::new(
            &commit.sha,
            Some(commit.author_email.clone()),
            self.remote.as_ref(),
        )
        .size(rems_from_px(32_f32))
        .render(window, cx);
        let sha = commit.sha.clone();
        let repository = self.repository.downgrade();
        let workspace = self.workspace.clone();

        h_flex()
            .w_full()
            .gap_2()
            .justify_between()
            .child(
                h_flex().min_w_0().gap_2().child(avatar).child(
                    v_flex()
                        .min_w_0()
                        .child(Label::new(commit_subject(&commit.message).to_string()).truncate())
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(
                                    Label::new(commit.author_name.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new("•")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .alpha(0.5),
                                )
                                .child(
                                    Label::new(date_string)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        ),
                ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(
                        Button::new("file-history-copy-sha", short_sha(&commit.sha).to_string())
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(IconName::Copy)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .tooltip({
                                let sha = sha.clone();
                                move |_, cx| {
                                    Tooltip::with_meta("Copy Commit SHA", None, sha.clone(), cx)
                                }
                            })
                            .on_click({
                                let sha = sha.clone();
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        sha.to_string(),
                                    ));
                                }
                            }),
                    )
                    .child(
                        Button::new("file-history-view-commit", "View Commit")
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(IconName::GitCommit)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .on_click(move |_, window, cx| {
                                CommitView::open(
                                    sha.to_string(),
                                    repository.clone(),
                                    workspace.clone(),
                                    None,
                                    None,
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
            .into_any_element()
    }
}

/// Adds the file's live buffer with its diff against HEAD, so the view shows
/// the uncommitted changes and keeps up as the file is edited.
async fn add_uncommitted_changes(
    this: &WeakEntity<FileHistoryView>,
    editor: &WeakEntity<SplittableEditor>,
    path: &RepoPath,
    project: &Entity<Project>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let project_path = this
        .read_with(cx, |this, cx| {
            this.repository.read(cx).repo_path_to_project_path(path, cx)
        })?
        .context("file is outside the project's worktrees")?;
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(project_path, cx))
        .await?;
    let buffer_diff = project
        .update(cx, |project, cx| {
            project.open_uncommitted_diff(buffer.clone(), cx)
        })
        .await?;
    let (buffer_id, excerpt_ranges) = cx.update(|_, cx| {
        let snapshot = buffer.read(cx).snapshot();
        let diff_snapshot = buffer_diff.read(cx).snapshot(cx);
        let mut hunks = diff_snapshot.hunks(&snapshot).peekable();
        let ranges = if hunks.peek().is_none() {
            vec![language::Point::zero()..snapshot.max_point()]
        } else {
            hunks
                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                .collect::<Vec<_>>()
        };
        (snapshot.remote_id(), ranges)
    })?;
    let rel_path: &Arc<RelPath> = path.as_ref();
    let path_key = PathKey::with_sort_prefix(FILE_NAMESPACE_SORT_PREFIX, rel_path.clone());
    editor.update_in(cx, |editor, window, cx| {
        editor.update_excerpts_for_path(
            path_key,
            buffer,
            excerpt_ranges,
            multibuffer_context_lines(cx),
            buffer_diff,
            cx,
        );
        if editor.diff_view_style() == DiffViewStyle::Split {
            editor.split(window, cx);
        }
    })?;
    this.update(cx, |this, cx| {
        this.sections.push(FileHistorySection {
            buffer_id,
            commit: None,
        });
        cx.notify();
    })?;
    Ok(())
}

impl EventEmitter<EditorEvent> for FileHistoryView {}

impl Focusable for FileHistoryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for FileHistoryView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.start_sha {
            Some(sha) => format!("{} — {}", self.file_name(), short_sha(sha)).into(),
            None => format!("{} — History", self.file_name()).into(),
        }
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(match &self.start_sha {
            Some(sha) => format!(
                "History of {} from {}",
                self.path.as_unix_str(),
                short_sha(sha)
            )
            .into(),
            None => format!(
                "Uncommitted changes and history of {}",
                self.path.as_unix_str()
            )
            .into(),
        })
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for FileHistoryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_section_header(window, cx))
            .child(div().flex_grow(1.).min_h_0().child(self.editor.clone()))
    }
}

impl EventEmitter<EditorEvent> for CommitView {}

impl Focusable for CommitView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for CommitView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitCommit).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if let Some(older_commit) = &self.older_commit {
            return short_revision(&self.commit, Some(older_commit)).into();
        }
        let short_sha = self.commit.sha.get(0..7).unwrap_or(&*self.commit.sha);
        let subject = truncate_and_trailoff(self.commit.message.split('\n').next().unwrap(), 20);
        format!("{short_sha} — {subject}").into()
    }

    fn tab_tooltip_content(&self, _: &App) -> Option<TabTooltipContent> {
        if let Some(older_commit) = &self.older_commit {
            let commit_rows = [older_commit, &self.commit].map(|commit| {
                (
                    SharedString::from(short_sha(&commit.sha).to_string()),
                    SharedString::from(commit_subject(&commit.message).to_string()),
                )
            });
            return Some(TabTooltipContent::Custom(Box::new(Tooltip::element(
                move |_, _| {
                    v_flex()
                        .children(commit_rows.iter().map(|(short_sha, subject)| {
                            h_flex()
                                .gap_1p5()
                                .child(Label::new(short_sha.clone()).color(Color::Muted))
                                .child(Label::new(subject.clone()))
                        }))
                        .into_any_element()
                },
            ))));
        }

        let short_sha = self.commit.sha.get(0..16).unwrap_or(&*self.commit.sha);
        let subject = self.commit.message.split('\n').next().unwrap();

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let subject = subject.to_string();
            let short_sha = short_sha.to_string();

            move |_, _| {
                v_flex()
                    .child(Label::new(subject.clone()))
                    .child(
                        Label::new(short_sha.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Commit View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let file_statuses = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .addon::<CommitDiffAddon>()
            .map(|addon| addon.file_statuses.clone())
            .unwrap_or_default();
        let Some(workspace_entity) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let project = self.project.clone();
        let diff_view_style = self.editor.read(cx).diff_view_style();
        let multibuffer = self.multibuffer.clone();
        Task::ready(Some(cx.new(|cx| {
            let commit_view = cx.weak_entity();
            let editor = cx.new({
                let file_statuses = file_statuses.clone();
                let project = project.clone();
                let workspace_entity = workspace_entity.clone();
                let multibuffer = multibuffer.clone();
                move |cx| {
                    let editor = SplittableEditor::new(
                        diff_view_style,
                        multibuffer.clone(),
                        project.clone(),
                        workspace_entity.clone(),
                        window,
                        cx,
                    );
                    editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                    editor.rhs_editor().update(cx, |editor, cx| {
                        editor.set_show_bookmarks(false, cx);
                        editor.set_show_breakpoints(false, cx);
                        editor.set_show_diff_review_button(true, cx);
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                    editor
                }
            });
            let language_registry = project.read(cx).languages().clone();
            let message = cx.new(|cx| {
                Markdown::new(
                    self.commit.message.clone(),
                    Some(language_registry),
                    None,
                    cx,
                )
            });
            Self {
                editor,
                message,
                message_expanded: self.message_expanded,
                message_scroll_handle: ScrollHandle::new(),
                multibuffer: self.multibuffer.clone(),
                commit: self.commit.clone(),
                older_commit: self.older_commit.clone(),
                stash: self.stash,
                repository: self.repository.clone(),
                project: self.project.clone(),
                workspace: self.workspace.clone(),
                remote: self.remote.clone(),
                is_shallow_boundary: self.is_shallow_boundary,
                file_filter: self.file_filter.clone(),
                _load_diff_task: Task::ready(Ok(())),
            }
        })))
    }
}

impl Render for CommitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_stash = self.stash.is_some();
        let header = match &self.older_commit {
            Some(older_commit) => self
                .render_comparison_header(older_commit, window, cx)
                .into_any_element(),
            None => self.render_header(window, cx).into_any_element(),
        };

        v_flex()
            .key_context(if is_stash { "StashDiff" } else { "CommitDiff" })
            .on_action(cx.listener(Self::open_file_at_head_action))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(header)
            .when(
                !self.editor.read(cx).rhs_editor().read(cx).is_empty(cx),
                |this| this.child(div().flex_grow(1.).child(self.editor.clone())),
            )
            .when(self.is_shallow_boundary, |this| {
                this.child(self.render_shallow_boundary_notice(cx))
            })
    }
}

pub struct CommitViewToolbar {
    commit_view: Option<WeakEntity<CommitView>>,
}

impl CommitViewToolbar {
    pub fn new() -> Self {
        Self { commit_view: None }
    }
}

impl EventEmitter<ToolbarItemEvent> for CommitViewToolbar {}

impl Render for CommitViewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(commit_view) = self.commit_view.as_ref().and_then(|w| w.upgrade()) else {
            return div();
        };

        let commit_view_ref = commit_view.read(cx);
        let is_stash = commit_view_ref.stash.is_some();

        let (additions, deletions) = commit_view_ref.calculate_changed_lines(cx);

        let commit_sha = commit_view_ref.commit.sha.clone();
        let is_comparison = commit_view_ref.older_commit.is_some();

        let remote_info = commit_view_ref.remote.as_ref().map(|remote| {
            let provider = remote.host.name();
            let parsed_remote = ParsedGitRemote {
                owner: remote.owner.as_ref().into(),
                repo: remote.repo.as_ref().into(),
            };
            let params = BuildCommitPermalinkParams { sha: &commit_sha };
            let url = remote
                .host
                .build_commit_permalink(&parsed_remote, params)
                .to_string();
            (provider, url)
        });

        let sha_for_graph = commit_sha.to_string();

        h_flex()
            .gap_1()
            .when(additions > 0 || deletions > 0, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(DiffStat::new(
                            "toolbar-diff-stat",
                            additions as usize,
                            deletions as usize,
                        ))
                        .child(Divider::vertical()),
                )
            })
            .child(
                IconButton::new("buffer-search", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action(
                            "Buffer Search",
                            &zed_actions::buffer_search::Deploy::find(),
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::buffer_search::Deploy::find()),
                            cx,
                        );
                    }),
            )
            .when(!is_stash, |this| {
                this.child(
                    IconButton::new("show-in-git-graph", IconName::GitGraph)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Show in Git Graph"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::git_graph::OpenAtCommit {
                                    sha: sha_for_graph.clone(),
                                }),
                                cx,
                            );
                        }),
                )
                // Hosting providers only build permalinks to a single commit, which for
                // a comparison would show the newer end's own changes instead.
                .children(
                    remote_info
                        .filter(|_| !is_comparison)
                        .map(|(provider_name, url)| {
                            let icon = ui::git_hosting_provider_icon(provider_name.as_str());

                            IconButton::new("view_on_provider", icon)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(format!("View on {}", provider_name)))
                                .on_click(move |_, _, cx| cx.open_url(&url))
                        }),
                )
            })
    }
}

impl ToolbarItemView for CommitViewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        if let Some(entity) = active_pane_item.and_then(|i| i.act_as::<CommitView>(cx)) {
            self.commit_view = Some(entity.downgrade());
            return ToolbarItemLocation::PrimaryRight;
        }
        self.commit_view = None;
        ToolbarItemLocation::Hidden
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(0..git::SHORT_SHA_LENGTH).unwrap_or(sha)
}

fn short_revision(commit: &CommitDetails, older_commit: Option<&CommitDetails>) -> String {
    match older_commit {
        Some(older_commit) => format!(
            "{}..{}",
            short_sha(&older_commit.sha),
            short_sha(&commit.sha)
        ),
        None => short_sha(&commit.sha).to_string(),
    }
}

fn commit_subject(message: &str) -> &str {
    message.lines().next().unwrap_or_default()
}

fn stash_matches_index(sha: &str, stash_index: usize, repo: &Repository) -> bool {
    repo.stash_entries
        .entries
        .get(stash_index)
        .map(|entry| entry.oid.to_string() == sha)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{EmptyView, TestAppContext};
    use indoc::indoc;
    use language::{Language, LanguageConfig, LanguageMatcher, markdown_lang};
    use settings::SettingsStore;

    #[gpui::test]
    async fn test_build_buffer_resolves_injected_languages(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
        });

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.add(markdown_lang());
        language_registry.add(Arc::new(markdown_inline_lang()));

        let window = cx.add_window(|_, _| EmptyView);
        let mut async_cx = window
            .update(cx, |_, window, cx| window.to_async(cx))
            .expect("window should be open");

        let text = indoc! {"
            # Title

            Some *emphasized* text.
        "}
        .to_string();
        let blob = Arc::new(GitBlob {
            path: RepoPath::new("notes.md").unwrap(),
            worktree_id: WorktreeId::from_usize(0),
            is_deleted: false,
            is_binary: false,
            display_name: "abc1234 - notes.md".into(),
        }) as Arc<dyn File>;

        let buffer = build_buffer(text, blob, &language_registry, &mut async_cx)
            .await
            .expect("buffer should build");

        cx.run_until_parked();

        buffer.read_with(cx, |buffer, _| {
            let language = buffer.language().expect("buffer should have a language");
            assert_eq!(language.name().as_ref(), "Markdown");

            let layers = buffer
                .snapshot()
                .syntax_layers()
                .map(|layer| layer.language.name().to_string())
                .collect::<Vec<_>>();
            assert!(
                layers.iter().any(|name| name == "Markdown-Inline"),
                "emphasis, links, and fenced code blocks are highlighted by the injected \
                 Markdown-Inline grammar, but the buffer parsed with layers {layers:?}",
            );
        });
    }

    #[gpui::test]
    fn test_git_blob_language_detection_ignores_display_name(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut store = SettingsStore::test(cx);
            store
                .set_user_settings(r#"{"file_types":{"JSONC":["tsconfig*.json"]}}"#, cx)
                .expect("valid file type settings");
            cx.set_global(store);
        });

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.register_test_language(LanguageConfig {
            name: "JSON".into(),
            matcher: Arc::new(LanguageMatcher {
                path_suffixes: vec!["json".into(), "flake.lock".into()],
                ..LanguageMatcher::default()
            }),
            ..LanguageConfig::default()
        });
        language_registry.register_test_language(LanguageConfig {
            name: "JSONC".into(),
            ..LanguageConfig::default()
        });

        let languages = cx.update(|cx| {
            [
                ("flake.lock", "abc1234 - flake.lock"),
                ("flake.lock", "flake.lock @ abc1234"),
                ("nix/flake.lock", "abc1234 - flake.lock"),
                ("nix/flake.lock", "flake.lock @ abc1234"),
                ("web/tsconfig.json", "abc1234 - tsconfig.json"),
                ("web/tsconfig.json", "tsconfig.json @ abc1234"),
            ]
            .map(|(path, display_name)| {
                let blob = Arc::new(GitBlob {
                    path: RepoPath::new(path).expect("valid repository path"),
                    worktree_id: WorktreeId::from_usize(0),
                    is_deleted: false,
                    is_binary: false,
                    display_name: display_name.to_string(),
                }) as Arc<dyn File>;
                assert_eq!(blob.file_system_abs_path(cx), None);
                language_registry
                    .language_for_file(&blob, None, cx)
                    .and_then(|id| language_registry.language_name_for_id(id))
                    .map(|name| name.to_string())
            })
        });

        assert_eq!(
            languages.each_ref().map(|name| name.as_deref()),
            [
                Some("JSON"),
                Some("JSON"),
                Some("JSON"),
                Some("JSON"),
                Some("JSONC"),
                Some("JSONC"),
            ]
        );
    }

    fn markdown_inline_lang() -> Language {
        Language::new(
            LanguageConfig {
                name: "Markdown-Inline".into(),
                hidden: true,
                ..LanguageConfig::default()
            },
            Some(tree_sitter_md::INLINE_LANGUAGE.into()),
        )
        .with_highlights_query("(emphasis) @emphasis")
        .unwrap()
    }
}
