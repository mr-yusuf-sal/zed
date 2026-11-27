use askpass::AskPassDelegate;
use collections::{HashMap, HashSet};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent};
use git::{
    repository::{CommitOptions, LogSource, RepoPath},
    status::FileStatus,
};
use git_ui_core::{askpass_modal::AskPassModal, notifications::show_error_toast};
use gpui::{
    AnyElement, App, AsyncApp, ClickEvent, ClipboardItem, Context, CursorStyle, DispatchPhase,
    Empty, Entity, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, MouseUpEvent,
    Pixels, Point, PromptLevel, Render, ScrollHandle, SharedString, Subscription, Task, WeakEntity,
    Window, anchored, canvas, deferred, point, px,
};
use project::{
    Project, ProjectPath,
    git_store::{GitStore, GitStoreEvent, Repository, RepositoryEvent, RepositoryId},
};
use std::time::Duration;
use ui::{
    ContextMenu, ContextMenuEntry, DiffStat, ListItem, ListItemSpacing, PopoverMenu, Tooltip,
    prelude::*, right_click_menu,
};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{commit_view::FileHistoryView, git_status_icon, project_diff::ProjectDiff};

const SECTIONS_SIDE_BY_SIDE_KEY: &str = "working_tree_changes_sections_side_by_side";
const TREE_VIEW_KEY: &str = "working_tree_changes_tree_view";
/// A folder with more changed files than this starts collapsed, so a folder added or
/// deleted in one go takes one row instead of hundreds.
const COLLAPSE_DIRECTORY_ABOVE: usize = 20;
const TREE_INDENT: Pixels = px(12.);
/// How far the pointer can move from where the middle button was pressed before
/// autoscroll starts moving the list.
const AUTOSCROLL_DEAD_ZONE: Pixels = px(8.);
const AUTOSCROLL_FRAME: Duration = Duration::from_millis(16);
/// Pixels scrolled per frame for every pixel the pointer is beyond the dead zone.
const AUTOSCROLL_SPEED: f32 = 0.12;
const AUTOSCROLL_MAX_STEP: Pixels = px(160.);

/// The uncommitted and staged changes of a project's repositories, with buttons to stage,
/// unstage and discard them, and a form to commit the staged ones and push.
pub struct WorkingTreeChanges {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    /// `None` lists every repository, and commits to `commit_target`.
    repository_id: Option<RepositoryId>,
    /// The repository picked to commit and push when every repository is
    /// listed. `None` follows the staged changes, see `target_repository`.
    commit_target: Option<RepositoryId>,
    changes: Vec<RepositoryChanges>,
    commit_title_editor: Entity<Editor>,
    commit_description_editor: Entity<Editor>,
    pending_commit: Option<Task<()>>,
    /// Whether the uncommitted and staged lists sit next to each other rather than one
    /// above the other.
    sections_side_by_side: bool,
    /// Whether files are grouped in a tree of their folders rather than listed flat.
    tree_view: bool,
    filter_editor: Entity<Editor>,
    status_filter: Option<StatusFilter>,
    /// Folders opened or closed against their default, see `is_directory_collapsed`.
    toggled_directories: HashSet<(bool, RepositoryId, SharedString)>,
    /// The row the arrow keys, Space, Enter and Delete act on.
    selection: Option<RowSelection>,
    unstaged_scroll_handle: ScrollHandle,
    staged_scroll_handle: ScrollHandle,
    autoscroll: Option<ListAutoscroll>,
    focus_handle: FocusHandle,
    /// Held by the two lists, so their keys apply only while one of them has focus.
    list_focus_handle: FocusHandle,
    _git_store_subscription: Subscription,
    _filter_editor_subscription: Subscription,
}

/// Which kind of change a list shows, when filtered by status.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    Modified,
    Created,
    Deleted,
    Conflicted,
}

impl StatusFilter {
    const ALL: [StatusFilter; 4] = [
        StatusFilter::Modified,
        StatusFilter::Created,
        StatusFilter::Deleted,
        StatusFilter::Conflicted,
    ];

    fn matches(self, status: FileStatus) -> bool {
        match self {
            StatusFilter::Conflicted => status.is_conflicted(),
            StatusFilter::Deleted => status.is_deleted(),
            StatusFilter::Created => status.is_created(),
            StatusFilter::Modified => {
                status.is_modified() && !status.is_deleted() && !status.is_created()
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            StatusFilter::Modified => "Modified",
            StatusFilter::Created => "New",
            StatusFilter::Deleted => "Deleted",
            StatusFilter::Conflicted => "Conflicts",
        }
    }
}

/// One row of a list, in the order shown.
#[derive(Clone)]
enum ChangeRow {
    Repository {
        repository_index: usize,
        /// The repository's files the search and status filter let through.
        shown: Vec<usize>,
        filtering: bool,
    },
    Directory {
        repository_index: usize,
        path: SharedString,
        name: SharedString,
        depth: usize,
        /// The shown files anywhere under the folder.
        files: Vec<usize>,
        collapsed: bool,
    },
    File {
        repository_index: usize,
        file_index: usize,
        depth: usize,
    },
}

impl ChangeRow {
    fn repository_index(&self) -> usize {
        match self {
            ChangeRow::Repository {
                repository_index, ..
            }
            | ChangeRow::Directory {
                repository_index, ..
            }
            | ChangeRow::File {
                repository_index, ..
            } => *repository_index,
        }
    }
}

/// A selected row, kept by what it shows rather than by position, so it survives the lists
/// changing under it.
#[derive(Clone, PartialEq, Eq)]
struct RowSelection {
    staged: bool,
    repository_id: RepositoryId,
    target: RowTarget,
}

#[derive(Clone, PartialEq, Eq)]
enum RowTarget {
    Directory(SharedString),
    File(RepoPath),
}

/// Middle-click autoscroll: a list scrolls towards the pointer, faster the farther it is
/// from `anchor`, until any button is pressed or Escape. Holding the middle button and
/// dragging ends it on release instead.
struct ListAutoscroll {
    staged: bool,
    anchor: Point<Pixels>,
    /// The pointer has left the dead zone since the press, so releasing the middle button
    /// ends the autoscroll.
    moved: bool,
    _task: Task<()>,
}

#[derive(PartialEq)]
struct RepositoryChanges {
    id: RepositoryId,
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
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter files…", window, cx);
            editor
        });
        let filter_editor_subscription =
            cx.subscribe(&filter_editor, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            });
        let mut this = Self {
            workspace,
            git_store,
            repository_id,
            commit_target: None,
            changes: Vec::new(),
            commit_title_editor,
            commit_description_editor,
            pending_commit: None,
            sections_side_by_side: Self::restore_sections_side_by_side(cx),
            tree_view: Self::restore_tree_view(cx),
            filter_editor,
            status_filter: None,
            toggled_directories: HashSet::default(),
            selection: None,
            unstaged_scroll_handle: ScrollHandle::new(),
            staged_scroll_handle: ScrollHandle::new(),
            autoscroll: None,
            focus_handle: cx.focus_handle(),
            list_focus_handle: cx.focus_handle(),
            _git_store_subscription: git_store_subscription,
            _filter_editor_subscription: filter_editor_subscription,
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
                    id: repository.id,
                    name: name.into(),
                    repository: repository_handle.downgrade(),
                    unstaged,
                    staged,
                }
            })
            .collect()
    }

    /// The repository the commit form commits to and pushes. When every
    /// repository is listed and none was picked, it is the one holding the
    /// staged changes: the active repository is only where the last edited
    /// file was, and committing there found nothing staged.
    fn target_repository(&self, cx: &App) -> Option<Entity<Repository>> {
        let git_store = self.git_store.read(cx);
        let repositories = git_store.repositories();
        if let Some(repository_id) = self.repository_id {
            return repositories.get(&repository_id).cloned();
        }
        if let Some(repository) = self
            .commit_target
            .and_then(|repository_id| repositories.get(&repository_id))
        {
            return Some(repository.clone());
        }
        let active = git_store.active_repository();
        if active
            .as_ref()
            .is_some_and(|repository| has_staged_changes(repository.read(cx)))
        {
            return active;
        }
        self.changes
            .iter()
            .filter(|repository| !repository.staged.is_empty())
            .find_map(|repository| repository.repository.upgrade())
            .or(active)
    }

    fn set_commit_target(&mut self, repository_id: RepositoryId, cx: &mut Context<Self>) {
        self.commit_target = Some(repository_id);
        cx.notify();
    }

    /// Picks the repository to commit to, shown only when there is more than
    /// one to pick from.
    fn render_commit_target(
        &self,
        target: Option<&Entity<Repository>>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.repository_id.is_some() || self.changes.len() < 2 {
            return None;
        }
        let target_id = target.map(|repository| repository.read(cx).id);
        let label = self
            .changes
            .iter()
            .find(|repository| Some(repository.id) == target_id)
            .map_or_else(
                || SharedString::from("Repository"),
                |repository| repository.name.clone(),
            );
        let repositories = self
            .changes
            .iter()
            .map(|repository| {
                (
                    repository.id,
                    repository.name.clone(),
                    repository.staged.len(),
                )
            })
            .collect::<Vec<_>>();
        let this = cx.weak_entity();
        Some(
            PopoverMenu::new("commit-target")
                .trigger_with_tooltip(
                    Button::new("commit-target-button", label)
                        .label_size(LabelSize::Small)
                        .end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                    Tooltip::text("The repository to commit to and push"),
                )
                .anchor(gpui::Anchor::BottomLeft)
                .menu(move |window, cx| {
                    let this = this.clone();
                    let repositories = repositories.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for (repository_id, name, staged_count) in &repositories {
                            let this = this.clone();
                            let repository_id = *repository_id;
                            let label = if *staged_count > 0 {
                                format!("{name} ({staged_count} staged)")
                            } else {
                                name.to_string()
                            };
                            menu = menu.toggleable_entry(
                                label,
                                Some(repository_id) == target_id,
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    this.update(cx, |this, cx| {
                                        this.set_commit_target(repository_id, cx);
                                    })
                                    .log_err();
                                },
                            );
                        }
                        menu
                    }))
                })
                .into_any_element(),
        )
    }

    /// The rows of one list: a header per repository, then its shown files, in a tree of
    /// their directories or flat. A search or status filter shows every directory open, so
    /// matches can't hide in a collapsed one.
    fn build_rows(&self, staged: bool, cx: &App) -> Vec<ChangeRow> {
        let terms = self.filter_terms(cx);
        let filtering = !terms.is_empty() || self.status_filter.is_some();
        let mut rows = Vec::new();
        for (repository_index, repository) in self.changes.iter().enumerate() {
            let files = repository.files(staged);
            let mut shown = (0..files.len())
                .filter(|&file_index| self.is_shown(&files[file_index], &terms))
                .collect::<Vec<_>>();
            if shown.is_empty() {
                continue;
            }
            // A directory's own files come before its subdirectories.
            shown.sort_by(|&a, &b| {
                (
                    files[a].directory.as_deref().unwrap_or(""),
                    &files[a].file_name,
                )
                    .cmp(&(
                        files[b].directory.as_deref().unwrap_or(""),
                        &files[b].file_name,
                    ))
            });
            rows.push(ChangeRow::Repository {
                repository_index,
                shown: shown.clone(),
                filtering,
            });
            if !self.tree_view {
                rows.extend(shown.iter().map(|&file_index| ChangeRow::File {
                    repository_index,
                    file_index,
                    depth: 0,
                }));
                continue;
            }

            let mut files_under: HashMap<String, Vec<usize>> = HashMap::default();
            for &file_index in &shown {
                if let Some(directory) = &files[file_index].directory {
                    let components = directory.split('/').collect::<Vec<_>>();
                    for depth in 1..=components.len() {
                        files_under
                            .entry(components[..depth].join("/"))
                            .or_default()
                            .push(file_index);
                    }
                }
            }

            // The directories of the previous file, and the depth of a collapsed one among
            // them whose contents are left out.
            let mut open: Vec<String> = Vec::new();
            let mut collapsed_depth: Option<usize> = None;
            for &file_index in &shown {
                let components = files[file_index]
                    .directory
                    .as_deref()
                    .map(|directory| directory.split('/').collect::<Vec<_>>())
                    .unwrap_or_default();
                let common = open
                    .iter()
                    .zip(&components)
                    .take_while(|(open, component)| open.as_str() == **component)
                    .count();
                open.truncate(common);
                if collapsed_depth.is_some_and(|depth| depth > common) {
                    collapsed_depth = None;
                }
                for depth in common..components.len() {
                    open.push(components[depth].to_string());
                    if collapsed_depth.is_some() {
                        continue;
                    }
                    let path = open.join("/");
                    let files_here = files_under.get(&path).cloned().unwrap_or_default();
                    let collapsed = !filtering
                        && self.is_directory_collapsed(
                            staged,
                            repository.id,
                            &path,
                            files_here.len(),
                        );
                    rows.push(ChangeRow::Directory {
                        repository_index,
                        path: path.into(),
                        name: components[depth].to_string().into(),
                        depth,
                        files: files_here,
                        collapsed,
                    });
                    if collapsed {
                        collapsed_depth = Some(depth + 1);
                    }
                }
                if collapsed_depth.is_none() {
                    rows.push(ChangeRow::File {
                        repository_index,
                        file_index,
                        depth: components.len(),
                    });
                }
            }
        }
        rows
    }

    fn filter_terms(&self, cx: &App) -> Vec<String> {
        self.filter_editor
            .read(cx)
            .text(cx)
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    fn is_shown(&self, file: &ChangedFile, terms: &[String]) -> bool {
        if self
            .status_filter
            .is_some_and(|filter| !filter.matches(file.status))
        {
            return false;
        }
        if terms.is_empty() {
            return true;
        }
        let path = file.repo_path.as_unix_str().to_lowercase();
        terms.iter().all(|term| path.contains(term.as_str()))
    }

    fn is_directory_collapsed(
        &self,
        staged: bool,
        repository_id: RepositoryId,
        path: &str,
        file_count: usize,
    ) -> bool {
        let toggled = self.toggled_directories.contains(&(
            staged,
            repository_id,
            SharedString::from(path.to_string()),
        ));
        (file_count > COLLAPSE_DIRECTORY_ABOVE) != toggled
    }

    fn toggle_directory(
        &mut self,
        staged: bool,
        repository_id: RepositoryId,
        path: SharedString,
        cx: &mut Context<Self>,
    ) {
        let key = (staged, repository_id, path);
        if !self.toggled_directories.remove(&key) {
            self.toggled_directories.insert(key);
        }
        cx.notify();
    }

    fn row_selection(&self, staged: bool, row: &ChangeRow) -> Option<RowSelection> {
        let repository = self.changes.get(row.repository_index())?;
        let target = match row {
            ChangeRow::Repository { .. } => return None,
            ChangeRow::Directory { path, .. } => RowTarget::Directory(path.clone()),
            ChangeRow::File { file_index, .. } => {
                RowTarget::File(repository.files(staged).get(*file_index)?.repo_path.clone())
            }
        };
        Some(RowSelection {
            staged,
            repository_id: repository.id,
            target,
        })
    }

    fn select_row(
        &mut self,
        selection: Option<RowSelection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selection = selection;
        window.focus(&self.list_focus_handle, cx);
        cx.notify();
    }

    /// The files of a row: the file itself, or every shown file under a directory.
    fn row_files(&self, staged: bool, row: &ChangeRow) -> Vec<ChangedFile> {
        let Some(repository) = self.changes.get(row.repository_index()) else {
            return Vec::new();
        };
        let files = repository.files(staged);
        match row {
            ChangeRow::Repository { .. } => Vec::new(),
            ChangeRow::Directory { files: indices, .. } => indices
                .iter()
                .filter_map(|&file_index| files.get(file_index).cloned())
                .collect(),
            ChangeRow::File { file_index, .. } => {
                files.get(*file_index).cloned().into_iter().collect()
            }
        }
    }

    fn set_row_staged(&mut self, staged: bool, row: &ChangeRow, cx: &mut Context<Self>) {
        let Some(repository) = self.changes.get(row.repository_index()) else {
            return;
        };
        let repository = repository.repository.clone();
        let repo_paths = self
            .row_files(staged, row)
            .into_iter()
            .map(|file| file.repo_path)
            .collect::<Vec<_>>();
        if !repo_paths.is_empty() {
            self.set_staged(&repository, repo_paths, !staged, cx);
        }
    }

    /// The selected row's list and the rows of that list, with the selected row's index.
    fn selected_row(&self, cx: &App) -> Option<(bool, Vec<ChangeRow>, usize)> {
        let selection = self.selection.as_ref()?;
        let rows = self.build_rows(selection.staged, cx);
        let index = rows.iter().position(|row| {
            self.row_selection(selection.staged, row).as_ref() == Some(selection)
        })?;
        Some((selection.staged, rows, index))
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (staged, rows, current) = match self.selected_row(cx) {
            Some(selected) => {
                let (staged, rows, index) = selected;
                (staged, rows, Some(index))
            }
            None => {
                let staged = self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.staged);
                (staged, self.build_rows(staged, cx), None)
            }
        };
        let selectable = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !matches!(row, ChangeRow::Repository { .. }))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let Some(&first) = selectable.first() else {
            return;
        };
        let next = match current
            .and_then(|current| selectable.iter().position(|&index| index == current))
        {
            Some(position) => {
                let position = (position as isize + delta).clamp(0, selectable.len() as isize - 1);
                selectable[position as usize]
            }
            None => first,
        };
        self.selection = self.row_selection(staged, &rows[next]);
        self.scroll_handle(staged).scroll_to_item(next);
        cx.notify();
    }

    fn scroll_handle(&self, staged: bool) -> &ScrollHandle {
        if staged {
            &self.staged_scroll_handle
        } else {
            &self.unstaged_scroll_handle
        }
    }

    /// Stages or unstages the selected row and selects the row after it, which takes its
    /// place once the files move to the other list.
    fn toggle_selected_staged(&mut self, cx: &mut Context<Self>) {
        let Some((staged, rows, index)) = self.selected_row(cx) else {
            return;
        };
        let next = rows
            .iter()
            .skip(index + 1)
            .chain(rows.iter().take(index).rev())
            .find(|row| !matches!(row, ChangeRow::Repository { .. }))
            .and_then(|row| self.row_selection(staged, row));
        self.set_row_staged(staged, &rows[index], cx);
        self.selection = next;
        cx.notify();
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((staged, rows, index)) = self.selected_row(cx) else {
            return;
        };
        let Some(repository) = self.changes.get(rows[index].repository_index()) else {
            return;
        };
        let repository_id = repository.id;
        match &rows[index] {
            ChangeRow::Directory { path, .. } => {
                self.toggle_directory(staged, repository_id, path.clone(), cx);
            }
            ChangeRow::File { file_index, .. } => {
                let Some(file) = repository.files(staged).get(*file_index) else {
                    return;
                };
                let repo_path = file.repo_path.clone();
                let repository = repository.repository.clone();
                self.open_file_changes(&repository, repo_path, window, cx);
            }
            ChangeRow::Repository { .. } => {}
        }
    }

    /// Left collapses a directory and right opens it, like a file tree.
    fn set_selected_directory_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let Some((staged, rows, index)) = self.selected_row(cx) else {
            return;
        };
        if let ChangeRow::Directory {
            path, collapsed, ..
        } = &rows[index]
            && *collapsed == open
            && let Some(repository) = self.changes.get(rows[index].repository_index())
        {
            let repository_id = repository.id;
            self.toggle_directory(staged, repository_id, path.clone(), cx);
        }
    }

    fn discard_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((staged, rows, index)) = self.selected_row(cx) else {
            return;
        };
        if staged {
            return;
        }
        let Some(repository) = self.changes.get(rows[index].repository_index()) else {
            return;
        };
        let repository = repository.repository.clone();
        let files = self.row_files(staged, &rows[index]);
        self.discard(&repository, files, window, cx);
    }

    fn handle_list_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" && self.autoscroll.is_some() {
            self.stop_autoscroll(cx);
            cx.stop_propagation();
            return;
        }
        if !self.list_focus_handle.is_focused(window) || event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.move_selection(1, cx),
            "up" => self.move_selection(-1, cx),
            "space" => self.toggle_selected_staged(cx),
            "enter" => self.open_selected(window, cx),
            "delete" => self.discard_selected(window, cx),
            "left" => self.set_selected_directory_open(false, cx),
            "right" => self.set_selected_directory_open(true, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn start_autoscroll(
        &mut self,
        staged: bool,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(AUTOSCROLL_FRAME).await;
                let Ok(true) =
                    this.update_in(cx, |this, window, cx| this.step_autoscroll(window, cx))
                else {
                    break;
                };
            }
        });
        self.autoscroll = Some(ListAutoscroll {
            staged,
            anchor,
            moved: false,
            _task: task,
        });
        cx.notify();
    }

    fn stop_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll.take().is_some() {
            cx.notify();
        }
    }

    /// Scrolls one frame's worth towards the pointer, faster the farther it is from where
    /// the middle button was pressed. Returns whether the autoscroll is still running.
    fn step_autoscroll(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(autoscroll) = self.autoscroll.as_mut() else {
            return false;
        };
        let offset = window.mouse_position().y - autoscroll.anchor.y;
        let beyond = offset.abs() - AUTOSCROLL_DEAD_ZONE;
        if beyond > px(0.) {
            autoscroll.moved = true;
            let step = (beyond * AUTOSCROLL_SPEED).min(AUTOSCROLL_MAX_STEP);
            let handle = if autoscroll.staged {
                &self.staged_scroll_handle
            } else {
                &self.unstaged_scroll_handle
            };
            let current = handle.offset();
            let max = handle.max_offset();
            // A pointer below the anchor moves down the list, which is a more negative offset.
            let y = if offset > px(0.) {
                current.y - step
            } else {
                current.y + step
            };
            handle.set_offset(point(current.x, y.clamp(-max.y, px(0.))));
            cx.notify();
        }
        true
    }

    /// The marker at the autoscroll anchor, plus the window-wide mouse listeners that end
    /// the autoscroll, wherever the pointer has gone.
    fn render_autoscroll_overlay(
        &self,
        anchor: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak = cx.weak_entity();
        let listeners = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                window.set_window_cursor_style(CursorStyle::ResizeUpDown);
                window.on_mouse_event({
                    let weak = weak.clone();
                    move |_: &MouseDownEvent, phase, _, cx| {
                        if phase != DispatchPhase::Capture {
                            return;
                        }
                        // The press only ends the autoscroll; it doesn't also open a file.
                        cx.stop_propagation();
                        weak.update(cx, |this, cx| this.stop_autoscroll(cx)).ok();
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture || event.button != MouseButton::Middle {
                        return;
                    }
                    weak.update(cx, |this, cx| {
                        if this
                            .autoscroll
                            .as_ref()
                            .is_some_and(|autoscroll| autoscroll.moved)
                        {
                            this.stop_autoscroll(cx);
                        }
                    })
                    .ok();
                });
            },
        )
        .absolute()
        .size_full();

        let size = px(24.);
        let marker = div()
            .size(size)
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .shadow_sm()
            .child(
                Icon::new(IconName::ChevronUpDown)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            );

        div().absolute().inset_0().child(listeners).child(deferred(
            anchored()
                .position(anchor - point(size / 2., size / 2.))
                .child(marker),
        ))
    }

    fn render_rows(
        &self,
        staged: bool,
        rows: &[ChangeRow],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if self.changes.is_empty() {
            return vec![render_placeholder("This project has no git repository.")];
        }
        if rows.is_empty() {
            let filtering = !self.filter_terms(cx).is_empty() || self.status_filter.is_some();
            return vec![render_placeholder(match (filtering, staged) {
                (true, _) => "No changes match the filter.",
                (false, true) => "No staged changes.",
                (false, false) => "No uncommitted changes.",
            })];
        }
        rows.iter()
            .enumerate()
            .filter_map(|(row_index, row)| {
                let repository = self.changes.get(row.repository_index())?;
                Some(match row {
                    ChangeRow::Repository {
                        repository_index,
                        shown,
                        filtering,
                    } => {
                        let files = repository.files(staged);
                        let shown = shown
                            .iter()
                            .filter_map(|&file_index| files.get(file_index).cloned())
                            .collect::<Vec<_>>();
                        render_repository_header(
                            *repository_index,
                            staged,
                            repository,
                            &shown,
                            *filtering,
                            cx,
                        )
                    }
                    ChangeRow::Directory { .. } => {
                        self.render_directory_row(row_index, staged, repository, row, cx)
                    }
                    ChangeRow::File {
                        file_index, depth, ..
                    } => {
                        let file = repository.files(staged).get(*file_index)?;
                        self.render_changed_file(row_index, staged, repository, file, *depth, cx)
                    }
                })
            })
            .collect()
    }

    fn render_directory_row(
        &self,
        row_index: usize,
        staged: bool,
        repository: &RepositoryChanges,
        row: &ChangeRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ChangeRow::Directory {
            name,
            depth,
            files,
            collapsed,
            path,
            ..
        } = row
        else {
            return Empty.into_any_element();
        };
        let selection = self.row_selection(staged, row);
        let is_selected = selection.is_some() && selection == self.selection;
        let repository_id = repository.id;
        let repository_handle = repository.repository.clone();
        let (id, stage_icon, stage_tooltip) = if staged {
            ("staged-directory", IconName::Dash, "Unstage Folder")
        } else {
            ("unstaged-directory", IconName::Plus, "Stage Folder")
        };
        let click_row = row.clone();
        let stage_row = row.clone();
        let discard_files = self.row_files(staged, row);
        let path = path.clone();
        ListItem::new((id, row_index))
            .spacing(ListItemSpacing::Dense)
            .indent_level(*depth)
            .indent_step_size(TREE_INDENT)
            .toggle_state(is_selected)
            .start_slot(
                Icon::new(if *collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Icon::new(if *collapsed {
                            IconName::Folder
                        } else {
                            IconName::FolderOpen
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(Label::new(name.clone()).size(LabelSize::Small).truncate())
                    .child(
                        Label::new(files.len().to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .end_slot(
                h_flex()
                    .gap_1()
                    .when(!staged, |this| {
                        let repository_handle = repository_handle.clone();
                        this.child(
                            IconButton::new(("discard-directory", row_index), IconName::Undo)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Discard Folder Changes"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.discard(
                                        &repository_handle,
                                        discard_files.clone(),
                                        window,
                                        cx,
                                    );
                                })),
                        )
                    })
                    .child(
                        IconButton::new((id, row_index), stage_icon)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(stage_tooltip))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.set_row_staged(staged, &stage_row, cx);
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let selection = this.row_selection(staged, &click_row);
                this.select_row(selection, window, cx);
                if event.modifiers().secondary(cx) {
                    this.set_row_staged(staged, &click_row, cx);
                } else {
                    this.toggle_directory(staged, repository_id, path.clone(), cx);
                }
            }))
            .into_any_element()
    }

    fn render_changed_file(
        &self,
        row_index: usize,
        staged: bool,
        repository: &RepositoryChanges,
        file: &ChangedFile,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let repository_handle = repository.repository.clone();
        let history_repository = repository.repository.clone();
        let history_repo_path = file.repo_path.clone();
        let repo_path = file.repo_path.clone();
        let menu_this = cx.weak_entity();
        let menu_repository = repository.repository.clone();
        let menu_file = file.clone();
        let selection = RowSelection {
            staged,
            repository_id: repository.id,
            target: RowTarget::File(file.repo_path.clone()),
        };
        let is_selected = self.selection.as_ref() == Some(&selection);
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
        let show_directory = !self.tree_view;
        let list_item = ListItem::new((id, row_index))
            .spacing(ListItemSpacing::Dense)
            .indent_level(depth)
            .indent_step_size(TREE_INDENT)
            .toggle_state(is_selected)
            .start_slot(git_status_icon(file.status))
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(Label::new(file.file_name.clone()).size(LabelSize::Small))
                    .when(show_directory, |this| {
                        this.children(file.directory.clone().map(|directory| {
                            Label::new(directory)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate()
                        }))
                    }),
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
                    .child({
                        let repository_handle = repository_handle.clone();
                        let repo_path = repo_path.clone();
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
                            }))
                    }),
            )
            // Ctrl-click (cmd-click on macOS) stages an uncommitted file and unstages a staged
            // one, so a long list can be sorted out without aiming at the small buttons.
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select_row(Some(selection.clone()), window, cx);
                if event.modifiers().secondary(cx) {
                    this.set_staged(&repository_handle, vec![repo_path.clone()], !staged, cx);
                } else {
                    this.open_file_changes(
                        &history_repository,
                        history_repo_path.clone(),
                        window,
                        cx,
                    );
                }
            }));

        right_click_menu((
            if staged {
                "staged-change-menu"
            } else {
                "unstaged-change-menu"
            },
            row_index,
        ))
        .trigger(move |_, _, _| list_item)
        .maybe_menu(move |window, cx| {
            let this = menu_this.upgrade()?;
            Some(this.update(cx, |this, cx| {
                this.file_context_menu(
                    menu_repository.clone(),
                    menu_file.clone(),
                    staged,
                    window,
                    cx,
                )
            }))
        })
        .into_any_element()
    }
    fn file_context_menu(
        &self,
        repository: WeakEntity<Repository>,
        file: ChangedFile,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let this = cx.entity();
        let is_untracked = file.status.is_untracked();
        ContextMenu::build(window, cx, move |menu, window, _| {
            menu.entry("Open File", None, {
                let project_path = file.project_path.clone();
                window.handler_for(&this, move |this, window, cx| {
                    this.open_file(project_path.clone(), window, cx);
                })
            })
            .entry("Open Diff", None, {
                let repository = repository.clone();
                let project_path = file.project_path.clone();
                window.handler_for(&this, move |this, window, cx| {
                    this.open_project_diff(&repository, project_path.clone(), window, cx);
                })
            })
            .item(
                ContextMenuEntry::new("File History")
                    .disabled(is_untracked)
                    .handler({
                        let repository = repository.clone();
                        let repo_path = file.repo_path.clone();
                        window.handler_for(&this, move |this, window, cx| {
                            this.open_file_history(&repository, repo_path.clone(), window, cx);
                        })
                    }),
            )
            .separator()
            .entry(if staged { "Unstage File" } else { "Stage File" }, None, {
                let repository = repository.clone();
                let repo_path = file.repo_path.clone();
                window.handler_for(&this, move |this, _, cx| {
                    this.set_staged(&repository, vec![repo_path.clone()], !staged, cx);
                })
            })
            .entry(
                if file.status.is_created() {
                    "Trash File…"
                } else {
                    "Discard Changes…"
                },
                None,
                {
                    let repository = repository.clone();
                    let file = file.clone();
                    window.handler_for(&this, move |this, window, cx| {
                        this.discard(&repository, vec![file.clone()], window, cx);
                    })
                },
            )
            .entry("Stash File", None, {
                let repository = repository.clone();
                let repo_path = file.repo_path.clone();
                window.handler_for(&this, move |this, _, cx| {
                    this.stash_files(&repository, vec![repo_path.clone()], cx);
                })
            })
            .when(is_untracked, |menu| {
                menu.entry("Add to .gitignore", None, {
                    let repository = repository.clone();
                    let repo_path = file.repo_path.clone();
                    window.handler_for(&this, move |this, _, cx| {
                        this.add_to_gitignore(&repository, repo_path.clone(), cx);
                    })
                })
            })
            .separator()
            .entry("Copy Relative Path", None, {
                let repo_path = file.repo_path.as_unix_str().to_string();
                move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(repo_path.clone()))
            })
        })
    }

    fn open_file(
        &self,
        project_path: Option<ProjectPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project_path) = project_path else {
            return;
        };
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace
                        .open_path(project_path, None, true, window, cx)
                        .detach_and_log_err(cx);
                })
                .log_err();
        });
    }

    /// Opens the file's uncommitted changes with what earlier commits changed in it below.
    fn open_file_changes(
        &self,
        repository: &WeakEntity<Repository>,
        repo_path: RepoPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = repository.upgrade() else {
            return;
        };
        let workspace = self.workspace.clone();
        // Opening the view deactivates the tab showing this list, which updates it, and
        // the click is handled while that tab may still be updating.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    FileHistoryView::open(repo_path, repository, workspace, window, cx);
                })
                .log_err();
        });
    }

    fn open_project_diff(
        &self,
        repository: &WeakEntity<Repository>,
        project_path: Option<ProjectPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.workspace.clone();
        let repository = repository.clone();
        // Opening the diff deactivates the tab showing this list, which updates it, and
        // the menu is handled while that tab may still be updating.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| match project_path {
                    Some(project_path) => {
                        ProjectDiff::deploy_at_project_path(workspace, project_path, window, cx)
                    }
                    None => {
                        ProjectDiff::deploy_at(workspace, None, window, cx);
                        // `deploy_at` shows the active repository, which may not be the
                        // one this file belongs to.
                        let project_diff = workspace.items_of_type::<ProjectDiff>(cx).next();
                        if let Some(repository) = repository.upgrade()
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
    }

    fn open_file_history(
        &self,
        repository: &WeakEntity<Repository>,
        repo_path: RepoPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = repository.upgrade() else {
            return;
        };
        let repo_id = repository.read(cx).id;
        let git_store = self.git_store.clone();
        let workspace = self.workspace.clone();
        // This list is shown inside the git graph, which opening the history
        // updates, and the menu is handled while that graph may be updating.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    crate::git_graph::open_or_reuse_graph(
                        workspace,
                        repo_id,
                        git_store,
                        LogSource::Path(repo_path),
                        None,
                        window,
                        cx,
                    );
                })
                .log_err();
        });
    }

    /// Stashes just these files, untracked ones included, leaving the rest
    /// of the work tree as it is.
    fn stash_files(
        &mut self,
        repository: &WeakEntity<Repository>,
        repo_paths: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = repository.upgrade() else {
            return;
        };
        let task = repository.update(cx, |repository, cx| {
            repository.stash_entries(repo_paths, None, cx)
        });
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            if let Err(error) = task.await {
                cx.update(|cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        show_error_toast(workspace, "stash", error, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn add_to_gitignore(
        &mut self,
        repository: &WeakEntity<Repository>,
        repo_path: RepoPath,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = repository.upgrade() else {
            return;
        };
        let receiver = repository.update(cx, |repository, _| {
            repository.add_path_to_gitignore(&repo_path, false)
        });
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            if let Err(error) = receiver.await? {
                cx.update(|cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        show_error_toast(workspace, "add to .gitignore", error, cx);
                    }
                });
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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
        if !has_staged_changes(repository.read(cx)) {
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
        let commit_target = self.render_commit_target(repository.as_ref(), cx);
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
        let unpushed_commits = repository
            .as_ref()
            .and_then(|repository| repository.read(cx).branch.as_ref())
            .and_then(|branch| branch.upstream.as_ref())
            .and_then(|upstream| upstream.tracking.status())
            .map_or(0, |status| status.ahead);
        let push_label: SharedString = if unpushed_commits > 0 {
            format!("Push {unpushed_commits}").into()
        } else {
            "Push".into()
        };
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
                    .children(commit_target)
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
                        Button::new("push", push_label)
                            .start_icon(Icon::new(IconName::ArrowUp).size(IconSize::Small))
                            .disabled(repository.is_none())
                            .on_click({
                                // The git panel owns pushing: it picks the remote, asks for
                                // credentials and reports the result. `git::Push` would push
                                // the active repository rather than this one.
                                let workspace = self.workspace.clone();
                                let repository = repository.clone();
                                move |_, window, cx| {
                                    crate::git_graph::with_git_panel(
                                        &workspace,
                                        repository.clone(),
                                        window,
                                        cx,
                                        |panel, repository, window, cx| {
                                            panel.push_repository(
                                                repository, false, false, window, cx,
                                            )
                                        },
                                    );
                                }
                            }),
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

    /// The layout is global and survives restarts, like the git graph's detail panel position,
    /// so it applies to every git graph's list alike.
    fn restore_sections_side_by_side(cx: &App) -> bool {
        KeyValueStore::global(cx)
            .read_kvp(SECTIONS_SIDE_BY_SIDE_KEY)
            .log_err()
            .flatten()
            .is_some_and(|value| value == "true")
    }

    fn toggle_sections_layout(&mut self, cx: &mut Context<Self>) {
        self.sections_side_by_side = !self.sections_side_by_side;
        let value = self.sections_side_by_side.to_string();
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(SECTIONS_SIDE_BY_SIDE_KEY.to_string(), value)
                .await
                .log_err()
        })
        .detach();
        cx.notify();
    }

    fn render_sections_layout_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, tooltip) = if self.sections_side_by_side {
            (IconName::Split, "Stack Staged Changes Below")
        } else {
            (IconName::SplitAlt, "Show Staged Changes Side by Side")
        };
        IconButton::new("toggle-sections-layout", icon)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text(tooltip))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sections_layout(cx)))
            .into_any_element()
    }

    /// The search over both lists, the status filters with their counts, and the
    /// tree or flat layout. The git graph shows them beside its own buttons for the
    /// uncommitted changes, rather than taking a row of their own above the lists.
    pub(crate) fn render_filter_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut seen = HashSet::default();
        let mut counts = [0usize; StatusFilter::ALL.len()];
        for repository in &self.changes {
            for file in repository.unstaged.iter().chain(&repository.staged) {
                if !seen.insert((repository.id, file.repo_path.clone())) {
                    continue;
                }
                for (count, filter) in counts.iter_mut().zip(StatusFilter::ALL) {
                    if filter.matches(file.status) {
                        *count += 1;
                    }
                }
            }
        }
        let border_color = cx.theme().colors().border_variant;
        let (tree_icon, tree_tooltip) = if self.tree_view {
            (IconName::ListTree, "Show Files as a List")
        } else {
            (IconName::ListTree, "Group Files by Folder")
        };
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_1()
            .flex_wrap()
            .child(
                div()
                    .flex_1()
                    .min_w(px(120.))
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(border_color)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.filter_editor.clone()),
            )
            .children(
                StatusFilter::ALL
                    .iter()
                    .zip(counts)
                    .enumerate()
                    .filter(|(_, (_, count))| *count > 0)
                    .map(|(index, (filter, count))| {
                        let filter = *filter;
                        Button::new(
                            ("status-filter", index),
                            format!("{} {count}", filter.label()),
                        )
                        .label_size(LabelSize::XSmall)
                        .toggle_state(self.status_filter == Some(filter))
                        .tooltip(Tooltip::text(format!(
                            "Show only {} files",
                            filter.label().to_lowercase()
                        )))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.status_filter = if this.status_filter == Some(filter) {
                                None
                            } else {
                                Some(filter)
                            };
                            cx.notify();
                        }))
                    }),
            )
            .child(
                IconButton::new("toggle-tree-view", tree_icon)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.tree_view)
                    .tooltip(Tooltip::text(tree_tooltip))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_tree_view(cx))),
            )
            .into_any_element()
    }

    fn restore_tree_view(cx: &App) -> bool {
        KeyValueStore::global(cx)
            .read_kvp(TREE_VIEW_KEY)
            .log_err()
            .flatten()
            .is_none_or(|value| value == "true")
    }

    fn toggle_tree_view(&mut self, cx: &mut Context<Self>) {
        self.tree_view = !self.tree_view;
        let value = self.tree_view.to_string();
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(TREE_VIEW_KEY.to_string(), value)
                .await
                .log_err()
        })
        .detach();
        cx.notify();
    }

    fn render_section(
        &self,
        staged: bool,
        header_end: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (id, title) = if staged {
            ("working-tree-staged-changes", "Staged Changes")
        } else {
            ("working-tree-unstaged-changes", "Uncommitted Changes")
        };
        let rows = self.build_rows(staged, cx);
        let rows = self.render_rows(staged, &rows, cx);
        v_flex()
            .flex_1()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
                    .children(header_end),
            )
            .child(
                div()
                    .id(id)
                    .flex_1()
                    .min_h_0()
                    .p_1()
                    .overflow_y_scroll()
                    .track_scroll(self.scroll_handle(staged))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            if this.autoscroll.is_none() {
                                this.start_autoscroll(staged, event.position, window, cx);
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .children(rows),
            )
            .into_any_element()
    }
}

fn has_staged_changes(repository: &Repository) -> bool {
    repository
        .cached_status()
        .any(|entry| entry.status.staging().has_staged())
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
                repository.checkout_files(Some("HEAD"), repo_paths, cx)
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

/// The repository's name and count, with buttons that act on the files shown: all of
/// them, or those a search or status filter lets through.
fn render_repository_header(
    repository_index: usize,
    staged: bool,
    repository: &RepositoryChanges,
    files: &[ChangedFile],
    filtering: bool,
    cx: &mut Context<WorkingTreeChanges>,
) -> AnyElement {
    let repository_handle = repository.repository.clone();
    let repo_paths = files
        .iter()
        .map(|file| file.repo_path.clone())
        .collect::<Vec<_>>();
    let (id, label) = match (staged, filtering) {
        (true, false) => ("unstage-all", "Unstage All"),
        (true, true) => ("unstage-all", "Unstage Shown"),
        (false, false) => ("stage-all", "Stage All"),
        (false, true) => ("stage-all", "Stage Shown"),
    };
    let discard_label = if filtering {
        "Discard Shown"
    } else {
        "Discard All"
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
                        Button::new(("discard-all", repository_index), discard_label)
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
        let layout_toggle = self.render_sections_layout_toggle(cx);
        let unstaged_changes = self.render_section(false, Some(layout_toggle), cx);
        let staged_changes = self.render_section(true, None, cx);
        let autoscroll_anchor = self.autoscroll.as_ref().map(|autoscroll| autoscroll.anchor);
        let side_by_side = self.sections_side_by_side;
        let border_color = cx.theme().colors().border_variant;
        v_flex()
            .key_context("WorkingTreeChanges")
            .track_focus(&self.focus_handle)
            // Enter in the commit title commits, instead of reaching the view this list is in.
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                if this.list_focus_handle.is_focused(window) {
                    this.open_selected(window, cx);
                } else {
                    this.commit(window, cx);
                }
            }))
            // Bound to ctrl-enter in both commit editors. Handled here so it commits this
            // form's repository rather than reaching the git panel's handler.
            .on_action(cx.listener(|this, _: &git::Commit, window, cx| {
                this.commit(window, cx);
            }))
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(
                div()
                    .track_focus(&self.list_focus_handle)
                    .on_key_down(cx.listener(Self::handle_list_key_down))
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .map(|this| {
                        if side_by_side {
                            this.flex_row()
                        } else {
                            this.flex_col()
                        }
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .child(unstaged_changes),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .map(|this| {
                                if side_by_side {
                                    this.border_l_1()
                                } else {
                                    this.border_t_1()
                                }
                            })
                            .border_color(border_color)
                            .child(staged_changes),
                    ),
            )
            .child(div().flex_none().child(self.render_commit_form(cx)))
            .when_some(autoscroll_anchor, |this, anchor| {
                this.child(self.render_autoscroll_overlay(anchor, cx))
            })
    }
}
