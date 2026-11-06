use git::repository::LogSource;
use git_ui::git_graph::{self, GitGraph};
use gpui::{
    Action as _, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, WeakEntity, Window,
};
use project::git_store::{GitStore, GitStoreEvent};
use ui::prelude::*;
use workspace::{
    MultiWorkspace, Workspace,
    item::{Item, ItemEvent},
};

/// The permanent first tab: the project's git graph. The project's files, threads and Quarry are
/// in the Workspaces panel.
///
/// The graph needs a repository to be created, and the project's git scan usually finishes after
/// the tab is added, so the tab waits for one and creates the graph when it appears.
pub struct GitPage {
    workspace: WeakEntity<Workspace>,
    git_store: Entity<GitStore>,
    git_graph: Option<Entity<GitGraph>>,
    focus_handle: FocusHandle,
    _git_store_subscription: Subscription,
    _git_graph_subscription: Option<Subscription>,
}

impl GitPage {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        git_store: Entity<GitStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store_subscription = cx.subscribe_in(
            &git_store,
            window,
            |this, _git_store, event: &GitStoreEvent, window, cx| {
                if matches!(
                    event,
                    GitStoreEvent::ActiveRepositoryChanged(_) | GitStoreEvent::RepositoryAdded
                ) {
                    this.ensure_graph(window, cx);
                }
            },
        );
        let mut this = Self {
            workspace,
            git_store,
            git_graph: None,
            focus_handle: cx.focus_handle(),
            _git_store_subscription: git_store_subscription,
            _git_graph_subscription: None,
        };
        this.ensure_graph(window, cx);
        this
    }

    /// The graph, created the first time the project has an active repository. It comes back to
    /// the repository and branch it was last left on, across restarts too.
    fn ensure_graph(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<GitGraph>> {
        if let Some(git_graph) = &self.git_graph {
            return Some(git_graph.clone());
        }
        let active_repo_id = self.git_store.read(cx).active_repository()?.read(cx).id;
        let (repo_id, log_source) = git_graph::remembered_project_graph_target(&self.git_store, cx)
            .unwrap_or((active_repo_id, LogSource::All));
        let git_store = self.git_store.clone();
        let workspace = self.workspace.clone();
        let git_graph =
            cx.new(|cx| GitGraph::new(repo_id, git_store, workspace, Some(log_source), window, cx));
        let had_focus = self.focus_handle.is_focused(window);
        // The tab shows the graph's title, which follows the repository it is on.
        self._git_graph_subscription =
            Some(cx.subscribe(&git_graph, |_, _, event: &ItemEvent, cx| {
                if *event == ItemEvent::UpdateTab {
                    cx.emit(ItemEvent::UpdateTab);
                }
            }));
        self.git_graph = Some(git_graph.clone());
        if had_focus {
            window.focus(&git_graph.focus_handle(cx), cx);
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
        Some(git_graph)
    }
}

/// Registered in place of the git graph's and Quarry's own handlers, which open them in tabs of
/// their own. Those are still used for a workspace without the Git tab or the Workspaces panel.
pub fn register_actions(workspace: &mut Workspace) {
    workspace
        .register_action(|workspace, _: &git_graph::Open, window, cx| {
            toggle_git_and_search(workspace, window, cx);
        })
        .register_action(|workspace, action: &git_graph::OpenAtCommit, window, cx| {
            show_git_graph(workspace, Some(action.sha.clone()), window, cx);
        })
        .register_action(|workspace, _: &quarry::panel::ToggleFocus, window, cx| {
            focus_databases(workspace, window, cx);
        });
}

fn show_git_graph(
    workspace: &mut Workspace,
    sha: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(git_page) = workspace.items_of_type::<GitPage>(cx).next() else {
        let Some((active_repo_id, git_store)) = git_graph::active_repository_target(workspace, cx)
        else {
            return;
        };
        let (repo_id, log_source) = match sha {
            Some(_) => (active_repo_id, LogSource::All),
            None => git_graph::remembered_project_graph_target(&git_store, cx)
                .unwrap_or((active_repo_id, LogSource::All)),
        };
        git_graph::open_or_reuse_graph(workspace, repo_id, git_store, log_source, sha, window, cx);
        return;
    };
    workspace.activate_item(&git_page, true, true, window, cx);
    // A commit to show belongs to the active repository.
    if sha.is_some()
        && let Some((active_repo_id, _)) = git_graph::active_repository_target(workspace, cx)
        && let Some(git_graph) =
            git_page.update(cx, |git_page, cx| git_page.ensure_graph(window, cx))
    {
        git_graph::retarget_project_graph(&git_graph, active_repo_id, sha, cx);
    }
}

/// The git graph's shortcut moves between the Git tab and the Workspaces panel's search: from
/// anywhere else it brings up the Git tab, from the Git tab it opens the search, and from the
/// search it clears and closes it and goes back to the Git tab.
fn toggle_git_and_search(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if MultiWorkspace::is_search_focused_in_sidebar(window, cx) {
        MultiWorkspace::clear_search_in_sidebar(window, cx);
        show_git_graph(workspace, None, window, cx);
        return;
    }
    let git_tab_focused = workspace
        .items_of_type::<GitPage>(cx)
        .next()
        .is_some_and(|git_page| {
            is_active_item(workspace, &git_page, cx)
                && git_page
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
        });
    if git_tab_focused && MultiWorkspace::toggle_search_in_sidebar(window, cx) {
        return;
    }
    show_git_graph(workspace, None, window, cx);
}

fn is_active_item(workspace: &Workspace, git_page: &Entity<GitPage>, cx: &App) -> bool {
    workspace
        .active_item(cx)
        .is_some_and(|item| item.item_id() == git_page.entity_id())
}

/// Quarry's tree is in the Workspaces panel's Databases region; a window without that panel
/// opens Quarry as a tab.
fn focus_databases(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !MultiWorkspace::focus_databases_in_sidebar(window, cx) {
        quarry::panel::open(workspace, window, cx);
    }
}

impl Render for GitPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.git_graph {
            Some(git_graph) => git_graph.clone().into_any_element(),
            None => v_flex()
                .track_focus(&self.focus_handle)
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Label::new("No Git repository in this project").color(Color::Muted))
                .child(
                    Button::new("git-page-init", "Initialize Repository").on_click(
                        |_, window, cx| {
                            window.dispatch_action(git::Init.boxed_clone(), cx);
                        },
                    ),
                )
                .into_any_element(),
        }
    }
}

impl Focusable for GitPage {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.git_graph {
            Some(git_graph) => git_graph.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<ItemEvent> for GitPage {}

impl Item for GitPage {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        match &self.git_graph {
            Some(git_graph) => git_graph.read(cx).tab_content_text(detail, cx),
            None => "Git".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitGraph))
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn is_permanent(&self, _cx: &App) -> bool {
        true
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The graph saves the size of its split when it is left.
        if let Some(git_graph) = &self.git_graph {
            git_graph.update(cx, |git_graph, cx| git_graph.deactivated(window, cx));
        }
    }
}
