use crate::{
    completion::QuarryCompletionProvider,
    connection_store::{ConnectionStore, query_history, record_query},
    driver::{CatalogEntry, CellEdit, ConnectionConfig, Driver, Engine, Environment, ResultSet},
    result_table,
    runtime::QuarryRuntime,
    statements,
};
use anyhow::{Context as _, Result, anyhow};
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, MultiBufferOffset};
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, PromptLevel, Render, Subscription, Task, WeakEntity, Window, actions,
};
use language::LanguageRegistry;
use project::Project;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use tabular_data_preview::{TableView, TableViewEvent};
use ui::{
    Color, ContextMenu, Icon, IconButton, IconName, Label, LabelCommon as _, PopoverMenu,
    SharedString, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    ItemId, Workspace, WorkspaceId,
    item::{Item, ItemEvent, SerializableItem, TabContentParams},
};

actions!(
    quarry,
    [
        /// Runs the selected text, or the statement under the cursor when nothing is selected.
        RunQuery,
        /// Runs every statement in the editor, in order.
        RunAll,
        /// Stops the running query.
        CancelQuery,
    ]
);

/// Results are capped rather than streamed. A browser's job is to let someone look at data, and
/// a `SELECT *` against a large table would otherwise spend minutes filling memory with rows
/// nobody scrolls to. The view says so when it truncates, so the cap is never silent.
const ROW_LIMIT: usize = 2_000;

enum Status {
    Connecting,
    Idle,
    Running {
        started: Instant,
        statements: usize,
    },
    Cancelled,
    Failed(SharedString),
    Succeeded {
        statements: usize,
        elapsed: Duration,
    },
    Updated(SharedString),
}

/// A cell being edited in the bar under the grid. Indexes are into `QueryView::results`.
struct PendingEdit {
    result_index: usize,
    row: usize,
    column: usize,
    editor: Entity<Editor>,
    saving: bool,
}

pub struct QueryView {
    focus_handle: FocusHandle,
    config: ConnectionConfig,
    title: SharedString,
    editor: Entity<Editor>,
    table: Entity<TableView>,
    /// `None` while a restored tab is still reconnecting.
    driver: Option<Arc<dyn Driver>>,
    databases: Vec<SharedString>,
    /// The database queries run in; `None` means the connection's default.
    database: Option<SharedString>,
    catalog: Rc<RefCell<Vec<CatalogEntry>>>,
    results: Vec<ResultSet>,
    active_result: usize,
    status: Status,
    pending_edit: Option<PendingEdit>,
    running: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl QueryView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: ConnectionConfig,
        driver: Option<Arc<dyn Driver>>,
        database: Option<SharedString>,
        title: SharedString,
        initial_text: Option<String>,
        languages: Option<Arc<LanguageRegistry>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let engine = config.engine;
        let catalog = Rc::new(RefCell::new(Vec::new()));
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(placeholder(engine), window, cx);
            editor.set_completion_provider(Some(Rc::new(QuarryCompletionProvider::new(
                engine,
                catalog.clone(),
            ))));
            editor.set_show_completions_on_input(Some(true));
            if let Some(text) = initial_text {
                editor.set_text(text, window, cx);
            }
            editor
        });

        let table = cx.new(|cx| {
            let mut table = TableView::new(window, cx);
            table.set_selection_enabled(true);
            table
        });

        let subscriptions = vec![
            cx.subscribe_in(&table, window, |this, _, event, window, cx| {
                if let TableViewEvent::CellDoubleClicked(cell) = event {
                    this.start_edit(cell.row.0, cell.col.0, window, cx);
                }
            }),
            // Saving the tab's text is what makes it survive a restart; doing it on every edit
            // is cheap because the workspace debounces serialization.
            cx.subscribe(&editor, |_, _, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.emit(ItemEvent::Edit);
                }
            }),
        ];

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            config,
            title,
            editor,
            table,
            driver: None,
            databases: Vec::new(),
            database,
            catalog,
            results: Vec::new(),
            active_result: 0,
            status: Status::Idle,
            pending_edit: None,
            running: None,
            _tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        if let Some(languages) = languages {
            this.set_language(languages, cx);
        }
        match driver {
            Some(driver) => this.attach(driver, cx),
            None => this.connect(cx),
        }
        this
    }

    /// Highlighting comes from whatever grammar is installed for the engine's language; with
    /// none (Redis, or no SQL extension) the editor simply stays plain text.
    fn set_language(&mut self, languages: Arc<LanguageRegistry>, cx: &mut Context<Self>) {
        let language_name = match self.config.engine {
            Engine::Postgres | Engine::SqlServer | Engine::ClickHouse => "SQL",
            Engine::MongoDb => "JSON",
            Engine::Redis => return,
        };
        let language = languages.language_for_name(language_name);
        let editor = self.editor.downgrade();
        self._tasks.push(cx.spawn(async move |_, cx| {
            let Ok(language) = language.await else {
                return;
            };
            editor
                .update(cx, |editor, cx| {
                    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                        buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
                    }
                })
                .ok();
        }));
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        self.status = Status::Connecting;
        let config = self.config.clone();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let connected = crate::connect(config, cx).await;
            this.update(cx, |this, cx| match connected {
                Ok(driver) => {
                    this.status = Status::Idle;
                    this.attach(driver, cx);
                }
                Err(error) => {
                    this.status = Status::Failed(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn attach(&mut self, driver: Arc<dyn Driver>, cx: &mut Context<Self>) {
        self.driver = Some(driver.clone());
        if self.database.is_none() {
            self.database = driver.default_database();
        }
        cx.notify();

        let Some(runtime) = QuarryRuntime::global(cx) else {
            return;
        };
        let databases = runtime.spawn(async move { driver.databases().await });
        self._tasks
            .push(cx.spawn(async move |this, cx| match databases.await {
                Ok(databases) => {
                    this.update(cx, |this, cx| {
                        this.databases = databases;
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => log::warn!("Quarry could not list databases: {error:#}"),
            }));
        self.load_catalog(cx);
    }

    /// Refreshes the names offered as completions. Called on connect, when the database picker
    /// changes, and after a statement that may have created or dropped something.
    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let (Some(driver), Some(runtime)) = (self.driver.clone(), QuarryRuntime::global(cx)) else {
            return;
        };
        let database = self.database.clone();
        let catalog = runtime.spawn(async move { driver.catalog(database.as_deref()).await });
        let shared = self.catalog.clone();
        self._tasks
            .push(cx.spawn(async move |_, _| match catalog.await {
                Ok(entries) => *shared.borrow_mut() = entries,
                Err(error) => log::warn!("Quarry could not load completions: {error:#}"),
            }));
    }

    pub fn set_database(&mut self, database: Option<SharedString>, cx: &mut Context<Self>) {
        if self.database == database {
            return;
        }
        self.database = database;
        self.load_catalog(cx);
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// Replaces the grid with `result`, for the table preview that opens from the tree.
    pub fn show_result(&mut self, result: ResultSet, cx: &mut Context<Self>) {
        self.status = Status::Succeeded {
            statements: 1,
            elapsed: Duration::ZERO,
        };
        self.set_results(vec![result], cx);
    }

    /// Shows why a preview could not be loaded where the user is looking, instead of an empty
    /// grid that reads like an empty table.
    pub fn show_error(&mut self, error: SharedString, cx: &mut Context<Self>) {
        self.status = Status::Failed(error);
        self.table
            .update(cx, |table, cx| table.set_loading(false, cx));
        cx.notify();
    }

    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        self.status = Status::Running {
            started: Instant::now(),
            statements: 1,
        };
        self.table
            .update(cx, |table, cx| table.set_loading(true, cx));
        cx.notify();
    }

    /// The text a run should send: what is selected, or else the one statement under the
    /// cursor. Nothing outside the selection is ever run.
    fn text_to_run(&self, cx: &mut App) -> Vec<String> {
        let engine = self.config.engine;
        self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            let snapshot = editor.display_snapshot(cx);
            let selections = editor.selections.all::<MultiBufferOffset>(&snapshot);
            let selected: Vec<String> = selections
                .iter()
                .filter(|selection| selection.start != selection.end)
                .filter_map(|selection| text.get(selection.start.0..selection.end.0))
                .map(str::to_string)
                .collect();
            if !selected.is_empty() {
                return selected;
            }
            let cursor = selections.last().map_or(0, |selection| selection.head().0);
            statements::statement_at(&text, cursor, engine)
                .and_then(|range| text.get(range))
                .map(|statement| vec![statement.to_string()])
                .unwrap_or_default()
        })
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        let chunks = self.text_to_run(cx);
        self.confirm_and_execute(chunks, window, cx);
    }

    fn run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        self.confirm_and_execute(vec![text], window, cx);
    }

    /// The connection's environment as it is saved now, not as it was when this tab opened.
    fn environment(&self, cx: &App) -> Option<Environment> {
        ConnectionStore::global(cx).map_or(self.config.environment, |store| {
            store.read(cx).environment_of(&self.config)
        })
    }

    /// Runs `chunks`, first asking when the connection is marked production and any of the
    /// statements may change something there.
    fn confirm_and_execute(
        &mut self,
        chunks: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.config.engine;
        let writes = split_statements(&chunks, engine)
            .iter()
            .filter(|statement| statements::may_write(statement, engine))
            .count();
        if writes == 0 || self.environment(cx) != Some(Environment::Production) {
            self.execute(chunks, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Run on production ({})?", self.config.name),
            Some(&format!(
                "{writes} statement(s) may change data or schema on a connection marked production."
            )),
            &["Run", "Cancel"],
            cx,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |this, cx| this.execute(chunks, cx)).ok();
            }
        }));
    }

    /// Runs every statement in the editor, for a tab that was opened to run a statement the
    /// panel wrote, such as "Select Top 100".
    pub fn run_everything(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        self.execute(vec![text], cx);
    }

    fn execute(&mut self, chunks: Vec<String>, cx: &mut Context<Self>) {
        let statements = split_statements(&chunks, self.config.engine);
        if statements.is_empty() {
            return;
        }
        let Some(driver) = self.driver.clone() else {
            self.status = Status::Failed(match self.status {
                Status::Connecting => "still connecting — try again in a moment".into(),
                _ => "not connected; use Refresh on the connection to retry".into(),
            });
            cx.notify();
            return;
        };
        let Some(runtime) = QuarryRuntime::global(cx) else {
            self.status = Status::Failed("the Quarry runtime is not available".into());
            cx.notify();
            return;
        };

        for chunk in &chunks {
            record_query(&self.config, chunk.clone(), cx);
        }

        self.pending_edit = None;
        let total = statements.len();
        let started = Instant::now();
        self.status = Status::Running {
            started,
            statements: total,
        };
        self.table
            .update(cx, |table, cx| table.set_loading(true, cx));
        cx.notify();

        // The whole batch runs as one task on the Quarry runtime so that cancelling drops it in
        // one place, between or during statements alike.
        let database = self.database.clone();
        let batch = runtime.spawn(async move {
            let mut results = Vec::new();
            for (index, statement) in statements.iter().enumerate() {
                match driver
                    .query(statement, database.as_deref(), ROW_LIMIT)
                    .await
                {
                    Ok(sets) => results.extend(sets),
                    Err(error) => {
                        let failure = if total > 1 {
                            format!("statement {} of {total} failed: {error:#}", index + 1)
                        } else {
                            format!("{error:#}")
                        };
                        return Ok((results, Some(failure)));
                    }
                }
            }
            anyhow::Ok((results, None))
        });

        self.running = Some(cx.spawn(async move |this, cx| {
            let outcome = batch.await;
            this.update(cx, |this, cx| {
                this.running = None;
                this.table
                    .update(cx, |table, cx| table.set_loading(false, cx));
                match outcome {
                    Ok((results, failure)) => {
                        let changed_schema = results.iter().any(|result| result.columns.is_empty());
                        if !results.is_empty() {
                            this.set_results(results, cx);
                        }
                        this.status = match failure {
                            Some(failure) => Status::Failed(failure.into()),
                            None => Status::Succeeded {
                                statements: total,
                                elapsed: started.elapsed(),
                            },
                        };
                        // A statement that returned no rows may have been DDL; refreshing is
                        // cheap compared with offering a dropped table.
                        if changed_schema {
                            this.load_catalog(cx);
                        }
                    }
                    Err(error) => this.status = Status::Failed(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn cancel_query(&mut self, _: &CancelQuery, _window: &mut Window, cx: &mut Context<Self>) {
        // Dropping the task aborts the batch on the Quarry runtime; the driver is also asked to
        // stop the statement on the server, which would otherwise keep running unseen.
        if self.running.take().is_none() {
            return;
        }
        if let (Some(driver), Some(runtime)) = (self.driver.clone(), QuarryRuntime::global(cx)) {
            let cancelled = runtime.spawn(async move { driver.cancel().await });
            self._tasks.push(cx.spawn(async move |_, _| {
                if let Err(error) = cancelled.await {
                    log::warn!("Quarry could not cancel the query on the server: {error:#}");
                }
            }));
        }
        self.status = Status::Cancelled;
        self.table
            .update(cx, |table, cx| table.set_loading(false, cx));
        cx.notify();
    }

    fn set_results(&mut self, results: Vec<ResultSet>, cx: &mut Context<Self>) {
        self.results = results;
        self.pending_edit = None;
        self.select_result(0, cx);
    }

    fn select_result(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(index) else {
            return;
        };
        self.active_result = index;
        let contents = result_table::to_table_content(result);
        self.table
            .update(cx, |table, cx| table.set_contents(contents, cx));
        cx.notify();
    }

    fn active(&self) -> Option<&ResultSet> {
        self.results.get(self.active_result)
    }

    fn start_edit(
        &mut self,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(result) = self.active() else {
            return;
        };
        let Some(target) = &result.edit_target else {
            self.status = Status::Failed(
                "this result is read-only: it did not come straight from a table with a key".into(),
            );
            cx.notify();
            return;
        };
        if target.key_columns.contains(&column) || target.read_only_columns.contains(&column) {
            self.status =
                Status::Failed("this column identifies the row and cannot be edited here".into());
            cx.notify();
            return;
        }
        let current = result
            .rows
            .get(row)
            .and_then(|cells| cells.get(column))
            .cloned()
            .flatten();
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            if let Some(current) = current {
                editor.set_text(current.to_string(), window, cx);
            } else {
                editor.set_placeholder_text("NULL", window, cx);
            }
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        editor.focus_handle(cx).focus(window, cx);
        self.pending_edit = Some(PendingEdit {
            result_index: self.active_result,
            row,
            column,
            editor,
            saving: false,
        });
        cx.notify();
    }

    fn save_edit(&mut self, set_null: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_edit.as_mut() else {
            return;
        };
        if pending.saving {
            return;
        }
        let (result_index, row, column) = (pending.result_index, pending.row, pending.column);
        let value = (!set_null).then(|| pending.editor.read(cx).text(cx));
        let Some(edit) = self.cell_edit(result_index, row, column, value.clone()) else {
            return;
        };
        let (Some(driver), Some(runtime)) = (self.driver.clone(), QuarryRuntime::global(cx)) else {
            return;
        };
        if let Some(pending) = self.pending_edit.as_mut() {
            pending.saving = true;
        }
        let update = runtime.spawn(async move { driver.update_cell(&edit).await });
        self._tasks.push(cx.spawn_in(window, async move |this, cx| {
            let updated = update.await;
            this.update_in(cx, |this, window, cx| {
                match updated {
                    Ok(rows) => {
                        if let Some(cell) = this
                            .results
                            .get_mut(result_index)
                            .and_then(|result| result.rows.get_mut(row))
                            .and_then(|cells| cells.get_mut(column))
                        {
                            *cell = value.map(SharedString::from);
                        }
                        this.pending_edit = None;
                        this.status = Status::Updated(format!("{rows} row(s) updated").into());
                        if result_index == this.active_result {
                            this.select_result(result_index, cx);
                        }
                        this.table.focus_handle(cx).focus(window, cx);
                    }
                    Err(error) => {
                        if let Some(pending) = this.pending_edit.as_mut() {
                            pending.saving = false;
                        }
                        this.status = Status::Failed(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn cell_edit(
        &self,
        result_index: usize,
        row: usize,
        column: usize,
        value: Option<String>,
    ) -> Option<CellEdit> {
        let result = self.results.get(result_index)?;
        let target = result.edit_target.as_ref()?;
        let cells = result.rows.get(row)?;
        let key = target
            .key_columns
            .iter()
            .map(|&index| {
                Some((
                    result.columns.get(index)?.clone(),
                    cells.get(index).cloned().flatten(),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(CellEdit {
            path: target.path.clone(),
            key,
            column: result.columns.get(column)?.clone(),
            value,
        })
    }

    fn insert_history(&mut self, statement: String, window: &mut Window, cx: &mut Context<Self>) {
        // Appended rather than replacing the editor's text, which may hold unsaved work.
        self.editor.update(cx, |editor, cx| {
            let existing = editor.text(cx);
            let separator = if existing.trim().is_empty() {
                ""
            } else if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            editor.move_to_end(&editor::actions::MoveToEnd, window, cx);
            editor.insert(&format!("{separator}{statement}"), window, cx);
        });
        self.editor.focus_handle(cx).focus(window, cx);
    }

    fn export(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        let Some(result) = self.active() else {
            return;
        };
        let contents = match format {
            ExportFormat::Csv => result_table::to_csv(result),
            ExportFormat::Json => result_table::to_json(result),
        };
        let suggested = format!(
            "{}.{}",
            self.title.replace(['/', '\\', ':', ' ', '·'], "_"),
            format.extension()
        );
        let path = cx.prompt_for_new_path(util::paths::home_dir(), Some(&suggested));
        self._tasks.push(cx.spawn(async move |this, cx| {
            let written: Result<Option<std::path::PathBuf>> = async {
                let Some(path) = path.await.context("the save dialog closed")?? else {
                    return Ok(None);
                };
                cx.background_spawn({
                    let path = path.clone();
                    async move { std::fs::write(&path, contents) }
                })
                .await
                .with_context(|| format!("writing {}", path.display()))?;
                Ok(Some(path))
            }
            .await;
            this.update(cx, |this, cx| {
                match written {
                    Ok(Some(path)) => {
                        this.status = Status::Updated(format!("saved {}", path.display()).into())
                    }
                    Ok(None) => {}
                    Err(error) => this.status = Status::Failed(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn copy_csv(&mut self, cx: &mut Context<Self>) {
        if let Some(result) = self.active() {
            cx.write_to_clipboard(ClipboardItem::new_string(result_table::to_csv(result)));
            self.status = Status::Updated("copied as CSV".into());
            cx.notify();
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = self.running.is_some();
        let has_result = self
            .active()
            .is_some_and(|result| !result.columns.is_empty());
        let this = cx.entity().downgrade();

        h_flex()
            .w_full()
            .gap_1()
            .px_2()
            .py_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .when_some(self.environment(cx), |toolbar, environment| {
                toolbar.child(environment_badge(environment))
            })
            .child(
                Button::new("quarry-run", "Run")
                    .start_icon(Icon::new(IconName::PlayFilled).size(IconSize::Small))
                    .label_size(LabelSize::Small)
                    .disabled(running || self.driver.is_none())
                    .tooltip(|_, cx| {
                        Tooltip::with_meta(
                            "Run",
                            Some(&RunQuery),
                            "The selection, or the statement under the cursor",
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_query(&RunQuery, window, cx);
                    })),
            )
            .child(
                IconButton::new("quarry-run-all", IconName::ListTodo)
                    .icon_size(IconSize::Small)
                    .disabled(running || self.driver.is_none())
                    .tooltip(|_, cx| Tooltip::for_action("Run Everything", &RunAll, cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_all(&RunAll, window, cx);
                    })),
            )
            .when(running, |toolbar| {
                toolbar.child(
                    Button::new("quarry-cancel", "Cancel")
                        .start_icon(Icon::new(IconName::Stop).size(IconSize::Small))
                        .label_size(LabelSize::Small)
                        .tooltip(|_, cx| Tooltip::for_action("Cancel Query", &CancelQuery, cx))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_query(&CancelQuery, window, cx);
                        })),
                )
            })
            .when(!self.databases.is_empty(), |toolbar| {
                let databases = self.databases.clone();
                let current = self.database.clone();
                let this = this.clone();
                toolbar.child(
                    PopoverMenu::new("quarry-database")
                        .trigger(
                            Button::new(
                                "quarry-database-trigger",
                                current
                                    .clone()
                                    .unwrap_or_else(|| SharedString::from("default database")),
                            )
                            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall))
                            .label_size(LabelSize::Small),
                        )
                        .menu(move |window, cx| {
                            let databases = databases.clone();
                            let current = current.clone();
                            let this = this.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                for database in databases {
                                    let this = this.clone();
                                    let selected = current.as_ref() == Some(&database);
                                    menu = menu.toggleable_entry(
                                        database.clone(),
                                        selected,
                                        IconPosition::Start,
                                        None,
                                        move |_, cx| {
                                            this.update(cx, |this, cx| {
                                                this.set_database(Some(database.clone()), cx);
                                            })
                                            .ok();
                                        },
                                    );
                                }
                                menu
                            }))
                        }),
                )
            })
            .child(div().flex_1())
            .child(self.render_history_menu(this.clone(), cx))
            .child(
                IconButton::new("quarry-copy-csv", IconName::Copy)
                    .icon_size(IconSize::Small)
                    .disabled(!has_result)
                    .tooltip(Tooltip::text("Copy Result as CSV"))
                    .on_click(cx.listener(|this, _, _, cx| this.copy_csv(cx))),
            )
            .child(
                PopoverMenu::new("quarry-export")
                    .trigger(
                        IconButton::new("quarry-export-trigger", IconName::Download)
                            .icon_size(IconSize::Small)
                            .disabled(!has_result)
                            .tooltip(Tooltip::text("Export Result")),
                    )
                    .menu(move |window, cx| {
                        let this = this.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let for_csv = this.clone();
                            let for_json = this;
                            menu.entry("Export as CSV…", None, move |_, cx| {
                                for_csv
                                    .update(cx, |this, cx| this.export(ExportFormat::Csv, cx))
                                    .ok();
                            })
                            .entry(
                                "Export as JSON…",
                                None,
                                move |_, cx| {
                                    for_json
                                        .update(cx, |this, cx| this.export(ExportFormat::Json, cx))
                                        .ok();
                                },
                            )
                        }))
                    }),
            )
            .into_any_element()
    }

    fn render_history_menu(&self, this: WeakEntity<Self>, cx: &App) -> AnyElement {
        let config = self.config.clone();
        let has_history = !query_history(&config, cx).is_empty();
        PopoverMenu::new("quarry-history")
            .trigger(
                IconButton::new("quarry-history-trigger", IconName::HistoryRerun)
                    .icon_size(IconSize::Small)
                    .disabled(!has_history)
                    .tooltip(Tooltip::text("Query History")),
            )
            .menu(move |window, cx| {
                let history = query_history(&config, cx);
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for statement in history.into_iter().take(50) {
                        let this = this.clone();
                        let label = history_label(&statement);
                        menu = menu.entry(label, None, move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.insert_history(statement.clone(), window, cx);
                            })
                            .ok();
                        });
                    }
                    menu
                }))
            })
            .into_any_element()
    }

    fn render_result_tabs(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.results.len() < 2 {
            return None;
        }
        Some(
            h_flex()
                .w_full()
                .gap_1()
                .px_2()
                .py_0p5()
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .children(self.results.iter().enumerate().map(|(index, result)| {
                    let label = match result.rows_affected {
                        Some(rows) if result.columns.is_empty() => {
                            format!("{} · {rows} affected", index + 1)
                        }
                        _ => format!("{} · {} rows", index + 1, result.rows.len()),
                    };
                    Button::new(("quarry-result", index), label)
                        .label_size(LabelSize::Small)
                        .toggle_state(index == self.active_result)
                        .on_click(cx.listener(move |this, _, _, cx| this.select_result(index, cx)))
                        .into_any_element()
                }))
                .into_any_element(),
        )
    }

    fn render_edit_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending_edit.as_ref()?;
        let result = self.results.get(pending.result_index)?;
        let column = result.columns.get(pending.column)?.clone();
        Some(
            h_flex()
                .w_full()
                .gap_1()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new(SharedString::from(format!(
                        "{column} (row {})",
                        pending.row + 1
                    )))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(div().flex_1().child(pending.editor.clone()))
                .child(
                    Button::new("quarry-edit-null", "Set NULL")
                        .label_size(LabelSize::Small)
                        .disabled(pending.saving)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_edit(true, window, cx);
                        })),
                )
                .child(
                    Button::new("quarry-edit-save", "Save")
                        .label_size(LabelSize::Small)
                        .disabled(pending.saving)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.save_edit(false, window, cx);
                        })),
                )
                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                    this.save_edit(false, window, cx);
                }))
                .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.pending_edit = None;
                    this.table.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    fn render_status(&self, cx: &App) -> AnyElement {
        let (icon, color, text) = match &self.status {
            Status::Connecting => (
                IconName::ArrowCircle,
                Color::Info,
                SharedString::from(format!("Connecting to {}…", self.config.name)),
            ),
            Status::Idle => (
                IconName::PlayOutlined,
                Color::Muted,
                SharedString::from(format!(
                    "Ready — {} on {}. The selection, or the statement under the cursor, runs; nothing else does.",
                    self.config.engine.query_language(),
                    self.config.name
                )),
            ),
            Status::Running {
                started,
                statements,
            } => (
                IconName::ArrowCircle,
                Color::Info,
                SharedString::from(if *statements > 1 {
                    format!(
                        "Running {statements} statements… {:.1}s",
                        started.elapsed().as_secs_f32()
                    )
                } else {
                    "Running…".to_string()
                }),
            ),
            Status::Cancelled => (IconName::XCircle, Color::Warning, "Cancelled".into()),
            Status::Failed(error) => (IconName::XCircle, Color::Error, error.clone()),
            Status::Updated(message) => (IconName::Check, Color::Success, message.clone()),
            Status::Succeeded {
                statements,
                elapsed,
            } => {
                let mut text = match self.active() {
                    Some(result) => result_summary(result),
                    None => "Done".to_string(),
                };
                if *statements > 1 {
                    text.push_str(&format!(" · {statements} statements"));
                }
                if !elapsed.is_zero() {
                    text.push_str(&format!(" · {} ms", elapsed.as_millis()));
                }
                if let Some(result) = self.active()
                    && !result.notices.is_empty()
                {
                    text.push_str(" · ");
                    text.push_str(
                        &result
                            .notices
                            .iter()
                            .map(|notice| notice.as_ref())
                            .collect::<Vec<_>>()
                            .join(" · "),
                    );
                }
                (IconName::Check, Color::Success, SharedString::from(text))
            }
        };

        h_flex()
            .w_full()
            .gap_1p5()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            .child(Label::new(text).size(LabelSize::Small).color(color))
            .into_any_element()
    }
}

pub fn environment_badge(environment: Environment) -> Label {
    Label::new(environment.label().to_uppercase())
        .size(LabelSize::XSmall)
        .weight(gpui::FontWeight::BOLD)
        .color(environment.color())
}

fn split_statements(chunks: &[String], engine: Engine) -> Vec<String> {
    chunks
        .iter()
        .flat_map(|chunk| {
            statements::split(chunk, engine)
                .into_iter()
                .filter_map(|range| chunk.get(range).map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn result_summary(result: &ResultSet) -> String {
    if let Some(message) = result.message()
        && result.columns.is_empty()
    {
        return message.to_string();
    }
    let rows = result.rows.len();
    let mut summary = if result.truncated {
        format!("{rows} rows (truncated at {ROW_LIMIT})")
    } else {
        format!("{rows} rows")
    };
    if result.edit_target.is_some() {
        summary.push_str(" · double-click a cell to edit");
    }
    summary
}

fn history_label(statement: &str) -> SharedString {
    let single_line = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut label: String = single_line.chars().take(80).collect();
    if single_line.chars().count() > 80 {
        label.push('…');
    }
    label.into()
}

fn placeholder(engine: Engine) -> &'static str {
    match engine {
        Engine::Redis => "SCAN 0 MATCH user:* COUNT 100",
        Engine::MongoDb => "{ \"find\": \"collection\", \"filter\": {}, \"limit\": 50 }",
        Engine::SqlServer => "SELECT TOP 100 * FROM sys.tables",
        Engine::Postgres | Engine::ClickHouse => "SELECT 1",
    }
}

#[derive(Clone, Copy)]
enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
        }
    }
}

impl Focusable for QueryView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<ItemEvent> for QueryView {}

impl Item for QueryView {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Server))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(format!(
            "{} · {}{}",
            self.config.name,
            self.config.engine.label(),
            self.database
                .as_ref()
                .map(|database| format!(" · {database}"))
                .unwrap_or_default()
        )))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        h_flex()
            .gap_1()
            .when_some(self.environment(cx), |tab, environment| {
                tab.child(environment_badge(environment))
            })
            .child(Label::new(self.title.clone()).color(params.text_color()))
            .into_any_element()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for QueryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("Quarry")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            // A frame in the error colour, so a production tab cannot pass for any other.
            .when(
                self.environment(cx) == Some(Environment::Production),
                |view| view.border_2().border_color(cx.theme().status().error),
            )
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::cancel_query))
            // Base keymaps (VS Code, JetBrains, TextMate) bind the run key to
            // `editor::NewlineBelow` at the editor's own depth, and they load after Zed's
            // defaults, so the Quarry binding can never outrank them. Capturing the action they
            // dispatch keeps the run key working under every base keymap; the cost is that
            // "insert line below" is not available in the query editor.
            .capture_action(
                cx.listener(|this, _: &editor::actions::NewlineBelow, window, cx| {
                    if !this.editor.focus_handle(cx).is_focused(window) {
                        return;
                    }
                    cx.stop_propagation();
                    if window.modifiers().shift {
                        this.run_all(&RunAll, window, cx);
                    } else {
                        this.run_query(&RunQuery, window, cx);
                    }
                }),
            )
            .child(self.render_toolbar(cx))
            .child(
                // The editor is given a share of the tab rather than the whole of it: a query
                // is usually a few lines and the result is the thing being read.
                div()
                    .h(relative(0.35))
                    .w_full()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.editor.clone()),
            )
            .children(self.render_result_tabs(cx))
            .child(div().flex_1().min_h_0().w_full().child(self.table.clone()))
            .children(self.render_edit_bar(cx))
            .child(self.render_status(cx))
    }
}

/// What a query tab needs to come back after a restart: where it was connected and what was
/// typed. Results are not kept — rerunning is one keystroke, and stale rows would mislead.
#[derive(serde::Serialize, serde::Deserialize)]
struct SerializedQueryView {
    config: ConnectionConfig,
    database: Option<String>,
    title: String,
    text: String,
}

/// Query tabs are stored in the scoped key-value store rather than a table of their own, for the
/// same reason connections are: a new table means a migration in a crate this fork rebases.
const TABS_NAMESPACE: &str = "quarry_query_tabs";

fn tab_key(workspace_id: WorkspaceId, item_id: ItemId) -> String {
    format!("{}:{item_id}", i64::from(workspace_id))
}

/// Lists the tabs saved for a workspace, so `cleanup` can find the ones that were closed: the
/// scoped store has no way to enumerate its keys.
fn index_key(workspace_id: WorkspaceId) -> String {
    format!("index:{}", i64::from(workspace_id))
}

fn read_index(kvp: &KeyValueStore, workspace_id: WorkspaceId) -> Vec<ItemId> {
    kvp.scoped(TABS_NAMESPACE)
        .read(&index_key(workspace_id))
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
        .unwrap_or_default()
}

impl SerializableItem for QueryView {
    fn serialized_item_kind() -> &'static str {
        "QuarryQueryView"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            let index = read_index(&kvp, workspace_id);
            let store = kvp.scoped(TABS_NAMESPACE);
            let mut kept = Vec::new();
            for item_id in index {
                if alive_items.contains(&item_id) {
                    kept.push(item_id);
                } else {
                    store.delete(tab_key(workspace_id, item_id)).await?;
                }
            }
            store
                .write(index_key(workspace_id), serde_json::to_string(&kept)?)
                .await
        })
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let saved = KeyValueStore::global(cx)
            .scoped(TABS_NAMESPACE)
            .read(&tab_key(workspace_id, item_id))
            .and_then(|json| {
                let json = json.ok_or_else(|| anyhow!("no saved Quarry tab {item_id}"))?;
                Ok(serde_json::from_str::<SerializedQueryView>(&json)?)
            });
        let languages = project.read(cx).languages().clone();
        window.spawn(cx, async move |cx| {
            let saved = saved?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    QueryView::new(
                        saved.config,
                        None,
                        saved.database.map(SharedString::from),
                        saved.title.into(),
                        Some(saved.text),
                        Some(languages),
                        window,
                        cx,
                    )
                })
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let saved = SerializedQueryView {
            config: self.config.clone(),
            database: self.database.as_ref().map(|database| database.to_string()),
            title: self.title.to_string(),
            text: self.editor.read(cx).text(cx),
        };
        let json = serde_json::to_string(&saved).log_err()?;
        let kvp = KeyValueStore::global(cx);
        Some(cx.background_spawn(async move {
            let mut index = read_index(&kvp, workspace_id);
            if !index.contains(&item_id) {
                index.push(item_id);
            }
            let store = kvp.scoped(TABS_NAMESPACE);
            store.write(tab_key(workspace_id, item_id), json).await?;
            store
                .write(index_key(workspace_id), serde_json::to_string(&index)?)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::Edit)
    }
}
