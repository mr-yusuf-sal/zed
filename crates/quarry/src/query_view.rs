use crate::{
    completion::QuarryCompletionProvider,
    connection_store::{ConnectionStore, query_history, record_query},
    driver::{CatalogEntry, CellEdit, ConnectionConfig, Driver, Engine, Environment, ResultSet},
    result_table,
    runtime::QuarryRuntime,
    statements, templates,
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

/// A preview opened from the tree shows this many rows at first, and "Load More" adds this many
/// more each time.
pub const PREVIEW_ROW_LIMIT: usize = 500;

/// What a run asks of the driver for each statement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Query,
    /// The statement's plan; with `analyze`, measured by running it.
    Explain {
        analyze: bool,
    },
}

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

/// The form under the grid for adding a row to the object the active result came from.
struct PendingInsert {
    path: Vec<SharedString>,
    /// Each column the new row is given a value for, with the editor it is typed in.
    fields: Vec<(SharedString, Entity<Editor>)>,
    saving: bool,
}

/// The table, collection or key a preview tab was opened on, kept while its rows are on screen
/// so that "Load More" can read the rows after them. They come from `QueryView::driver`, which
/// is the driver the panel read the first page with.
struct PreviewSource {
    path: Vec<SharedString>,
    /// The next page while it is being read. Dropping it abandons the read.
    loading: Option<Task<()>>,
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
    /// Narrows the grid to rows with a cell containing this text.
    search: Entity<Editor>,
    /// For each row in the grid, the row of the active result it shows. The search hides rows,
    /// so a grid row number is not a result row number.
    visible_rows: Vec<usize>,
    status: Status,
    pending_edit: Option<PendingEdit>,
    pending_insert: Option<PendingInsert>,
    /// Set while the grid shows a preview from the tree; it is the only result then. Any other
    /// result replacing it clears this.
    preview: Option<PreviewSource>,
    /// The read-only editor beside the grid showing the selected row as JSON, while open.
    record: Option<Entity<Editor>>,
    /// The result row last selected in the grid, which the record view shows.
    selected_row: Option<usize>,
    languages: Option<Arc<LanguageRegistry>>,
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

        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search results…", window, cx);
            editor
        });

        let table = cx.new(|cx| {
            let mut table = TableView::new(window, cx);
            table.set_selection_enabled(true);
            table
        });

        let subscriptions = vec![
            cx.subscribe(&search, |this, _, event, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.refresh_table(cx);
                }
            }),
            cx.subscribe_in(&table, window, |this, _, event, window, cx| match event {
                TableViewEvent::CellDoubleClicked(cell) => {
                    if let Some(row) = this.visible_rows.get(cell.row.0).copied() {
                        this.start_edit(row, cell.col.0, window, cx);
                    }
                }
                TableViewEvent::SelectionChanged(cell) => {
                    this.selected_row = this.visible_rows.get(cell.row.0).copied();
                    this.refresh_record(window, cx);
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
            search,
            visible_rows: Vec::new(),
            status: Status::Idle,
            pending_edit: None,
            pending_insert: None,
            preview: None,
            record: None,
            selected_row: None,
            languages: languages.clone(),
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

    /// Replaces the grid with `result`, the first page of the preview that opens from the tree,
    /// and keeps `path` so that the pages after it can be loaded.
    pub fn show_preview(
        &mut self,
        path: Vec<SharedString>,
        result: ResultSet,
        cx: &mut Context<Self>,
    ) {
        self.status = Status::Succeeded {
            statements: 1,
            elapsed: Duration::ZERO,
        };
        self.set_results(vec![result], cx);
        self.preview = Some(PreviewSource {
            path,
            loading: None,
        });
    }

    /// Replaces the grid with `result`, for something the panel read on the tab's behalf that is
    /// not a page of one object, such as the keys a pattern matched.
    pub fn show_result(&mut self, result: ResultSet, cx: &mut Context<Self>) {
        self.status = Status::Succeeded {
            statements: 1,
            elapsed: Duration::ZERO,
        };
        self.set_results(vec![result], cx);
    }

    /// Reads the page after the rows the preview shows and adds it to them.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        if preview.loading.is_some() {
            return;
        }
        let Some(offset) = self.results.first().map(|result| result.rows.len()) else {
            return;
        };
        let Some(driver) = self.driver.clone() else {
            return;
        };
        let Some(runtime) = QuarryRuntime::global(cx) else {
            self.status = Status::Failed("the Quarry runtime is not available".into());
            cx.notify();
            return;
        };
        let path = preview.path.clone();
        let page = runtime
            .spawn(async move { driver.preview_page(&path, PREVIEW_ROW_LIMIT, offset).await });
        preview.loading = Some(cx.spawn(async move |this, cx| {
            let page = page.await;
            this.update(cx, |this, cx| {
                if let Some(preview) = this.preview.as_mut() {
                    preview.loading = None;
                }
                match page {
                    Ok(page) => this.append_page(page, cx),
                    Err(error) => {
                        this.status = Status::Failed(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Adds `page`, read from where the preview's rows end, to the bottom of the grid. Rows and
    /// columns already shown keep their indexes, so the edit target and a cell being edited
    /// still point at the same cells.
    fn append_page(&mut self, page: ResultSet, cx: &mut Context<Self>) {
        let Some(result) = self.results.first_mut() else {
            return;
        };
        result_table::append_page(result, page);
        self.status = Status::Succeeded {
            statements: 1,
            elapsed: Duration::ZERO,
        };
        self.refresh_table(cx);
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
            self.execute(chunks, Operation::Query, cx);
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
                this.update(cx, |this, cx| this.execute(chunks, Operation::Query, cx))
                    .ok();
            }
        }));
    }

    /// Runs every statement in the editor, for a tab that was opened to run a statement the
    /// panel wrote, such as "Select Top 100".
    pub fn run_everything(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        self.execute(vec![text], Operation::Query, cx);
    }

    /// Shows the plan of the selection, or of the statement under the cursor. Analyzing runs
    /// the statement, so it is refused for anything that may write rather than asked about:
    /// a plan is never worth an accidental DELETE.
    fn explain(&mut self, analyze: bool, cx: &mut Context<Self>) {
        let engine = self.config.engine;
        let chunks = self.text_to_run(cx);
        if analyze
            && split_statements(&chunks, engine)
                .iter()
                .any(|statement| statements::may_write(statement, engine))
        {
            self.status = Status::Failed(
                "Explain Analyze runs the statement to measure it, and this one may change data; \
                 use Explain for its estimated plan"
                    .into(),
            );
            cx.notify();
            return;
        }
        self.execute(chunks, Operation::Explain { analyze }, cx);
    }

    fn execute(&mut self, chunks: Vec<String>, operation: Operation, cx: &mut Context<Self>) {
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

        if operation == Operation::Query {
            for chunk in &chunks {
                record_query(&self.config, chunk.clone(), cx);
            }
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
                let sets = match operation {
                    Operation::Query => {
                        driver
                            .query(statement, database.as_deref(), ROW_LIMIT)
                            .await
                    }
                    Operation::Explain { analyze } => {
                        driver
                            .explain(statement, database.as_deref(), analyze)
                            .await
                    }
                };
                match sets {
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
                        let changed_schema = operation == Operation::Query
                            && results.iter().any(|result| result.columns.is_empty());
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
        self.pending_insert = None;
        self.preview = None;
        self.selected_row = None;
        self.select_result(0, cx);
    }

    fn select_result(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.results.len() {
            return;
        }
        self.active_result = index;
        self.refresh_table(cx);
    }

    /// Shows the active result in the grid, narrowed to the rows the search matches.
    fn refresh_table(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(self.active_result) else {
            return;
        };
        let query = self.search.read(cx).text(cx);
        self.visible_rows = result_table::matching_rows(result, &query);
        let contents = result_table::to_table_content(result, &self.visible_rows);
        self.table
            .update(cx, |table, cx| table.set_contents(contents, cx));
        cx.notify();
    }

    /// Opens or closes the record view beside the grid.
    fn toggle_record(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.record.take().is_some() {
            cx.notify();
            return;
        }
        let record = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor.set_placeholder_text("Select a cell to see its row as JSON", window, cx);
            editor
        });
        // The JSON grammar gives the record folding, which is what makes a nested document
        // readable: each object and array folds away to one line.
        if let Some(languages) = self.languages.clone() {
            let language = languages.language_for_name("JSON");
            let record = record.downgrade();
            self._tasks.push(cx.spawn(async move |_, cx| {
                let Ok(language) = language.await else {
                    return;
                };
                record
                    .update(cx, |editor, cx| {
                        if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                            buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
                        }
                    })
                    .ok();
            }));
        }
        self.record = Some(record);
        self.refresh_record(window, cx);
    }

    fn refresh_record(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.record.clone() else {
            return;
        };
        let text = self
            .selected_row
            .zip(self.active())
            .and_then(|(row, result)| result_table::row_json(result, row))
            .unwrap_or_default();
        record.update(cx, |editor, cx| editor.set_text(text, window, cx));
        cx.notify();
    }

    /// Adds a template at the end of the editor, after whatever is already there, and selects
    /// nothing so the cursor lands in it.
    fn insert_template(&mut self, text: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_history(text.to_string(), window, cx);
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
                        if let Some(result) = this.results.get_mut(result_index) {
                            if let Some(cell) = result
                                .rows
                                .get_mut(row)
                                .and_then(|cells| cells.get_mut(column))
                            {
                                *cell = value.map(SharedString::from);
                            }
                            // The document was read before the edit; the record view builds
                            // the row from its cells from now on.
                            if let Some(document) = result.documents.get_mut(row) {
                                *document = None;
                            }
                        }
                        this.pending_edit = None;
                        this.status = Status::Updated(format!("{rows} row(s) updated").into());
                        if result_index == this.active_result {
                            this.select_result(result_index, cx);
                        }
                        this.refresh_record(window, cx);
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

    /// Opens the form for a new row, with one field per column the driver lets a new row set.
    fn start_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.active() else {
            return;
        };
        let Some(target) = result.edit_target.clone() else {
            return;
        };
        let columns: Vec<SharedString> = target
            .insert_columns
            .iter()
            .filter_map(|&index| result.columns.get(index).cloned())
            .collect();
        let fields: Vec<(SharedString, Entity<Editor>)> = columns
            .into_iter()
            .map(|column| {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text("default", window, cx);
                    editor
                });
                (column, editor)
            })
            .collect();
        if let Some((_, first)) = fields.first() {
            first.focus_handle(cx).focus(window, cx);
        }
        self.pending_edit = None;
        self.pending_insert = Some(PendingInsert {
            path: target.path,
            fields,
            saving: false,
        });
        cx.notify();
    }

    /// Adds the row the form describes, asking first on a production connection. A field left
    /// blank is left out, so the column gets its default.
    fn save_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_insert.as_ref() else {
            return;
        };
        if pending.saving {
            return;
        }
        let values: Vec<(SharedString, String)> = pending
            .fields
            .iter()
            .map(|(column, editor)| (column.clone(), editor.read(cx).text(cx)))
            .filter(|(_, text)| !text.is_empty())
            .collect();
        let path = pending.path.clone();
        if self.environment(cx) != Some(Environment::Production) {
            self.insert_row(path, values, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Add a row on production ({})?", self.config.name),
            Some(&format!("A new row is added to {}.", path.join("."))),
            &["Add", "Cancel"],
            cx,
        );
        self._tasks.push(cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |this, cx| this.insert_row(path, values, cx))
                    .ok();
            }
        }));
    }

    fn insert_row(
        &mut self,
        path: Vec<SharedString>,
        values: Vec<(SharedString, String)>,
        cx: &mut Context<Self>,
    ) {
        let (Some(driver), Some(runtime)) = (self.driver.clone(), QuarryRuntime::global(cx)) else {
            return;
        };
        if let Some(pending) = self.pending_insert.as_mut() {
            pending.saving = true;
        }
        let inserted = {
            let path = path.clone();
            runtime.spawn(async move { driver.insert_row(&path, &values).await })
        };
        self._tasks.push(cx.spawn(async move |this, cx| {
            let inserted = inserted.await;
            this.update(cx, |this, cx| {
                match inserted {
                    Ok(rows) => {
                        this.pending_insert = None;
                        this.reload_after_change(path, format!("{rows} row(s) added"), cx);
                    }
                    Err(error) => {
                        if let Some(pending) = this.pending_insert.as_mut() {
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

    /// Deletes the row selected in the grid, after asking.
    fn confirm_delete_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row else {
            self.status = Status::Failed("select a cell in the row to delete first".into());
            cx.notify();
            return;
        };
        let Some(result) = self.active() else {
            return;
        };
        let Some(target) = result.edit_target.clone() else {
            return;
        };
        let Some(cells) = result.rows.get(row) else {
            return;
        };
        let key: Vec<(SharedString, Option<SharedString>)> = target
            .key_columns
            .iter()
            .filter_map(|&index| {
                Some((
                    result.columns.get(index)?.clone(),
                    cells.get(index).cloned().flatten(),
                ))
            })
            .collect();
        if key.is_empty() {
            return;
        }
        let description = key
            .iter()
            .map(|(column, value)| format!("{column} = {}", value.as_deref().unwrap_or("NULL")))
            .collect::<Vec<_>>()
            .join(", ");
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the row {description}?"),
            Some(&format!(
                "It is removed from {} on {}.",
                target.path.join("."),
                self.config.name
            )),
            &["Delete", "Cancel"],
            cx,
        );
        let (Some(driver), Some(runtime)) = (self.driver.clone(), QuarryRuntime::global(cx)) else {
            return;
        };
        let path = target.path;
        self._tasks.push(cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let deleted = {
                let path = path.clone();
                runtime.spawn(async move { driver.delete_row(&path, &key).await })
            }
            .await;
            this.update(cx, |this, cx| {
                match deleted {
                    Ok(rows) => {
                        this.reload_after_change(path, format!("{rows} row(s) deleted"), cx)
                    }
                    Err(error) => this.status = Status::Failed(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Reads the first page of `path` again after a row was added or deleted there, so the grid
    /// shows the object as it now is, and says what changed.
    fn reload_after_change(
        &mut self,
        path: Vec<SharedString>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let (Some(driver), Some(runtime)) = (self.driver.clone(), QuarryRuntime::global(cx)) else {
            return;
        };
        let page = {
            let path = path.clone();
            runtime.spawn(async move { driver.preview(&path, PREVIEW_ROW_LIMIT).await })
        };
        self._tasks.push(cx.spawn(async move |this, cx| {
            let page = page.await;
            this.update(cx, |this, cx| {
                match page {
                    Ok(page) => {
                        this.show_preview(path, page, cx);
                        this.status = Status::Updated(message.into());
                    }
                    Err(error) => {
                        this.status = Status::Failed(
                            format!("{message}, but reading it again failed: {error:#}").into(),
                        )
                    }
                }
                cx.notify();
            })
            .ok();
        }));
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
        let target = self.active().and_then(|result| result.edit_target.as_ref());
        let can_insert = target.is_some_and(|target| !target.insert_columns.is_empty());
        let can_delete = target.is_some_and(|target| !target.key_columns.is_empty());
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
            .when(self.config.engine != Engine::Redis, |toolbar| {
                toolbar.child(self.render_explain_menu(this.clone(), running))
            })
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
            .when(can_insert, |toolbar| {
                toolbar.child(
                    IconButton::new("quarry-add-row", IconName::Plus)
                        .icon_size(IconSize::Small)
                        .disabled(running || self.pending_insert.is_some())
                        .tooltip(Tooltip::text("Add Row"))
                        .on_click(cx.listener(|this, _, window, cx| this.start_insert(window, cx))),
                )
            })
            .when(can_delete, |toolbar| {
                toolbar.child(
                    IconButton::new("quarry-delete-row", IconName::Trash)
                        .icon_size(IconSize::Small)
                        .disabled(running || self.selected_row.is_none())
                        .tooltip(Tooltip::text("Delete the Selected Row"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_delete_row(window, cx)),
                        ),
                )
            })
            .when(has_result, |toolbar| {
                toolbar.child(div().w_48().child(self.search.clone()))
            })
            .child(self.render_templates_menu(this.clone()))
            .child(self.render_history_menu(this.clone(), cx))
            .child(
                IconButton::new("quarry-record", IconName::Json)
                    .icon_size(IconSize::Small)
                    .toggle_state(self.record.is_some())
                    .tooltip(Tooltip::text("Show the Selected Row as JSON"))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_record(window, cx))),
            )
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

    fn render_explain_menu(&self, this: WeakEntity<Self>, running: bool) -> AnyElement {
        // ClickHouse has no way to measure a plan by running it.
        let can_analyze = self.config.engine != Engine::ClickHouse;
        PopoverMenu::new("quarry-explain")
            .trigger(
                IconButton::new("quarry-explain-trigger", IconName::ListTree)
                    .icon_size(IconSize::Small)
                    .disabled(running || self.driver.is_none())
                    .tooltip(Tooltip::text("Explain")),
            )
            .menu(move |window, cx| {
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    let estimated = this.clone();
                    menu.entry("Explain", None, move |_, cx| {
                        estimated
                            .update(cx, |this, cx| this.explain(false, cx))
                            .ok();
                    })
                    .when(can_analyze, |menu| {
                        menu.entry(
                            "Explain Analyze (runs the statement)",
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| this.explain(true, cx)).ok();
                            },
                        )
                    })
                }))
            })
            .into_any_element()
    }

    fn render_templates_menu(&self, this: WeakEntity<Self>) -> AnyElement {
        let engine = self.config.engine;
        PopoverMenu::new("quarry-templates")
            .trigger(
                IconButton::new("quarry-templates-trigger", IconName::TextSnippet)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Insert a Template")),
            )
            .menu(move |window, cx| {
                let this = this.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for (label, text) in templates::templates(engine) {
                        let this = this.clone();
                        menu = menu.entry(*label, None, move |window, cx| {
                            this.update(cx, |this, cx| this.insert_template(text, window, cx))
                                .ok();
                        });
                    }
                    menu
                }))
            })
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

    fn render_insert_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending_insert.as_ref()?;
        Some(
            v_flex()
                .w_full()
                .gap_0p5()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    Label::new(SharedString::from(format!(
                        "New row in {} · blank fields get their default",
                        pending.path.join(".")
                    )))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .id("quarry-insert-fields")
                        .max_h(px(200.))
                        .overflow_y_scroll()
                        .gap_0p5()
                        .children(pending.fields.iter().map(|(column, editor)| {
                            h_flex()
                                .gap_1()
                                .child(div().w_32().flex_none().child(
                                    Label::new(column.clone()).size(LabelSize::Small).truncate(),
                                ))
                                .child(div().flex_1().child(editor.clone()))
                        })),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("quarry-insert-save", "Add Row")
                                .label_size(LabelSize::Small)
                                .disabled(pending.saving)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.save_insert(window, cx)),
                                ),
                        )
                        .child(
                            Button::new("quarry-insert-cancel", "Cancel")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pending_insert = None;
                                    cx.notify();
                                })),
                        ),
                )
                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                    this.save_insert(window, cx);
                }))
                .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.pending_insert = None;
                    this.table.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
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
                if let Some(result) = self.active()
                    && self.visible_rows.len() < result.rows.len()
                {
                    text.push_str(&format!(" · {} match the search", self.visible_rows.len()));
                }
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
            .when_some(self.render_load_more(cx), |status, button| {
                status.child(div().flex_1()).child(button)
            })
            .into_any_element()
    }

    /// Offered under a preview whose last page was cut short. It stays after an edit or a failed
    /// query, which leave the preview's rows in place, so the status text is not what decides.
    fn render_load_more(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.preview.as_ref()?;
        if !self.results.first()?.truncated {
            return None;
        }
        let loading = preview.loading.is_some();
        Some(
            div()
                .flex_none()
                .child(
                    Button::new(
                        "quarry-load-more",
                        if loading { "Loading…" } else { "Load More" },
                    )
                    .label_size(LabelSize::Small)
                    .disabled(loading || self.running.is_some())
                    .tooltip(Tooltip::text(format!(
                        "Add the next {PREVIEW_ROW_LIMIT} rows"
                    )))
                    .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                )
                .into_any_element(),
        )
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
    // The row count already says where the result was cut: at ROW_LIMIT for a query, but at a
    // multiple of PREVIEW_ROW_LIMIT for a preview and the pages loaded under it.
    let mut summary = if result.truncated {
        format!("{rows} rows (truncated)")
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
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(div().flex_1().min_w_0().h_full().child(self.table.clone()))
                    .children(self.record.clone().map(|record| {
                        div()
                            .w(relative(0.4))
                            .h_full()
                            .border_l_1()
                            .border_color(cx.theme().colors().border)
                            .child(record)
                    })),
            )
            .children(self.render_edit_bar(cx))
            .children(self.render_insert_bar(cx))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{EditTarget, ObjectNode};
    use gpui::TestAppContext;
    use parking_lot::Mutex;
    use settings::SettingsStore;

    /// Numbered rows handed out a page at a time, recording the offset of every page asked for.
    /// As with MongoDB, a page has the columns its own rows have: the last ten rows carry one
    /// more, and it comes first in their pages.
    struct PagedTable {
        rows: usize,
        offsets: Mutex<Vec<usize>>,
    }

    impl PagedTable {
        fn page(&self, path: &[SharedString], row_limit: usize, offset: usize) -> ResultSet {
            let end = self.rows.min(offset.saturating_add(row_limit));
            let numbers = offset.min(end)..end;
            let is_late = |number: usize| number + 10 >= self.rows;
            let has_late = numbers.clone().any(is_late);
            let mut columns = vec![SharedString::from("_id"), SharedString::from("name")];
            if has_late {
                columns.insert(0, SharedString::from("late"));
            }
            let rows = numbers
                .map(|number| {
                    let mut row = vec![
                        Some(SharedString::from(number.to_string())),
                        Some(SharedString::from(format!("user {number}"))),
                    ];
                    if has_late {
                        row.insert(0, is_late(number).then(|| SharedString::from("yes")));
                    }
                    row
                })
                .collect();
            let id_column = usize::from(has_late);
            ResultSet {
                columns,
                rows,
                truncated: end < self.rows,
                edit_target: Some(EditTarget {
                    path: path.to_vec(),
                    key_columns: vec![id_column],
                    read_only_columns: vec![id_column],
                    insert_columns: Vec::new(),
                }),
                ..ResultSet::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl Driver for PagedTable {
        async fn children(&self, _path: &[SharedString]) -> Result<Vec<ObjectNode>> {
            Ok(Vec::new())
        }

        async fn query(
            &self,
            _statement: &str,
            _database: Option<&str>,
            _row_limit: usize,
        ) -> Result<Vec<ResultSet>> {
            Ok(Vec::new())
        }

        async fn preview_page(
            &self,
            path: &[SharedString],
            row_limit: usize,
            offset: usize,
        ) -> Result<ResultSet> {
            self.offsets.lock().push(offset);
            Ok(self.page(path, row_limit, offset))
        }

        async fn databases(&self) -> Result<Vec<SharedString>> {
            Ok(Vec::new())
        }

        fn default_database(&self) -> Option<SharedString> {
            None
        }

        async fn catalog(&self, _database: Option<&str>) -> Result<Vec<CatalogEntry>> {
            Ok(Vec::new())
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            QuarryRuntime::init(cx);
        });
        // Pages are read on the Quarry tokio runtime, whose threads the test scheduler does not
        // run, so the test has to wait for them in real time.
        cx.executor().allow_parking();
    }

    fn names(names: &[&'static str]) -> Vec<SharedString> {
        names.iter().map(|name| SharedString::from(*name)).collect()
    }

    #[gpui::test]
    async fn test_load_more_appends_the_next_pages(cx: &mut TestAppContext) {
        init_test(cx);
        let table = Arc::new(PagedTable {
            rows: PREVIEW_ROW_LIMIT * 2 + 10,
            offsets: Mutex::new(Vec::new()),
        });
        let path = names(&["app", "users"]);
        let config = ConnectionConfig {
            name: "test".to_string(),
            engine: Engine::MongoDb,
            host: "localhost".to_string(),
            port: 27017,
            database: None,
            username: None,
            environment: None,
            folder: None,
        };
        let (view, cx) = cx.add_window_view({
            let driver: Arc<dyn Driver> = table.clone();
            move |window, cx| {
                QueryView::new(
                    config,
                    Some(driver),
                    None,
                    "users".into(),
                    None,
                    None,
                    window,
                    cx,
                )
            }
        });

        view.update_in(cx, |view, window, cx| {
            view.show_preview(path.clone(), table.page(&path, PREVIEW_ROW_LIMIT, 0), cx);
            view.start_edit(3, 1, window, cx);
            view.search.update(cx, |search, cx| {
                search.set_text("user 50", window, cx);
            });
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.visible_rows, vec![50]);
            assert!(
                view.preview
                    .as_ref()
                    .is_some_and(|preview| preview.loading.is_none())
            );
        });

        let page_loaded = |view: &mut QueryView, _: &mut Context<QueryView>| {
            view.preview
                .as_ref()
                .is_some_and(|preview| preview.loading.is_none())
        };
        view.update(cx, |view, cx| view.load_more(cx));
        cx.condition(&view, page_loaded).await;
        view.read_with(cx, |view, _| {
            let result = &view.results[0];
            assert_eq!(result.rows.len(), PREVIEW_ROW_LIMIT * 2);
            assert!(result.truncated);
            assert_eq!(result.columns, names(&["_id", "name"]));
            assert_eq!(
                result.rows[PREVIEW_ROW_LIMIT][0].as_deref(),
                Some(PREVIEW_ROW_LIMIT.to_string().as_str())
            );
            let mut expected_matches = vec![50];
            expected_matches.extend(500..510);
            assert_eq!(view.visible_rows, expected_matches);
            let pending = view
                .pending_edit
                .as_ref()
                .expect("the cell being edited is still being edited");
            assert_eq!((pending.row, pending.column), (3, 1));
        });

        view.update(cx, |view, cx| view.load_more(cx));
        cx.condition(&view, page_loaded).await;
        view.read_with(cx, |view, _| {
            let result = &view.results[0];
            assert_eq!(result.rows.len(), PREVIEW_ROW_LIMIT * 2 + 10);
            assert!(!result.truncated);
            assert_eq!(result.columns, names(&["_id", "name", "late"]));
            assert_eq!(result.rows[0][2], None);
            assert_eq!(
                result.rows[PREVIEW_ROW_LIMIT * 2][2].as_deref(),
                Some("yes")
            );
            assert_eq!(
                result
                    .edit_target
                    .as_ref()
                    .map(|target| target.key_columns.clone()),
                Some(vec![0])
            );
            let edit = view
                .cell_edit(0, PREVIEW_ROW_LIMIT * 2 + 5, 1, Some("x".to_string()))
                .expect("a row from the last page is editable");
            assert_eq!(
                edit.key,
                vec![(
                    SharedString::from("_id"),
                    Some(SharedString::from((PREVIEW_ROW_LIMIT * 2 + 5).to_string()))
                )]
            );
            assert_eq!(edit.column, SharedString::from("name"));
        });
        assert_eq!(
            *table.offsets.lock(),
            vec![PREVIEW_ROW_LIMIT, PREVIEW_ROW_LIMIT * 2]
        );

        view.update(cx, |view, cx| {
            view.set_results(vec![ResultSet::default()], cx)
        });
        view.read_with(cx, |view, _| assert!(view.preview.is_none()));
    }
}
