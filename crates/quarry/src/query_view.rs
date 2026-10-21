use crate::{
    driver::{Driver, Engine, ResultSet},
    result_table,
    runtime::QuarryRuntime,
};
use editor::Editor;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Render, Task, Window, actions,
};
use std::sync::Arc;
use tabular_data_preview::TableView;
use ui::{Color, Icon, IconName, Label, LabelCommon as _, SharedString, prelude::*};
use workspace::item::{Item, ItemEvent, TabContentParams};

actions!(
    quarry,
    [
        /// Runs the query in the editor against this tab's connection.
        RunQuery,
    ]
);

/// Results are capped rather than streamed. A browser's job is to let someone look at data, and
/// a `SELECT *` against a large table would otherwise spend minutes filling memory with rows
/// nobody scrolls to. The view says so when it truncates, so the cap is never silent.
const ROW_LIMIT: usize = 2_000;

enum Status {
    Idle,
    Running,
    Failed(SharedString),
    Succeeded {
        row_count: usize,
        truncated: bool,
        message: Option<SharedString>,
    },
}

pub struct QueryView {
    focus_handle: FocusHandle,
    editor: Entity<Editor>,
    table: Entity<TableView>,
    driver: Arc<dyn Driver>,
    connection_name: SharedString,
    engine: Engine,
    status: Status,
    _running: Option<Task<()>>,
}

impl QueryView {
    pub fn new(
        connection_name: SharedString,
        engine: Engine,
        driver: Arc<dyn Driver>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(
                match engine {
                    Engine::Redis => "KEYS *",
                    Engine::MongoDb => "{ \"find\": \"collection\" }",
                    _ => "SELECT 1",
                },
                window,
                cx,
            );
            editor
        });

        Self {
            focus_handle: cx.focus_handle(),
            editor,
            table: cx.new(|cx| TableView::new(window, cx)),
            driver,
            connection_name,
            engine,
            status: Status::Idle,
            _running: None,
        }
    }

    /// Replaces the grid with `result`, for the table preview that opens from the tree.
    pub fn show_result(&mut self, result: ResultSet, cx: &mut Context<Self>) {
        self.apply(result, cx);
    }

    fn run(&mut self, _: &RunQuery, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        if text.trim().is_empty() {
            return;
        }

        let Some(runtime) = QuarryRuntime::global(cx) else {
            self.status = Status::Failed("the Quarry runtime is not available".into());
            cx.notify();
            return;
        };

        self.status = Status::Running;
        self.table
            .update(cx, |table, cx| table.set_loading(true, cx));
        cx.notify();

        // The driver's future must be polled by tokio, never by gpui's executor: several of
        // these drivers panic without a tokio reactor in scope. `spawn` hands it over and gives
        // back a plain future this task can await.
        let driver = self.driver.clone();
        let query = runtime.spawn(async move { driver.query(&text, ROW_LIMIT).await });

        self._running = Some(cx.spawn(async move |this, cx| {
            let outcome = query.await;
            this.update(cx, |this, cx| {
                this.table
                    .update(cx, |table, cx| table.set_loading(false, cx));
                match outcome {
                    Ok(result) => this.apply(result, cx),
                    Err(error) => {
                        // The whole error chain, since the useful part of a connection failure
                        // is usually the innermost cause rather than "running: SELECT ...".
                        this.status = Status::Failed(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    fn apply(&mut self, result: ResultSet, cx: &mut Context<Self>) {
        self.status = Status::Succeeded {
            row_count: result.rows.len(),
            truncated: result.truncated,
            message: result.message(),
        };
        let contents = result_table::to_table_content(&result);
        self.table
            .update(cx, |table, cx| table.set_contents(contents, cx));
        cx.notify();
    }

    fn render_status(&self, cx: &App) -> AnyElement {
        let (icon, color, text) = match &self.status {
            Status::Idle => (
                IconName::PlayOutlined,
                Color::Muted,
                SharedString::from(format!(
                    "Ready — {} on {}",
                    self.engine.query_language(),
                    self.connection_name
                )),
            ),
            Status::Running => (IconName::ArrowCircle, Color::Info, "Running…".into()),
            Status::Failed(error) => (IconName::XCircle, Color::Error, error.clone()),
            Status::Succeeded {
                row_count,
                truncated,
                message,
            } => {
                let text = if let Some(message) = message {
                    message.clone()
                } else if *truncated {
                    format!("{row_count} rows (truncated at {ROW_LIMIT})").into()
                } else {
                    format!("{row_count} rows").into()
                };
                (IconName::Check, Color::Success, text)
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
        self.connection_name.clone()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        Label::new(self.connection_name.clone())
            .color(params.text_color())
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
            .on_action(cx.listener(Self::run))
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
            .child(div().flex_1().min_h_0().w_full().child(self.table.clone()))
            .child(self.render_status(cx))
    }
}
