use acp_thread::{AcpThread, AcpThreadEvent};
use editor::Editor;
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription,
    Window, prelude::*,
};
use ui::prelude::*;
use workspace::{
    Item, Workspace,
    item::{FollowableItem as _, ItemEvent, TabContentParams},
};

use crate::DEFAULT_THREAD_TITLE;

pub enum AgentFollowTabEvent {
    TitleChanged,
}

/// A center tab that follows one agent session: it shows the file the session
/// last read or edited, so each session can be watched in its own tab while
/// the others keep working.
pub struct AgentFollowTab {
    thread: Entity<AcpThread>,
    editor: Option<Entity<Editor>>,
    agent_name: SharedString,
    icon: IconName,
    icon_from_external_svg: Option<SharedString>,
    focus_handle: FocusHandle,
    _thread_subscription: Subscription,
}

impl AgentFollowTab {
    /// Opens the session's tab, or finds the one already open, without
    /// switching to it unless `activate` is set.
    pub fn deploy(
        thread: Entity<AcpThread>,
        agent_name: SharedString,
        icon: IconName,
        icon_from_external_svg: Option<SharedString>,
        activate: bool,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|tab| tab.read(cx).thread == thread);
        if let Some(existing) = existing {
            if activate {
                workspace.activate_item(&existing, true, false, window, cx);
            }
            return existing;
        }

        let tab =
            cx.new(|cx| Self::new(thread, agent_name, icon, icon_from_external_svg, window, cx));
        workspace.active_pane().update(cx, |pane, cx| {
            pane.add_item_inner(
                Box::new(tab.clone()),
                activate,
                false,
                activate,
                None,
                window,
                cx,
            );
        });
        tab
    }

    fn new(
        thread: Entity<AcpThread>,
        agent_name: SharedString,
        icon: IconName,
        icon_from_external_svg: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let thread_subscription =
            cx.subscribe_in(&thread, window, |this, _, event, window, cx| match event {
                AcpThreadEvent::AgentLocationChanged => this.show_agent_location(window, cx),
                AcpThreadEvent::TitleUpdated => cx.emit(AgentFollowTabEvent::TitleChanged),
                _ => {}
            });
        let mut this = Self {
            thread,
            editor: None,
            agent_name,
            icon,
            icon_from_external_svg,
            focus_handle: cx.focus_handle(),
            _thread_subscription: thread_subscription,
        };
        this.show_agent_location(window, cx);
        this
    }

    fn show_agent_location(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.thread.read(cx).agent_location().cloned() else {
            return;
        };
        let Some(buffer) = location.buffer.upgrade() else {
            return;
        };

        let shows_buffer = self.editor.as_ref().is_some_and(|editor| {
            editor.read(cx).buffer().read(cx).as_singleton().as_ref() == Some(&buffer)
        });
        if !shows_buffer {
            let project = self.thread.read(cx).project().clone();
            let had_focus = self.focus_handle(cx).contains_focused(window, cx);
            let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
            if had_focus {
                editor.focus_handle(cx).focus(window, cx);
            }
            self.editor = Some(editor);
        }

        if let Some(editor) = &self.editor {
            editor.update(cx, |editor, cx| {
                editor.update_agent_location(location.position, window, cx)
            });
        }
        cx.notify();
    }
}

impl EventEmitter<AgentFollowTabEvent> for AgentFollowTab {}

impl Focusable for AgentFollowTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.editor {
            Some(editor) => editor.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Item for AgentFollowTab {
    type Event = AgentFollowTabEvent;

    fn to_item_events(event: &AgentFollowTabEvent, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            AgentFollowTabEvent::TitleChanged => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        let icon = match &self.icon_from_external_svg {
            Some(svg) => Icon::from_external_svg(svg.clone()),
            None => Icon::new(self.icon),
        };
        Some(icon.color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.thread
            .read(cx)
            .title()
            .unwrap_or_else(|| DEFAULT_THREAD_TITLE.into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .when(!params.selected, |label| label.color(Color::Muted))
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(format!("Following {}", self.agent_name).into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

impl Render for AgentFollowTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match &self.editor {
            Some(editor) => div().size_full().child(editor.clone()).into_any_element(),
            None => v_flex()
                .track_focus(&self.focus_handle)
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new(format!("{} has not opened a file yet.", self.agent_name))
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }
}
