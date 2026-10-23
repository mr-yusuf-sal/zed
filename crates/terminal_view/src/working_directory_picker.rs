use std::{cmp, path::PathBuf, sync::Arc};

use futures::channel::oneshot;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Subscription, Task, WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

/// The directories a new terminal can start in: the roots of every local
/// project in the window, the shown project's first. A remote project only
/// offers its own roots, since its shell can't start on another host.
pub(crate) fn working_directory_choices(workspace: &Workspace, cx: &App) -> Vec<PathBuf> {
    let current = workspace.project_group_key(cx);
    let mut keys = vec![current.clone()];
    if workspace.project().read(cx).is_local()
        && let Some(multi_workspace) = workspace
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
    {
        keys.extend(
            multi_workspace
                .read(cx)
                .project_group_keys()
                .into_iter()
                .filter(|key| key.host().is_none()),
        );
    }
    let mut directories = Vec::new();
    for key in keys {
        for path in key.path_list().ordered_paths() {
            if !directories.contains(path) {
                directories.push(path.clone());
            }
        }
    }
    directories
}

/// Asks which of `directories` a new terminal should start in. Resolves to
/// `None` when the picker is dismissed.
pub(crate) fn pick_working_directory(
    directories: Vec<PathBuf>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Task<Option<PathBuf>> {
    window.spawn(cx, async move |cx| {
        let (sender, receiver) = oneshot::channel();
        let delegate = WorkingDirectoryPickerDelegate::new(directories, sender);
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    WorkingDirectoryPicker::new(delegate, window, cx)
                })
            })
            .ok()?;
        receiver.await.ok()
    })
}

struct WorkingDirectoryPicker {
    picker: Entity<Picker<WorkingDirectoryPickerDelegate>>,
    _subscription: Subscription,
}

impl WorkingDirectoryPicker {
    fn new(
        delegate: WorkingDirectoryPickerDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(40.)));
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        Self {
            picker,
            _subscription,
        }
    }
}

impl ModalView for WorkingDirectoryPicker {}
impl EventEmitter<DismissEvent> for WorkingDirectoryPicker {}

impl Focusable for WorkingDirectoryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for WorkingDirectoryPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .child(self.picker.clone())
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.picker.update(cx, |picker, cx| {
                    picker.cancel(&Default::default(), window, cx);
                })
            }))
    }
}

struct WorkingDirectoryPickerDelegate {
    directories: Vec<PathBuf>,
    labels: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    sender: Option<oneshot::Sender<PathBuf>>,
}

impl WorkingDirectoryPickerDelegate {
    fn new(directories: Vec<PathBuf>, sender: oneshot::Sender<PathBuf>) -> Self {
        let labels = directories
            .iter()
            .map(|directory| match directory.file_name() {
                Some(name) => format!("{} — {}", name.to_string_lossy(), directory.display()),
                None => directory.display().to_string(),
            })
            .collect();
        Self {
            directories,
            labels,
            matches: Vec::new(),
            selected_index: 0,
            sender: Some(sender),
        }
    }
}

impl PickerDelegate for WorkingDirectoryPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "terminal working directory picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Start the terminal in…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let candidates = self
            .labels
            .iter()
            .enumerate()
            .map(|(index, label)| StringMatchCandidate::new(index, label))
            .collect::<Vec<_>>();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                fuzzy::match_strings(
                    &candidates,
                    &query,
                    true,
                    true,
                    100,
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await
            };
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    delegate.selected_index = cmp::min(
                        delegate.selected_index,
                        delegate.matches.len().saturating_sub(1),
                    );
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(directory) = self
            .matches
            .get(self.selected_index)
            .and_then(|hit| self.directories.get(hit.candidate_id))
        else {
            return;
        };
        if let Some(sender) = self.sender.take() {
            sender.send(directory.clone()).ok();
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(index)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    hit.string.clone(),
                    hit.positions.clone(),
                )),
        )
    }
}
