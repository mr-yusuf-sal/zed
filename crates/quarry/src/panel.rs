use crate::{
    connection_store::{ConnectionStore, parse_connection_url},
    driver::{ConnectionConfig, Driver, ObjectKind, ObjectNode},
    query_view::QueryView,
    runtime::QuarryRuntime,
};
use anyhow::Result;
use collections::HashMap;
use editor::Editor;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, Render, Task, WeakEntity, Window, actions, px,
};
use std::sync::Arc;
use ui::{Color, Icon, IconName, Label, SharedString, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    quarry,
    [
        /// Shows the Quarry panel.
        ToggleFocus,
        /// Starts adding a connection.
        AddConnection,
    ]
);

const PREVIEW_ROW_LIMIT: usize = 500;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<QuarryPanel>(window, cx);
        });
    })
    .detach();
}

/// One saved connection and, once opened, the live driver and the part of its object tree that
/// has been expanded so far. The tree is filled in lazily: asking a server for every schema,
/// table and column up front is slow on a real database and most of it is never looked at.
struct ConnectionState {
    driver: Option<Arc<dyn Driver>>,
    expanded: Vec<SharedString>,
    children: HashMap<Vec<SharedString>, Vec<ObjectNode>>,
    error: Option<SharedString>,
    busy: bool,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self {
            driver: None,
            expanded: Vec::new(),
            children: HashMap::default(),
            error: None,
            busy: false,
        }
    }
}

/// The add-connection form: a URL and an optional password, kept together so the password is
/// written to the credential store at the same moment the connection is saved.
struct Draft {
    url: Entity<Editor>,
    password: Entity<Editor>,
    error: Option<SharedString>,
}

fn new_password_editor(window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text("password (optional)", window, cx);
        // Masked so a shoulder or a screen share does not read it; the value still goes
        // straight to the OS credential store and nowhere else.
        editor.set_masked(true, cx);
        editor
    })
}

pub struct QuarryPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    store: Entity<ConnectionStore>,
    states: Vec<ConnectionState>,
    /// Present while the user is filling in the add-connection form.
    draft: Option<Draft>,
    /// Set while changing the password of a connection that already exists.
    password_prompt: Option<(usize, Entity<Editor>)>,
    width: Option<Pixels>,
    position: DockPosition,
    _tasks: Vec<Task<()>>,
}

impl QuarryPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<ConnectionStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let count = store.read(cx).connections().len();
        cx.observe(&store, |this, store, cx| {
            // Keep one state per saved connection; a newly added one starts disconnected.
            let count = store.read(cx).connections().len();
            this.states.resize_with(count, ConnectionState::default);
            cx.notify();
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            store,
            states: (0..count).map(|_| ConnectionState::default()).collect(),
            draft: None,
            password_prompt: None,
            width: None,
            position: DockPosition::Left,
            _tasks: Vec::new(),
        }
    }

    fn start_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("postgres://user@localhost:5432/database", window, cx);
            editor
        });
        url.focus_handle(cx).focus(window, cx);
        self.draft = Some(Draft {
            url,
            password: new_password_editor(window, cx),
            error: None,
        });
        cx.notify();
    }

    fn confirm_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.as_ref() else {
            return;
        };
        let text = draft.url.read(cx).text(cx);
        let config = match parse_connection_url(&text) {
            Ok(config) => config,
            Err(error) => {
                // Keep the form open with the text still in it so a typo can be corrected
                // rather than retyped, and say what was wrong where the user is looking.
                if let Some(draft) = self.draft.as_mut() {
                    draft.error = Some(format!("{error:#}").into());
                }
                cx.notify();
                return;
            }
        };

        let password = draft.password.read(cx).text(cx);
        self.draft = None;
        if !password.is_empty() {
            self.save_password(&config, password, cx);
        }
        self.store.update(cx, |store, cx| store.add(config, cx));
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Writes the password to the OS credential store, where `crate::connect` looks for it. It
    /// never enters the config, so it reaches neither the key-value store nor any file.
    fn save_password(
        &mut self,
        config: &ConnectionConfig,
        password: String,
        cx: &mut Context<Self>,
    ) {
        let config = config.clone();
        let username = config.username.clone().unwrap_or_default();
        let task = cx.spawn(async move |_, cx| {
            if let Err(error) = crate::save_password(&config, &username, &password, cx).await {
                log::error!("Quarry could not save the password: {error:#}");
            }
        });
        self._tasks.push(task);
    }

    fn start_password_prompt(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let editor = new_password_editor(window, cx);
        editor.focus_handle(cx).focus(window, cx);
        self.password_prompt = Some((index, editor));
        cx.notify();
    }

    fn confirm_password_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, editor)) = self.password_prompt.take() else {
            return;
        };
        let password = editor.read(cx).text(cx);
        if let Some(config) = self.store.read(cx).get(index).cloned() {
            self.save_password(&config, password, cx);
            // Drop the open driver so the next click reconnects with the new password instead
            // of keeping the session that was authenticated with the old one.
            if let Some(state) = self.states.get_mut(index) {
                state.driver = None;
                state.children.clear();
                state.expanded.clear();
                state.error = None;
            }
        }
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(config) = self.store.read(cx).get(index).cloned() else {
            return;
        };
        let Some(state) = self.states.get_mut(index) else {
            return;
        };
        if state.busy || state.driver.is_some() {
            return;
        }
        state.busy = true;
        state.error = None;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let opened = crate::connect(config, cx).await;
            this.update(cx, |this, cx| {
                let Some(state) = this.states.get_mut(index) else {
                    return;
                };
                state.busy = false;
                match opened {
                    Ok(driver) => {
                        state.driver = Some(driver);
                        this.load_children(index, Vec::new(), cx);
                    }
                    Err(error) => state.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self._tasks.push(task);
    }

    fn load_children(&mut self, index: usize, path: Vec<SharedString>, cx: &mut Context<Self>) {
        let Some(driver) = self
            .states
            .get(index)
            .and_then(|state| state.driver.clone())
        else {
            return;
        };
        let Some(runtime) = QuarryRuntime::global(cx) else {
            return;
        };

        let request = {
            let path = path.clone();
            runtime.spawn(async move { driver.children(&path).await })
        };
        let task = cx.spawn(async move |this, cx| {
            let children = request.await;
            this.update(cx, |this, cx| {
                let Some(state) = this.states.get_mut(index) else {
                    return;
                };
                match children {
                    Ok(children) => {
                        state.children.insert(path, children);
                    }
                    Err(error) => state.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        });
        self._tasks.push(task);
    }

    fn toggle_expanded(&mut self, index: usize, node: &ObjectNode, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(index) else {
            return;
        };
        let key = node.path.clone();
        if state.expanded.contains(&key.join("/").into()) {
            state.expanded.retain(|p| p.as_ref() != key.join("/"));
        } else {
            state.expanded.push(key.join("/").into());
            if !state.children.contains_key(&key) {
                self.load_children(index, key, cx);
                return;
            }
        }
        cx.notify();
    }

    fn is_expanded(&self, index: usize, path: &[SharedString]) -> bool {
        self.states.get(index).is_some_and(|state| {
            state
                .expanded
                .iter()
                .any(|expanded| expanded.as_ref() == path.join("/"))
        })
    }

    /// Opens a tab showing the first page of a table, which is what double-clicking an object
    /// in the tree is for. The query editor above it starts empty so it can be used straight
    /// away against the same connection.
    fn open_preview(
        &mut self,
        index: usize,
        node: &ObjectNode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(config) = self.store.read(cx).get(index).cloned() else {
            return;
        };
        let Some(driver) = self
            .states
            .get(index)
            .and_then(|state| state.driver.clone())
        else {
            return;
        };
        let Some(runtime) = QuarryRuntime::global(cx) else {
            return;
        };

        let name: SharedString = node.name.clone();
        let path = node.path.clone();
        let preview = {
            let driver = driver.clone();
            runtime.spawn(async move { driver.preview(&path, PREVIEW_ROW_LIMIT).await })
        };

        let workspace = self.workspace.clone();
        let task = cx.spawn(async move |_, cx| {
            let result = preview.await;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let view = cx.new(|cx| {
                        let mut view = QueryView::new(name, config.engine, driver, window, cx);
                        match result {
                            Ok(result) => view.show_result(result, cx),
                            Err(error) => log::error!("Quarry preview failed: {error:#}"),
                        }
                        view
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .ok();
        });
        self._tasks.push(task);
    }

    fn render_node(
        &self,
        index: usize,
        node: &ObjectNode,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.is_expanded(index, &node.path);
        let icon = match node.kind {
            ObjectKind::Database | ObjectKind::Schema => IconName::Folder,
            ObjectKind::Table | ObjectKind::Collection => IconName::Table,
            ObjectKind::View => IconName::Eye,
            ObjectKind::Column | ObjectKind::Key => IconName::Hash,
            ObjectKind::Index => IconName::ListTree,
        };
        let openable = matches!(
            node.kind,
            ObjectKind::Table | ObjectKind::View | ObjectKind::Collection
        );
        let node = node.clone();

        h_flex()
            .id(SharedString::from(format!(
                "quarry-{index}-{}",
                node.path.join("/")
            )))
            .w_full()
            .gap_1()
            .pr_1()
            .pl(px(8.0 + depth as f32 * 12.0))
            .py_0p5()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .when(node.has_children, |this| {
                this.child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
            })
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(Label::new(node.name.clone()).size(LabelSize::Small))
            .children(node.detail.clone().map(|detail| {
                Label::new(detail)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
            }))
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    if openable && event.click_count() >= 2 {
                        this.open_preview(index, &node, window, cx);
                    } else if node.has_children {
                        this.toggle_expanded(index, &node, cx);
                    }
                }),
            )
            .into_any_element()
    }

    fn render_subtree(
        &self,
        index: usize,
        path: &[SharedString],
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(state) = self.states.get(index) else {
            return Vec::new();
        };
        let Some(children) = state.children.get(path) else {
            return Vec::new();
        };

        let mut elements = Vec::new();
        for node in children.clone() {
            elements.push(self.render_node(index, &node, depth, cx));
            if self.is_expanded(index, &node.path) {
                elements.extend(self.render_subtree(index, &node.path, depth + 1, cx));
            }
        }
        elements
    }

    fn render_connection(
        &self,
        index: usize,
        config: &ConnectionConfig,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.states.get(index);
        let connected = state.is_some_and(|state| state.driver.is_some());
        let busy = state.is_some_and(|state| state.busy);
        let error = state.and_then(|state| state.error.clone());

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .id(SharedString::from(format!("quarry-connection-{index}")))
                    .w_full()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(IconName::Server)
                            .size(IconSize::XSmall)
                            .color(if connected {
                                Color::Success
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        Label::new(SharedString::from(config.name.clone())).size(LabelSize::Small),
                    )
                    .child(
                        Label::new(SharedString::from(config.engine.label()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when(busy, |this| {
                        this.child(
                            Label::new("connecting…")
                                .size(LabelSize::XSmall)
                                .color(Color::Info),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        IconButton::new(("quarry-password", index), IconName::Lock)
                            .icon_size(IconSize::XSmall)
                            .tooltip(|_, cx| Tooltip::simple("Set Password", cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_password_prompt(index, window, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.connect(index, cx);
                    })),
            )
            .children(error.map(|error| {
                Label::new(error)
                    .size(LabelSize::XSmall)
                    .color(Color::Error)
                    .into_any_element()
            }))
            .when(connected, |this| {
                this.children(self.render_subtree(index, &[], 1, cx))
            })
            .into_any_element()
    }
}

impl Focusable for QuarryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for QuarryPanel {}

impl Panel for QuarryPanel {
    fn persistent_name() -> &'static str {
        "QuarryPanel"
    }

    fn panel_key() -> &'static str {
        "QuarryPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        // Kept on the panel rather than written to settings.json: adding a settings key means
        // editing the central settings crate, which this fork has to keep rebasing.
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        self.width.unwrap_or(px(280.))
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Quarry")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        6
    }
}

impl Render for QuarryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let connections = self.store.read(cx).connections().to_vec();

        v_flex()
            .key_context("QuarryPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("Quarry")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("quarry-add", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(|_, cx| Tooltip::simple("Add Connection", cx))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_adding(window, cx);
                            })),
                    ),
            )
            .children(self.draft.as_ref().map(|draft| {
                v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .child(draft.url.clone())
                    .child(draft.password.clone())
                    .children(draft.error.clone().map(|error| {
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error)
                    }))
                    .child(
                        Label::new("Enter to save · a password in the URL is ignored")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.confirm_adding(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                        this.draft = None;
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }))
            .children(self.password_prompt.as_ref().map(|(index, editor)| {
                let name = self
                    .store
                    .read(cx)
                    .get(*index)
                    .map(|config| config.name.clone())
                    .unwrap_or_default();
                v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .child(
                        Label::new(SharedString::from(format!("Password for {name}")))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(editor.clone())
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.confirm_password_prompt(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                        this.password_prompt = None;
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }))
            .child(
                v_flex()
                    .id("quarry-connections")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .children(
                        connections
                            .iter()
                            .enumerate()
                            .map(|(index, config)| self.render_connection(index, config, cx))
                            .collect::<Vec<_>>(),
                    )
                    .when(connections.is_empty() && self.draft.is_none(), |this| {
                        this.child(
                            Label::new("No connections yet — use + to add one")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .into_any_element()
            .into_any()
    }
}

/// Opens the panel for a workspace. Returns an error rather than a panel when Quarry failed to
/// start, so the caller can leave it out of the dock instead of showing something inert.
pub async fn load(
    workspace: WeakEntity<Workspace>,
    mut cx: gpui::AsyncWindowContext,
) -> Result<Entity<QuarryPanel>> {
    cx.update(|_window, cx| {
        let store = ConnectionStore::global(cx)
            .ok_or_else(|| anyhow::anyhow!("the Quarry connection store is unavailable"))?;
        anyhow::Ok(cx.new(|cx| QuarryPanel::new(workspace, store, cx)))
    })?
}
