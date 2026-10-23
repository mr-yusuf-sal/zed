use crate::{
    connection_store::{ConnectionStore, connection_url, parse_connection_url},
    docker::DiscoveredServer,
    driver::{ConnectionConfig, Driver, Engine, Environment, KeyAction, ObjectKind, ObjectNode},
    query_view::{PREVIEW_ROW_LIMIT, QueryView, environment_badge},
    runtime::QuarryRuntime,
};
use anyhow::Result;
use collections::{BTreeMap, HashMap, HashSet};
use editor::Editor;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, MouseButton, MouseDownEvent, Pixels, Point, PromptLevel,
    Render, Subscription, Task, WeakEntity, Window, actions, anchored, deferred, px,
};
use std::sync::Arc;
use ui::{
    Color, ContextMenu, Icon, IconName, Label, PopoverMenu, SharedString, Tooltip, prelude::*,
};
use util::ResultExt as _;
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

const SELECT_ROW_LIMIT: usize = 100;

/// What the one-line prompt under the header is asking for.
#[derive(Clone)]
enum PromptKind {
    Password,
    /// Seconds until a Redis key expires.
    Ttl(ObjectNode),
    /// A Redis key's new full name.
    Rename(ObjectNode),
    /// A glob to SCAN for under a Redis database or prefix.
    FindKeys(ObjectNode),
}

struct InlinePrompt {
    /// The connection it is about. Positions shift when a connection is deleted, which is why
    /// deleting one closes the prompt.
    index: usize,
    kind: PromptKind,
    editor: Entity<Editor>,
}

/// A Redis key's full name, which the tree splits on `:` into its path after the database.
fn redis_key(node: &ObjectNode) -> String {
    node.path.get(1..).unwrap_or_default().join(":")
}

#[derive(Clone, Copy)]
enum Script {
    Definition,
    Create,
}

/// The actions on a saved connection, shared by its "⋯" button and its right-click menu.
fn connection_menu(
    menu: ContextMenu,
    panel: WeakEntity<QuarryPanel>,
    index: usize,
    connected: bool,
    busy: bool,
) -> ContextMenu {
    let update = |action: fn(&mut QuarryPanel, usize, &mut Window, &mut Context<QuarryPanel>)| {
        let panel = panel.clone();
        move |window: &mut Window, cx: &mut App| {
            panel
                .update(cx, |this, cx| action(this, index, window, cx))
                .log_err();
        }
    };
    menu.entry(
        "New Query",
        None,
        update(|this, index, window, cx| this.new_query(index, None, window, cx)),
    )
    .separator()
    .when(!busy, |menu| {
        menu.entry(
            "Refresh",
            None,
            update(|this, index, _, cx| this.refresh(index, cx)),
        )
    })
    .when(connected, |menu| {
        menu.entry(
            "Disconnect",
            None,
            update(|this, index, _, cx| this.disconnect(index, cx)),
        )
    })
    .separator()
    .entry(
        "Edit Connection",
        None,
        update(|this, index, window, cx| this.start_editing(index, window, cx)),
    )
    .entry(
        "Duplicate Connection",
        None,
        update(|this, index, _, cx| this.duplicate(index, cx)),
    )
    .entry(
        "Set Password",
        None,
        update(|this, index, window, cx| this.start_password_prompt(index, window, cx)),
    )
    .separator()
    .entry(
        "Delete Connection",
        None,
        update(|this, index, window, cx| this.confirm_delete(index, window, cx)),
    )
}

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
    /// What async work uses to find its way back to this state. A connection's position in the
    /// list shifts when one above it is deleted, and a refresh replaces the state outright, so a
    /// task that finishes late looks its state up by id and finds nothing rather than writing
    /// into a different connection.
    id: usize,
    driver: Option<Arc<dyn Driver>>,
    expanded: Vec<Vec<SharedString>>,
    children: HashMap<Vec<SharedString>, Vec<ObjectNode>>,
    error: Option<SharedString>,
    busy: bool,
}

impl ConnectionState {
    fn new(id: usize) -> Self {
        Self {
            id,
            driver: None,
            expanded: Vec::new(),
            children: HashMap::default(),
            error: None,
            busy: false,
        }
    }
}

/// The add/edit-connection form: a URL and an optional password, kept together so the password
/// is written to the credential store at the same moment the connection is saved.
struct Draft {
    name: Entity<Editor>,
    url: Entity<Editor>,
    password: Entity<Editor>,
    folder: Entity<Editor>,
    environment: Option<Environment>,
    error: Option<SharedString>,
    /// The id of the connection state being edited, or `None` when adding a new connection.
    editing: Option<usize>,
    test: TestState,
}

enum TestState {
    Untested,
    Testing,
    Succeeded,
    Failed(SharedString),
}

fn new_name_editor(name: Option<&str>, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text("name (optional — defaults to host/database)", window, cx);
        if let Some(name) = name {
            editor.set_text(name.to_string(), window, cx);
        }
        editor
    })
}

fn new_folder_editor(folder: Option<&str>, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text("folder (optional)", window, cx);
        if let Some(folder) = folder {
            editor.set_text(folder.to_string(), window, cx);
        }
        editor
    })
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
    next_state_id: usize,
    /// Present while the user is filling in the add- or edit-connection form.
    draft: Option<Draft>,
    /// Set while asking for one line of input: a new password, a key's TTL or name, a pattern.
    prompt: Option<InlinePrompt>,
    /// Narrows the tree to names containing this text. It filters what has been loaded; a
    /// folder that was never expanded is not searched, because that would mean asking the
    /// server for everything.
    filter: Entity<Editor>,
    width: Option<Pixels>,
    position: DockPosition,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    /// Set while `docker inspect` is running for "Add from Docker Containers".
    discovering: Option<Task<()>>,
    /// A one-line message under the header, such as why Docker discovery found nothing.
    notice: Option<(SharedString, Color)>,
    /// Folders whose connections are hidden. Not persisted: a restart opens every folder.
    collapsed_folders: HashSet<String>,
    _tasks: Vec<Task<()>>,
}

impl QuarryPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<ConnectionStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter loaded objects…", window, cx);
            editor
        });
        cx.subscribe(&filter, |_, _, event: &editor::EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                cx.notify();
            }
        })
        .detach();
        let count = store.read(cx).connections().len();
        cx.observe(&store, |this, store, cx| {
            // Keep one state per saved connection; a newly added one starts disconnected.
            // Deletion removes its state itself, so only additions show up here.
            let count = store.read(cx).connections().len();
            while this.states.len() < count {
                let state = this.new_state();
                this.states.push(state);
            }
            this.states.truncate(count);
            cx.notify();
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            store,
            states: (0..count).map(ConnectionState::new).collect(),
            next_state_id: count,
            draft: None,
            prompt: None,
            filter,
            width: None,
            position: DockPosition::Left,
            context_menu: None,
            collapsed_folders: HashSet::default(),
            discovering: None,
            notice: None,
            _tasks: Vec::new(),
        }
    }

    fn new_state(&mut self) -> ConnectionState {
        let id = self.next_state_id;
        self.next_state_id += 1;
        ConnectionState::new(id)
    }

    fn index_of(&self, state_id: usize) -> Option<usize> {
        self.states.iter().position(|state| state.id == state_id)
    }

    fn start_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let url = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("postgres://user@localhost:5432/database", window, cx);
            editor
        });
        url.focus_handle(cx).focus(window, cx);
        self.prompt = None;
        self.draft = Some(Draft {
            name: new_name_editor(None, window, cx),
            url,
            password: new_password_editor(window, cx),
            folder: new_folder_editor(None, window, cx),
            environment: None,
            error: None,
            editing: None,
            test: TestState::Untested,
        });
        cx.notify();
    }

    fn start_editing(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.store.read(cx).get(index).cloned() else {
            return;
        };
        let Some(state_id) = self.states.get(index).map(|state| state.id) else {
            return;
        };
        let url = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(connection_url(&config), window, cx);
            editor
        });
        url.focus_handle(cx).focus(window, cx);
        let password = new_password_editor(window, cx);
        password.update(cx, |editor, cx| {
            editor.set_placeholder_text("password (blank keeps the saved one)", window, cx);
        });
        self.prompt = None;
        self.draft = Some(Draft {
            name: new_name_editor(Some(&config.name), window, cx),
            url,
            password,
            folder: new_folder_editor(config.folder.as_deref(), window, cx),
            environment: config.environment,
            error: None,
            editing: Some(state_id),
            test: TestState::Untested,
        });
        cx.notify();
    }

    /// The config the form describes, or the reason it describes none. A typed name replaces
    /// the one derived from the URL.
    fn draft_config(&self, cx: &App) -> Option<Result<ConnectionConfig, SharedString>> {
        let draft = self.draft.as_ref()?;
        let text = draft.url.read(cx).text(cx);
        Some(match parse_connection_url(&text) {
            Ok(mut config) => {
                let name = draft.name.read(cx).text(cx);
                if !name.trim().is_empty() {
                    config.name = name.trim().to_string();
                }
                let folder = draft.folder.read(cx).text(cx);
                config.folder = Some(folder.trim().to_string()).filter(|folder| !folder.is_empty());
                config.environment = draft.environment;
                Ok(config)
            }
            Err(error) => Err(format!("{error:#}").into()),
        })
    }

    fn test_draft(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.draft_config(cx) else {
            return;
        };
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                draft.error = Some(error);
                cx.notify();
                return;
            }
        };
        let password = draft.password.read(cx).text(cx);
        let password = (!password.is_empty()).then_some(password);
        draft.error = None;
        draft.test = TestState::Testing;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let outcome = crate::test_connection(config, password, cx).await;
            this.update(cx, |this, cx| {
                if let Some(draft) = this.draft.as_mut() {
                    draft.test = match outcome {
                        Ok(()) => TestState::Succeeded,
                        Err(error) => TestState::Failed(format!("{error:#}").into()),
                    };
                }
                cx.notify();
            })
            .ok();
        });
        self._tasks.push(task);
    }

    fn confirm_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.draft_config(cx) else {
            return;
        };
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                // Keep the form open with the text still in it so a typo can be corrected
                // rather than retyped, and say what was wrong where the user is looking.
                if let Some(draft) = self.draft.as_mut() {
                    draft.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let Some(draft) = self.draft.as_ref() else {
            return;
        };

        let password = draft.password.read(cx).text(cx);
        let editing = draft.editing;
        self.draft = None;
        self.focus_handle.focus(window, cx);
        cx.notify();

        let Some(state_id) = editing else {
            if !password.is_empty() {
                self.save_password(&config, password, cx);
            }
            self.store.update(cx, |store, cx| store.add(config, cx));
            return;
        };
        // The connection may have been deleted while its form was open.
        let Some(index) = self.index_of(state_id) else {
            return;
        };
        let Some(previous) = self.store.read(cx).get(index).cloned() else {
            return;
        };
        self.store
            .update(cx, |store, cx| store.replace(index, config.clone(), cx));
        // Whatever the edit changed, the open session was made with the old settings.
        let fresh = self.new_state();
        if let Some(state) = self.states.get_mut(index) {
            *state = fresh;
        }
        if password.is_empty() {
            self.move_password(&previous, &config, cx);
        } else {
            self.save_password(&config, password, cx);
            self.forget_password_if_unused(&previous, cx);
        }
    }

    /// Asks Docker for running database containers and offers the ones not saved yet in a menu
    /// at `position`.
    fn discover_docker(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = Some(("Looking for database containers…".into(), Color::Muted));
        let discovery = cx.background_spawn(crate::docker::discover());
        self.discovering = Some(cx.spawn_in(window, async move |this, cx| {
            let discovered = discovery.await;
            this.update_in(cx, |this, window, cx| {
                this.discovering = None;
                this.notice = None;
                match discovered {
                    Ok(servers) => this.offer_discovered(servers, position, window, cx),
                    Err(error) => {
                        this.notice = Some((format!("{error:#}").into(), Color::Error));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn offer_discovered(
        &mut self,
        servers: Vec<DiscoveredServer>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let total = servers.len();
        let new: Vec<DiscoveredServer> = {
            let saved = self.store.read(cx).connections();
            servers
                .into_iter()
                .filter(|server| {
                    !saved.iter().any(|config| {
                        config.credential_url() == server.config.credential_url()
                            && config.username == server.config.username
                            && config.database == server.config.database
                    })
                })
                .collect()
        };
        if new.is_empty() {
            let notice = if total == 0 {
                "No running container publishes a PostgreSQL, SQL Server, MongoDB, Redis or \
                 ClickHouse port"
                    .to_string()
            } else {
                format!("All {total} database container(s) are already saved")
            };
            self.notice = Some((notice.into(), Color::Muted));
            return;
        }
        let panel = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.header("Docker Containers");
            if new.len() > 1 {
                let panel = panel.clone();
                let all = new.clone();
                menu = menu
                    .entry(format!("Add All {}", all.len()), None, move |_, cx| {
                        panel
                            .update(cx, |this, cx| {
                                for server in all.clone() {
                                    this.add_discovered(server, cx);
                                }
                            })
                            .log_err();
                    })
                    .separator();
            }
            for server in new {
                let panel = panel.clone();
                let label = format!(
                    "{} · {} {}:{}",
                    server.container,
                    server.config.engine.label(),
                    server.config.host,
                    server.config.port
                );
                menu = menu.entry(label, None, move |_, cx| {
                    panel
                        .update(cx, |this, cx| this.add_discovered(server.clone(), cx))
                        .log_err();
                });
            }
            menu
        });
        self.deploy_context_menu(position, menu, window, cx);
    }

    /// Saves a discovered container as a connection, filing the password its environment
    /// gave in the OS credential store like a typed one.
    fn add_discovered(&mut self, server: DiscoveredServer, cx: &mut Context<Self>) {
        if let Some(password) = server.password {
            self.save_password(&server.config, password, cx);
        }
        self.store
            .update(cx, |store, cx| store.add(server.config, cx));
    }

    /// Adds a copy of the connection, for pointing the same server at another database or
    /// user. The copy shares the saved password, which is filed by engine, host and port.
    fn duplicate(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(mut config) = self.store.read(cx).get(index).cloned() else {
            return;
        };
        config.name = format!("{} (copy)", config.name);
        self.store.update(cx, |store, cx| store.add(config, cx));
    }

    fn credential_in_use(&self, credential_url: &str, cx: &App) -> bool {
        self.store
            .read(cx)
            .connections()
            .iter()
            .any(|config| config.credential_url() == credential_url)
    }

    /// Carries the saved password over when an edit changes where it is filed (engine, host or
    /// port) or the username it was saved with. Without this, an edit that left the password
    /// field blank would silently lose the password — or, since a stored username wins over the
    /// config's, keep connecting as the old user.
    fn move_password(
        &mut self,
        previous: &ConnectionConfig,
        config: &ConnectionConfig,
        cx: &mut Context<Self>,
    ) {
        let old_url = previous.credential_url();
        let new_url = config.credential_url();
        if old_url == new_url && previous.username == config.username {
            return;
        }
        let delete_old = old_url != new_url && !self.credential_in_use(&old_url, cx);
        let username = config.username.clone().unwrap_or_default();
        let task = cx.spawn(async move |_, cx| {
            let moved = async {
                let Some((_, password)) = cx.update(|cx| cx.read_credentials(&old_url)).await?
                else {
                    return anyhow::Ok(());
                };
                cx.update(|cx| cx.write_credentials(&new_url, &username, &password))
                    .await?;
                if delete_old {
                    cx.update(|cx| cx.delete_credentials(&old_url)).await?;
                }
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = moved {
                log::error!("Quarry could not move the saved password: {error:#}");
            }
        });
        self._tasks.push(task);
    }

    /// Removes a connection's password from the OS credential store unless another saved
    /// connection still files its password under the same entry.
    fn forget_password_if_unused(&mut self, config: &ConnectionConfig, cx: &mut Context<Self>) {
        let url = config.credential_url();
        if self.credential_in_use(&url, cx) {
            return;
        }
        let task = cx.spawn(async move |_, cx| {
            if let Err(error) = cx.update(|cx| cx.delete_credentials(&url)).await {
                log::error!("Quarry could not delete the saved password: {error:#}");
            }
        });
        self._tasks.push(task);
    }

    /// Drops the open session and reconnects, keeping the expanded folders open so new or
    /// dropped tables show up where the user was looking. Also the way to retry a connection
    /// that failed, since a failed one otherwise keeps showing its error.
    fn refresh(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(expanded) = self.states.get(index).map(|state| state.expanded.clone()) else {
            return;
        };
        let mut fresh = self.new_state();
        fresh.expanded = expanded;
        if let Some(state) = self.states.get_mut(index) {
            *state = fresh;
        }
        self.connect(index, cx);
    }

    fn confirm_delete(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.store.read(cx).get(index).cloned() else {
            return;
        };
        let Some(state_id) = self.states.get(index).map(|state| state.id) else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the connection {}?", config.name),
            Some("Its saved password is removed as well."),
            &["Delete", "Cancel"],
            cx,
        );
        let task = cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| this.delete(state_id, cx)).ok();
        });
        self._tasks.push(task);
    }

    fn delete(&mut self, state_id: usize, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(state_id) else {
            return;
        };
        let Some(config) = self.store.read(cx).get(index).cloned() else {
            return;
        };
        self.states.remove(index);
        // The prompt refers to a connection by position, which has just shifted.
        self.prompt = None;
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.editing == Some(state_id))
        {
            self.draft = None;
        }
        self.store.update(cx, |store, cx| store.remove(index, cx));
        self.forget_password_if_unused(&config, cx);
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
        self.start_prompt(index, PromptKind::Password, window, cx);
    }

    fn start_prompt(
        &mut self,
        index: usize,
        kind: PromptKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = match &kind {
            PromptKind::Password => new_password_editor(window, cx),
            PromptKind::Ttl(_) | PromptKind::Rename(_) | PromptKind::FindKeys(_) => {
                let (placeholder, text) = match &kind {
                    PromptKind::Ttl(_) => ("seconds; 0 removes the TTL", None),
                    PromptKind::Rename(node) => ("new key name", Some(redis_key(node))),
                    _ => ("glob, e.g. *:email or session:*", None),
                };
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(placeholder, window, cx);
                    if let Some(text) = text {
                        editor.set_text(text, window, cx);
                        editor.select_all(&editor::actions::SelectAll, window, cx);
                    }
                    editor
                })
            }
        };
        editor.focus_handle(cx).focus(window, cx);
        self.prompt = Some(InlinePrompt {
            index,
            kind,
            editor,
        });
        cx.notify();
    }

    fn confirm_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(InlinePrompt {
            index,
            kind,
            editor,
        }) = self.prompt.take()
        else {
            return;
        };
        let text = editor.read(cx).text(cx);
        self.focus_handle.focus(window, cx);
        cx.notify();
        match kind {
            PromptKind::Password => self.change_password(index, text, cx),
            PromptKind::Ttl(node) => match text.trim() {
                "" | "0" => self.run_key_action(index, node, KeyAction::Expire(None), cx),
                seconds => match seconds.parse::<u64>() {
                    Ok(seconds) => {
                        self.run_key_action(index, node, KeyAction::Expire(Some(seconds)), cx)
                    }
                    Err(_) => {
                        self.notice = Some((
                            format!("{seconds:?} is not a number of seconds").into(),
                            Color::Error,
                        ));
                    }
                },
            },
            PromptKind::Rename(node) => {
                let new_name = text.trim().to_string();
                if !new_name.is_empty() && new_name != redis_key(&node) {
                    self.run_key_action(index, node, KeyAction::Rename(new_name), cx);
                }
            }
            PromptKind::FindKeys(node) => self.find_keys(index, node, text, window, cx),
        }
    }

    /// Changes a key from the tree, then reloads the folder it was in so the tree shows what
    /// is there now. A failure goes to the notice under the header.
    fn run_key_action(
        &mut self,
        index: usize,
        node: ObjectNode,
        action: KeyAction,
        cx: &mut Context<Self>,
    ) {
        let Some((state_id, driver)) = self
            .states
            .get(index)
            .and_then(|state| state.driver.clone().map(|driver| (state.id, driver)))
        else {
            return;
        };
        let Some(runtime) = QuarryRuntime::global(cx) else {
            return;
        };
        let applied = {
            let path = node.path.clone();
            runtime.spawn(async move { driver.key_action(&path, &action).await })
        };
        let parent = node
            .path
            .get(..node.path.len().saturating_sub(1))
            .unwrap_or_default()
            .to_vec();
        let task = cx.spawn(async move |this, cx| {
            let applied = applied.await;
            this.update(cx, |this, cx| {
                let Some(index) = this.index_of(state_id) else {
                    return;
                };
                if let Err(error) = applied {
                    this.notice = Some((format!("{error:#}").into(), Color::Error));
                }
                this.reload_node(index, parent, cx);
            })
            .ok();
        });
        self._tasks.push(task);
    }

    fn confirm_delete_key(
        &mut self,
        index: usize,
        node: ObjectNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state_id) = self.states.get(index).map(|state| state.id) else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the key {}?", redis_key(&node)),
            Some("Its value is removed from the server."),
            &["Delete", "Cancel"],
            cx,
        );
        let task = cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                if let Some(index) = this.index_of(state_id) {
                    this.run_key_action(index, node, KeyAction::Delete, cx);
                }
            })
            .ok();
        });
        self._tasks.push(task);
    }

    /// Opens a tab listing the keys under `node` that match `pattern`, found by SCAN on the
    /// server rather than in what the tree has loaded.
    fn find_keys(
        &mut self,
        index: usize,
        node: ObjectNode,
        pattern: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let pattern = match pattern.trim() {
            "" => "*".to_string(),
            pattern => pattern.to_string(),
        };
        let title = SharedString::from(format!("{} · {pattern}", node.path.join(":")));
        let Some(view) = self.open_tab(index, node.path.first().cloned(), title, None, window, cx)
        else {
            return;
        };
        view.update(cx, |view, cx| view.set_loading(cx));
        let found = runtime.spawn(async move { driver.search(&node.path, &pattern).await });
        let view = view.downgrade();
        let task = cx.spawn(async move |_, cx| {
            let found = found.await;
            view.update(cx, |view, cx| match found {
                Ok(result) => view.show_result(result, cx),
                Err(error) => view.show_error(format!("{error:#}").into(), cx),
            })
            .ok();
        });
        self._tasks.push(task);
    }

    fn change_password(&mut self, index: usize, password: String, cx: &mut Context<Self>) {
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
        let state_id = state.id;
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let opened = crate::connect(config, cx).await;
            this.update(cx, |this, cx| {
                let Some(index) = this.index_of(state_id) else {
                    return;
                };
                let Some(state) = this.states.get_mut(index) else {
                    return;
                };
                state.busy = false;
                match opened {
                    Ok(driver) => {
                        state.driver = Some(driver);
                        // Non-empty only after a refresh, which keeps what was open.
                        let expanded = state.expanded.clone();
                        this.load_children(index, Vec::new(), cx);
                        for path in expanded {
                            this.load_children(index, path, cx);
                        }
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
        let Some((state_id, driver)) = self
            .states
            .get(index)
            .and_then(|state| state.driver.clone().map(|driver| (state.id, driver)))
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
                let Some(state) = this.states.iter_mut().find(|state| state.id == state_id) else {
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
        if let Some(position) = state.expanded.iter().position(|path| *path == key) {
            state.expanded.remove(position);
        } else {
            state.expanded.push(key.clone());
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
                .any(|expanded| expanded.as_slice() == path)
        })
    }

    /// Drops the open session without reconnecting, which is what a user who is done with a
    /// server wants: the tree folds away and nothing stays connected in the background.
    fn disconnect(&mut self, index: usize, cx: &mut Context<Self>) {
        let fresh = self.new_state();
        if let Some(state) = self.states.get_mut(index) {
            *state = fresh;
        }
        cx.notify();
    }

    /// Opens a query tab on connection `index`. Without a live driver the tab connects on its
    /// own, so "New Query" works on a connection that was never expanded.
    fn open_tab(
        &mut self,
        index: usize,
        database: Option<SharedString>,
        title: SharedString,
        initial_text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<QueryView>> {
        let config = self.store.read(cx).get(index).cloned()?;
        let driver = self
            .states
            .get(index)
            .and_then(|state| state.driver.clone());
        let workspace = self.workspace.upgrade()?;
        let languages = workspace.read(cx).project().read(cx).languages().clone();
        let view = cx.new(|cx| {
            QueryView::new(
                config,
                driver,
                database,
                title,
                initial_text,
                Some(languages),
                window,
                cx,
            )
        });
        workspace.update(cx, |workspace, cx| {
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        });
        Some(view)
    }

    fn new_query(
        &mut self,
        index: usize,
        database: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self
            .store
            .read(cx)
            .get(index)
            .map(|config| config.name.clone())
        else {
            return;
        };
        let title = match &database {
            Some(database) => format!("{name} · {database}"),
            None => name,
        };
        if let Some(view) = self.open_tab(index, database, title.into(), None, window, cx) {
            view.focus_handle(cx).focus(window, cx);
        }
    }

    /// Opens a tab showing the first page of a table, collection or key, which is what
    /// double-clicking it in the tree is for. The tab opens at once and shows either the rows
    /// or why they could not be read; the query editor above them targets the same database.
    fn open_preview(
        &mut self,
        index: usize,
        node: &ObjectNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let Some(connection_name) = self
            .store
            .read(cx)
            .get(index)
            .map(|config| config.name.clone())
        else {
            return;
        };
        let title = SharedString::from(format!("{connection_name} · {}", node.name));
        let database = node.path.first().cloned();
        let Some(view) = self.open_tab(index, database, title, None, window, cx) else {
            return;
        };
        view.update(cx, |view, cx| view.set_loading(cx));

        let preview = runtime.spawn({
            let path = node.path.clone();
            async move { driver.preview(&path, PREVIEW_ROW_LIMIT).await }
        });
        let path = node.path.clone();
        let view = view.downgrade();
        let task = cx.spawn(async move |_, cx| {
            let result = preview.await;
            view.update(cx, |view, cx| match result {
                Ok(result) => view.show_preview(path, result, cx),
                Err(error) => view.show_error(format!("{error:#}").into(), cx),
            })
            .ok();
        });
        self._tasks.push(task);
    }

    /// Opens a view's or routine's source, or a table's CREATE script, in a query tab, ready to
    /// be edited and run.
    fn open_script(
        &mut self,
        index: usize,
        node: &ObjectNode,
        script: Script,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((state_id, driver)) = self
            .states
            .get(index)
            .and_then(|state| state.driver.clone().map(|driver| (state.id, driver)))
        else {
            return;
        };
        let Some(runtime) = QuarryRuntime::global(cx) else {
            return;
        };
        let path = node.path.clone();
        let name = node.name.clone();
        let definition = runtime.spawn(async move {
            match script {
                Script::Definition => driver.definition(&path).await,
                Script::Create => driver.create_script(&path).await,
            }
        });
        let database = node.path.first().cloned();
        let task = cx.spawn_in(window, async move |this, cx| {
            let definition = definition.await;
            this.update_in(cx, |this, window, cx| {
                let Some(index) = this.index_of(state_id) else {
                    return;
                };
                match definition {
                    Ok(text) => {
                        let title = SharedString::from(match script {
                            Script::Definition => format!("{name} (definition)"),
                            Script::Create => format!("{name} (create)"),
                        });
                        this.open_tab(index, database, title, Some(text), window, cx);
                    }
                    Err(error) => {
                        if let Some(state) = this.states.get_mut(index) {
                            state.error = Some(format!("{error:#}").into());
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        self._tasks.push(task);
    }

    /// Whether `node` or anything loaded beneath it matches the filter. Ancestors of a match stay
    /// visible so the match keeps its place in the tree.
    fn matches_filter(&self, index: usize, node: &ObjectNode, filter: &str) -> bool {
        if node.name.to_lowercase().contains(filter) {
            return true;
        }
        self.states
            .get(index)
            .and_then(|state| state.children.get(&node.path))
            .is_some_and(|children| {
                children
                    .iter()
                    .any(|child| self.matches_filter(index, child, filter))
            })
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
            ObjectKind::Database | ObjectKind::Schema | ObjectKind::Folder => IconName::Folder,
            ObjectKind::Table | ObjectKind::Collection => IconName::Table,
            ObjectKind::View => IconName::Eye,
            ObjectKind::Column | ObjectKind::Key => IconName::Hash,
            ObjectKind::Index => IconName::ListTree,
            ObjectKind::Routine => IconName::Code,
            ObjectKind::Sequence => IconName::Hash,
        };
        let previewable = node.kind.is_previewable();
        let has_definition = node.kind.has_definition();
        let queryable_scope = matches!(node.kind, ObjectKind::Database | ObjectKind::Schema);
        let node = node.clone();
        let row_id = SharedString::from(format!("quarry-{index}-{}", node.path.join("/")));
        let group = SharedString::from(format!("quarry-node-{index}-{}", node.path.join("/")));

        h_flex()
            .id(row_id.clone())
            .group(group.clone())
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
            // The names shrink and truncate so a long one cannot push the row's buttons out of
            // the panel.
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        Label::new(node.name.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .children(node.detail.clone().map(|detail| {
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate()
                    })),
            )
            .when(queryable_scope, |this| {
                let database = node.path.first().cloned();
                this.child(
                    div().visible_on_hover(group.clone()).child(
                        IconButton::new(
                            SharedString::from(format!("{row_id}-query")),
                            IconName::Plus,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("New Query Here"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.new_query(index, database.clone(), window, cx);
                            },
                        )),
                    ),
                )
            })
            .when(has_definition, |this| {
                let node = node.clone();
                this.child(
                    div().visible_on_hover(group.clone()).child(
                        IconButton::new(
                            SharedString::from(format!("{row_id}-definition")),
                            IconName::Code,
                        )
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text("Open Definition"))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.open_script(index, &node, Script::Definition, window, cx);
                            },
                        )),
                    ),
                )
            })
            .on_mouse_down(MouseButton::Right, {
                let node = node.clone();
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_node_menu(index, node.clone(), event.position, window, cx);
                })
            })
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    if event.click_count() >= 2 && previewable {
                        this.open_preview(index, &node, window, cx);
                    } else if event.click_count() >= 2 && has_definition {
                        this.open_script(index, &node, Script::Definition, window, cx);
                    } else if event.click_count() == 1 && node.has_children {
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
        filter: &str,
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
            if !filter.is_empty() && !self.matches_filter(index, &node, filter) {
                continue;
            }
            elements.push(self.render_node(index, &node, depth, cx));
            if self.is_expanded(index, &node.path) {
                elements.extend(self.render_subtree(index, &node.path, depth + 1, filter, cx));
            }
        }
        elements
    }

    fn render_connection_menu(
        &self,
        index: usize,
        connected: bool,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        PopoverMenu::new(SharedString::from(format!(
            "quarry-connection-menu-{index}"
        )))
        .trigger_with_tooltip(
            IconButton::new(
                ("quarry-connection-menu-trigger", index),
                IconName::Ellipsis,
            )
            .icon_size(IconSize::XSmall),
            Tooltip::text("Connection Actions"),
        )
        .anchor(Anchor::TopRight)
        .menu(move |window, cx| {
            let panel = panel.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                connection_menu(menu, panel, index, connected, busy)
            }))
        })
    }

    fn deploy_context_menu(
        &mut self,
        position: Point<Pixels>,
        menu: Entity<ContextMenu>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn deploy_connection_menu(
        &mut self,
        index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.states.get(index);
        let connected = state.is_some_and(|state| state.driver.is_some());
        let busy = state.is_some_and(|state| state.busy);
        let panel = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            connection_menu(menu, panel, index, connected, busy)
        });
        self.deploy_context_menu(position, menu, window, cx);
    }

    /// What can be done with one object in the tree. Every entry is something the driver says
    /// it can do for that object, so a Redis key prefix offers less than a Postgres table.
    fn deploy_node_menu(
        &mut self,
        index: usize,
        node: ObjectNode,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(driver) = self
            .states
            .get(index)
            .and_then(|state| state.driver.clone())
        else {
            return;
        };
        let is_data = matches!(
            node.kind,
            ObjectKind::Table | ObjectKind::View | ObjectKind::Collection
        );
        let select = is_data
            .then(|| driver.select_statement(&node.path, SELECT_ROW_LIMIT))
            .flatten();
        let count = is_data
            .then(|| driver.count_statement(&node.path))
            .flatten();
        let qualified_name = driver.qualified_name(&node.path);
        let is_redis = self
            .store
            .read(cx)
            .get(index)
            .is_some_and(|config| config.engine == Engine::Redis);
        let is_key = is_redis && node.kind == ObjectKind::Key;
        let can_find_keys =
            is_redis && matches!(node.kind, ObjectKind::Database | ObjectKind::Folder);
        let panel = cx.entity().downgrade();

        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let update =
                |action: Box<dyn Fn(&mut QuarryPanel, &mut Window, &mut Context<QuarryPanel>)>| {
                    let panel = panel.clone();
                    move |window: &mut Window, cx: &mut App| {
                        panel
                            .update(cx, |this, cx| action(this, window, cx))
                            .log_err();
                    }
                };
            let database = node.path.first().cloned();
            let name = node.name.clone();
            menu.when(node.kind.is_previewable(), |menu| {
                let node = node.clone();
                menu.entry(
                    "Open Data",
                    None,
                    update(Box::new(move |this, window, cx| {
                        this.open_preview(index, &node, window, cx)
                    })),
                )
            })
            .when_some(select, |menu, statement| {
                let database = database.clone();
                let title = SharedString::from(format!("{name} (top {SELECT_ROW_LIMIT})"));
                menu.entry(
                    format!("Select Top {SELECT_ROW_LIMIT}"),
                    None,
                    update(Box::new(move |this, window, cx| {
                        this.run_in_tab(
                            index,
                            database.clone(),
                            title.clone(),
                            statement.clone(),
                            window,
                            cx,
                        )
                    })),
                )
            })
            .when_some(count, |menu, statement| {
                let database = database.clone();
                let title = SharedString::from(format!("{name} (count)"));
                menu.entry(
                    "Count Rows",
                    None,
                    update(Box::new(move |this, window, cx| {
                        this.run_in_tab(
                            index,
                            database.clone(),
                            title.clone(),
                            statement.clone(),
                            window,
                            cx,
                        )
                    })),
                )
            })
            .when(
                matches!(node.kind, ObjectKind::Database | ObjectKind::Schema),
                |menu| {
                    let database = database.clone();
                    menu.entry(
                        "New Query Here",
                        None,
                        update(Box::new(move |this, window, cx| {
                            this.new_query(index, database.clone(), window, cx)
                        })),
                    )
                },
            )
            .separator()
            .when(node.kind.has_definition(), |menu| {
                let node = node.clone();
                menu.entry(
                    "Open Definition",
                    None,
                    update(Box::new(move |this, window, cx| {
                        this.open_script(index, &node, Script::Definition, window, cx)
                    })),
                )
            })
            .when(is_data, |menu| {
                let node = node.clone();
                menu.entry(
                    "Open CREATE Script",
                    None,
                    update(Box::new(move |this, window, cx| {
                        this.open_script(index, &node, Script::Create, window, cx)
                    })),
                )
            })
            .entry("Copy Name", None, move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(name.to_string()))
            })
            .when_some(qualified_name, |menu, qualified_name| {
                menu.entry("Copy Qualified Name", None, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(qualified_name.clone()))
                })
            })
            .when(can_find_keys, |menu| {
                let node = node.clone();
                menu.separator().entry(
                    "Find Keys…",
                    None,
                    update(Box::new(move |this, window, cx| {
                        this.start_prompt(index, PromptKind::FindKeys(node.clone()), window, cx)
                    })),
                )
            })
            .when(is_key, |menu| {
                let ttl = node.clone();
                let persist = node.clone();
                let rename = node.clone();
                let delete = node.clone();
                menu.separator()
                    .entry(
                        "Set TTL…",
                        None,
                        update(Box::new(move |this, window, cx| {
                            this.start_prompt(index, PromptKind::Ttl(ttl.clone()), window, cx)
                        })),
                    )
                    .entry(
                        "Remove TTL",
                        None,
                        update(Box::new(move |this, _, cx| {
                            this.run_key_action(index, persist.clone(), KeyAction::Expire(None), cx)
                        })),
                    )
                    .entry(
                        "Rename Key…",
                        None,
                        update(Box::new(move |this, window, cx| {
                            this.start_prompt(index, PromptKind::Rename(rename.clone()), window, cx)
                        })),
                    )
                    .entry(
                        "Delete Key…",
                        None,
                        update(Box::new(move |this, window, cx| {
                            this.confirm_delete_key(index, delete.clone(), window, cx)
                        })),
                    )
            })
            .when(node.has_children, |menu| {
                let path = node.path.clone();
                menu.separator().entry(
                    "Refresh",
                    None,
                    update(Box::new(move |this, _, cx| {
                        this.reload_node(index, path.clone(), cx)
                    })),
                )
            })
        });
        self.deploy_context_menu(position, menu, window, cx);
    }

    /// Forgets what was loaded under `path` and loads it again, so a table created since the
    /// folder was opened shows up without reconnecting the whole server.
    fn reload_node(&mut self, index: usize, path: Vec<SharedString>, cx: &mut Context<Self>) {
        let Some(state) = self.states.get_mut(index) else {
            return;
        };
        state
            .children
            .retain(|loaded, _| !loaded.starts_with(&path));
        if !state.expanded.contains(&path) {
            state.expanded.push(path.clone());
        }
        // Folders open below it are loaded again too, or they would show as open and empty.
        let open_below: Vec<Vec<SharedString>> = state
            .expanded
            .iter()
            .filter(|expanded| expanded.starts_with(&path) && **expanded != path)
            .cloned()
            .collect();
        self.load_children(index, path, cx);
        for expanded in open_below {
            self.load_children(index, expanded, cx);
        }
        cx.notify();
    }

    /// Opens `statement` in a new query tab and runs it straight away.
    fn run_in_tab(
        &mut self,
        index: usize,
        database: Option<SharedString>,
        title: SharedString,
        statement: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.open_tab(index, database, title, Some(statement), window, cx) {
            view.update(cx, |view, cx| view.run_everything(cx));
        }
    }

    /// Connections in folders first, each folder under a heading that folds it away, then the
    /// ones without a folder. Positions in the store are kept, so every row still addresses
    /// its connection by index.
    fn render_connection_list(
        &self,
        connections: &[ConnectionConfig],
        filter: &str,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut folders: BTreeMap<String, Vec<usize>> = BTreeMap::default();
        let mut loose = Vec::new();
        for (index, config) in connections.iter().enumerate() {
            match &config.folder {
                Some(folder) => folders.entry(folder.clone()).or_default().push(index),
                None => loose.push(index),
            }
        }

        let mut elements = Vec::new();
        for (folder, indexes) in folders {
            let collapsed = self.collapsed_folders.contains(&folder);
            elements.push(
                h_flex()
                    .id(SharedString::from(format!("quarry-folder-{folder}")))
                    .w_full()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .hover(|style| style.bg(cx.theme().colors().element_hover))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(SharedString::from(folder.clone()))
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        Label::new(SharedString::from(indexes.len().to_string()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_folders.remove(&folder) {
                            this.collapsed_folders.insert(folder.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            if !collapsed {
                for index in indexes {
                    if let Some(config) = connections.get(index) {
                        elements.push(
                            div()
                                .w_full()
                                .pl_3()
                                .child(self.render_connection(index, config, filter, cx))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        for index in loose {
            if let Some(config) = connections.get(index) {
                elements.push(self.render_connection(index, config, filter, cx));
            }
        }
        elements
    }

    fn render_connection(
        &self,
        index: usize,
        config: &ConnectionConfig,
        filter: &str,
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
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .overflow_hidden()
                            .child(
                                Label::new(SharedString::from(config.name.clone()))
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .when_some(config.environment, |row, environment| {
                                row.child(environment_badge(environment))
                            })
                            .when_some(config.ssh.as_ref(), |row, ssh| {
                                row.child(
                                    div()
                                        .id(("quarry-ssh", index))
                                        .child(
                                            Label::new("ssh")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .tooltip(Tooltip::text(format!("Through {ssh}"))),
                                )
                            })
                            .child(
                                Label::new(SharedString::from(config.engine.label()))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                            .when(busy, |this| {
                                this.child(
                                    Label::new("connecting…")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Info),
                                )
                            }),
                    )
                    .child(
                        IconButton::new(("quarry-new-query", index), IconName::Plus)
                            .icon_size(IconSize::XSmall)
                            .tooltip(|_, cx| Tooltip::simple("New Query", cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.new_query(index, None, window, cx);
                            })),
                    )
                    .child(self.render_connection_menu(index, connected, busy, cx))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.deploy_connection_menu(index, event.position, window, cx);
                        }),
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
                this.children(self.render_subtree(index, &[], 1, filter, cx))
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
        let filter = self.filter.read(cx).text(cx).trim().to_lowercase();

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
                        h_flex()
                            .gap_0p5()
                            .child(
                                IconButton::new("quarry-docker", IconName::Box)
                                    .icon_size(IconSize::Small)
                                    .disabled(self.discovering.is_some())
                                    .tooltip(|_, cx| {
                                        Tooltip::simple("Add from Docker Containers", cx)
                                    })
                                    .on_click(cx.listener(
                                        |this, event: &gpui::ClickEvent, window, cx| {
                                            this.discover_docker(event.position(), window, cx);
                                        },
                                    )),
                            )
                            .child(
                                IconButton::new("quarry-add", IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .tooltip(|_, cx| Tooltip::simple("Add Connection", cx))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_adding(window, cx);
                                    })),
                            ),
                    ),
            )
            .children(self.notice.clone().map(|(notice, color)| {
                h_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(notice).size(LabelSize::XSmall).color(color)),
                    )
                    .child(
                        IconButton::new("quarry-dismiss-notice", IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.notice = None;
                                cx.notify();
                            })),
                    )
            }))
            .children(self.draft.as_ref().map(|draft| {
                let editing_name = draft
                    .editing
                    .and_then(|state_id| self.index_of(state_id))
                    .and_then(|index| self.store.read(cx).get(index))
                    .map(|config| config.name.clone());
                v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .children(editing_name.map(|name| {
                        Label::new(SharedString::from(format!("Editing {name}")))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    }))
                    .child(draft.name.clone())
                    .child(draft.url.clone())
                    .child(draft.password.clone())
                    .child(draft.folder.clone())
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new("Environment")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .children(
                                std::iter::once(None)
                                    .chain(Environment::ALL.into_iter().map(Some))
                                    .map(|environment| {
                                        Button::new(
                                            SharedString::from(format!(
                                                "quarry-environment-{}",
                                                environment.map_or("none", Environment::label)
                                            )),
                                            environment.map_or("none", Environment::label),
                                        )
                                        .label_size(LabelSize::XSmall)
                                        .when_some(environment, |button, environment| {
                                            button.color(environment.color())
                                        })
                                        .toggle_state(draft.environment == environment)
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                if let Some(draft) = this.draft.as_mut() {
                                                    draft.environment = environment;
                                                }
                                                cx.notify();
                                            }),
                                        )
                                    }),
                            ),
                    )
                    .children(draft.error.clone().map(|error| {
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error)
                    }))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("quarry-test-connection", "Test")
                                    .label_size(LabelSize::Small)
                                    .disabled(matches!(draft.test, TestState::Testing))
                                    .on_click(cx.listener(|this, _, _, cx| this.test_draft(cx))),
                            )
                            .child(match &draft.test {
                                TestState::Untested => {
                                    Label::new(
                                        "Enter to save · a password in the URL is ignored ·                                          options: ?ssh=user@bastion&ssh_key=path&tls=require|verify|disable",
                                    )
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                }
                                TestState::Testing => Label::new("testing…")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Info),
                                TestState::Succeeded => Label::new("connected successfully")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Success),
                                TestState::Failed(error) => Label::new(error.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error),
                            }),
                    )
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.confirm_draft(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                        this.draft = None;
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }))
            .children(self.prompt.as_ref().map(|prompt| {
                let label = match &prompt.kind {
                    PromptKind::Password => {
                        let name = self
                            .store
                            .read(cx)
                            .get(prompt.index)
                            .map(|config| config.name.clone())
                            .unwrap_or_default();
                        format!("Password for {name}")
                    }
                    PromptKind::Ttl(node) => format!("TTL of {}", redis_key(node)),
                    PromptKind::Rename(node) => format!("Rename {}", redis_key(node)),
                    PromptKind::FindKeys(node) => {
                        format!("Find keys under {}", node.path.join(":"))
                    }
                };
                v_flex()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .child(
                        Label::new(SharedString::from(label))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(prompt.editor.clone())
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        this.confirm_prompt(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                        this.prompt = None;
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }))
            .when(!connections.is_empty(), |this| {
                this.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(self.filter.clone()),
                )
            })
            .child(
                v_flex()
                    .id("quarry-connections")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .children(self.render_connection_list(&connections, &filter, cx))
                    .when(connections.is_empty() && self.draft.is_none(), |this| {
                        this.child(
                            Label::new("No connections yet — use + to add one")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
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
    cx.update(|window, cx| {
        let store = ConnectionStore::global(cx)
            .ok_or_else(|| anyhow::anyhow!("the Quarry connection store is unavailable"))?;
        anyhow::Ok(cx.new(|cx| QuarryPanel::new(workspace, store, window, cx)))
    })?
}
