use std::{any::Any, sync::Arc};

use git::repository::LogSource;
use git_ui::{
    git_graph::{self, GitGraph},
    working_tree_changes::WorkingTreeChanges,
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Subscription,
    WeakEntity, Window,
};
use project::git_store::GitStore;
use project_panel::ProjectPanel;
use quarry::{connection_store::ConnectionStore, panel::QuarryPanel};
use ui::prelude::*;
use workspace::{
    ItemNavHistory, Workspace,
    item::{Item, ItemEvent},
};

/// The permanent "Main" tab: the project panel, the project's uncommitted and staged changes
/// with a commit form, and Quarry, side by side. The git graph opens in its place instead of in
/// a tab of its own.
pub struct MainPage {
    workspace: WeakEntity<Workspace>,
    view: MainView,
    project_panel: Entity<ProjectPanel>,
    working_tree_changes: Entity<WorkingTreeChanges>,
    /// Kept while the overview is shown, so coming back finds it as it was left.
    git_graph: Option<Entity<GitGraph>>,
    /// `None` only when the Quarry connection store failed to start.
    quarry: Option<Entity<QuarryPanel>>,
    nav_history: Option<ItemNavHistory>,
    _project_panel_focus: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainView {
    Overview,
    GitGraph,
}

impl MainView {
    /// The navigation history keeps one entry per item and row, so each view gets a row of its
    /// own for Go Back to return to it rather than drop it in favor of the latest one.
    fn history_row(self) -> u32 {
        match self {
            MainView::Overview => 0,
            MainView::GitGraph => 1,
        }
    }
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
        let quarry = match ConnectionStore::global(cx) {
            Some(store) => {
                let workspace = workspace.clone();
                Some(cx.new(|cx| QuarryPanel::new(workspace, store, window, cx)))
            }
            None => {
                log::error!("the Quarry connection store is unavailable");
                None
            }
        };
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
        Self {
            workspace,
            view: MainView::Overview,
            project_panel,
            working_tree_changes,
            git_graph: None,
            quarry,
            nav_history: None,
            _project_panel_focus: project_panel_focus,
        }
    }

    fn is_active(&self, cx: &Context<Self>) -> bool {
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
    let Some((active_repo_id, git_store)) = git_graph::active_repository_target(workspace, cx)
    else {
        return;
    };
    // A commit to show belongs to the active repository; otherwise the graph comes back to the
    // repository and branch it was last left on, across restarts too.
    let (repo_id, log_source) = match sha {
        Some(_) => (active_repo_id, LogSource::All),
        None => git_graph::remembered_project_graph_target(&git_store, cx)
            .unwrap_or((active_repo_id, LogSource::All)),
    };
    let Some(main_page) = main_page else {
        git_graph::open_or_reuse_graph(workspace, repo_id, git_store, log_source, sha, window, cx);
        return;
    };
    let workspace_handle = workspace.weak_handle();
    // A graph kept since it was last shown is already as it was left.
    let graph = match main_page.read(cx).git_graph.clone() {
        Some(graph) => graph,
        None => {
            let graph = cx.new(|cx| {
                GitGraph::new(
                    repo_id,
                    git_store,
                    workspace_handle,
                    Some(log_source),
                    window,
                    cx,
                )
            });
            main_page.update(cx, |main_page, _| {
                main_page.git_graph = Some(graph.clone());
            });
            graph
        }
    };
    if sha.is_some() {
        git_graph::retarget_project_graph(&graph, repo_id, sha, cx);
    }
    show(workspace, &main_page, MainView::GitGraph, window, cx);
}

/// Quarry is always on screen in the overview, so its shortcut only brings the overview up and
/// moves focus into it.
fn show_quarry(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(main_page) = workspace.items_of_type::<MainPage>(cx).next() else {
        quarry::panel::open(workspace, window, cx);
        return;
    };
    show(workspace, &main_page, MainView::Overview, window, cx);
    if let Some(quarry) = main_page.read(cx).quarry.clone() {
        window.focus(&quarry.focus_handle(cx), cx);
    }
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
            .children(self.quarry.clone().map(|quarry| {
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .border_l_1()
                    .border_color(border_color)
                    .child(quarry)
            }))
            .into_any_element()
    }
}

impl Render for MainPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match (self.view, &self.git_graph) {
            (MainView::GitGraph, Some(git_graph)) => git_graph.clone().into_any_element(),
            _ => self.render_overview(cx),
        }
    }
}

impl Focusable for MainPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match (self.view, &self.git_graph) {
            (MainView::GitGraph, Some(git_graph)) => git_graph.focus_handle(cx),
            _ => self.project_panel.read(cx).focus_handle(cx),
        }
    }
}

impl EventEmitter<ItemEvent> for MainPage {}

impl Item for MainPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        match (self.view, &self.git_graph) {
            (MainView::GitGraph, Some(git_graph)) => {
                git_graph.read(cx).tab_content_text(detail, cx)
            }
            _ => "Main".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        let icon = match self.view {
            MainView::Overview => IconName::FileTree,
            MainView::GitGraph => IconName::GitGraph,
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
