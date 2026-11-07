use anyhow::{Result, anyhow};
use editor::Editor;
use futures::channel::oneshot;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel,
    SharedString, Task, Window,
};
use menu::{Cancel, Confirm};
use project::git_store::Repository;
use ui::{
    Button, ButtonStyle, Color, Headline, HeadlineSize, Icon, IconButton, IconName, IconSize,
    Label, LabelSize, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::ModalView;

const DEFAULT_REMOTE: &str = "origin";

/// Lists the repository's remotes with their URLs, and adds, renames, removes
/// them or points them at another URL. The list is read again after every
/// change, so it always shows what git has.
pub(crate) struct RemotesModal {
    repository: Entity<Repository>,
    /// Remote names and URLs, `origin` first and the rest by name.
    remotes: Vec<(SharedString, SharedString)>,
    loaded: bool,
    /// The remote the form edits; `None` while it adds a new one.
    editing: Option<SharedString>,
    name_editor: Entity<Editor>,
    url_editor: Entity<Editor>,
    error: Option<SharedString>,
    busy: bool,
    _load: Task<()>,
}

impl RemotesModal {
    pub(crate) fn new(
        repository: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Name, e.g. origin or upstream", window, cx);
            editor
        });
        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("URL, e.g. https://github.com/owner/repo.git", window, cx);
            editor
        });
        let mut this = Self {
            repository,
            remotes: Vec::new(),
            loaded: false,
            editing: None,
            name_editor,
            url_editor,
            error: None,
            busy: false,
            _load: Task::ready(()),
        };
        this.reload(window, cx);
        this
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let urls = self
            .repository
            .update(cx, |repository, _| repository.remote_urls());
        self._load = cx.spawn_in(window, async move |this, cx| {
            let urls = urls.await;
            this.update_in(cx, |this, window, cx| {
                match urls {
                    Ok(Ok(urls)) => {
                        let mut remotes: Vec<(SharedString, SharedString)> = urls
                            .into_iter()
                            .map(|(name, url)| (name.into(), url.into()))
                            .collect();
                        remotes.sort_by(|(a, _), (b, _)| {
                            (a.as_ref() != DEFAULT_REMOTE)
                                .cmp(&(b.as_ref() != DEFAULT_REMOTE))
                                .then_with(|| a.cmp(b))
                        });
                        this.remotes = remotes;
                    }
                    Ok(Err(error)) => this.error = Some(format!("{error:#}").into()),
                    Err(_) => this.error = Some("Reading the remotes was canceled".into()),
                }
                this.loaded = true;
                // Without an origin yet, the form offers to add it.
                if this.editing.is_none()
                    && this.name_editor.read(cx).text(cx).is_empty()
                    && !this.has_remote(DEFAULT_REMOTE)
                {
                    this.name_editor.update(cx, |editor, cx| {
                        editor.set_text(DEFAULT_REMOTE, window, cx);
                    });
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn has_remote(&self, name: &str) -> bool {
        self.remotes.iter().any(|(remote, _)| remote.as_ref() == name)
    }

    fn edit(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let url = self
            .remotes
            .iter()
            .find(|(remote, _)| *remote == name)
            .map(|(_, url)| url.clone())
            .unwrap_or_default();
        self.name_editor.update(cx, |editor, cx| {
            editor.set_text(name.as_ref(), window, cx);
        });
        self.url_editor.update(cx, |editor, cx| {
            editor.set_text(url.as_ref(), window, cx);
        });
        self.editing = Some(name);
        self.error = None;
        window.focus(&self.url_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn reset_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.error = None;
        for editor in [&self.name_editor, &self.url_editor] {
            editor.update(cx, |editor, cx| editor.set_text("", window, cx));
        }
        window.focus(&self.name_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.reset_form(window, cx);
        } else {
            cx.emit(DismissEvent);
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx);
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let name = self.name_editor.read(cx).text(cx).trim().to_string();
        let url = self.url_editor.read(cx).text(cx).trim().to_string();
        if let Err(error) = self.validate(&name, &url) {
            self.error = Some(error.into());
            cx.notify();
            return;
        }

        let repository = self.repository.clone();
        let operations: Vec<oneshot::Receiver<Result<()>>> = match self.editing.clone() {
            Some(old_name) => {
                let old_url = self
                    .remotes
                    .iter()
                    .find(|(remote, _)| *remote == old_name)
                    .map(|(_, url)| url.to_string());
                repository.update(cx, |repository, _| {
                    let mut operations = Vec::new();
                    if old_url.as_deref() != Some(url.as_str()) {
                        operations.push(repository.set_remote_url(old_name.to_string(), url));
                    }
                    if old_name.as_ref() != name {
                        operations.push(repository.rename_remote(old_name.to_string(), name));
                    }
                    operations
                })
            }
            None => vec![repository.update(cx, |repository, _| repository.create_remote(name, url))],
        };
        self.run(operations, true, window, cx);
    }

    fn validate(&self, name: &str, url: &str) -> Result<(), &'static str> {
        if name.is_empty() {
            return Err("Enter a name for the remote");
        }
        if name.chars().any(char::is_whitespace) {
            return Err("A remote name can't contain spaces");
        }
        if url.is_empty() {
            return Err("Enter the remote's URL");
        }
        let renamed_onto_other = match &self.editing {
            Some(old_name) => old_name.as_ref() != name && self.has_remote(name),
            None => self.has_remote(name),
        };
        if renamed_onto_other {
            return Err("A remote with this name already exists");
        }
        Ok(())
    }

    fn remove(&mut self, name: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remove the remote {name}?"),
            Some(
                "Its remote-tracking branches are deleted, and branches that track it lose their upstream. Nothing on the remote itself changes.",
            ),
            &["Remove", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                let operation = this
                    .repository
                    .update(cx, |repository, _| repository.remove_remote(name.to_string()));
                let was_editing = this.editing.as_ref() == Some(&name);
                this.run(vec![operation], was_editing, window, cx);
            })
            .log_err();
        })
        .detach();
    }

    /// Runs `operations` one after another, stopping at the first failure,
    /// then reads the remotes again.
    fn run(
        &mut self,
        operations: Vec<oneshot::Receiver<Result<()>>>,
        reset_form: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.busy = true;
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let mut result = Ok(());
            for operation in operations {
                result = match operation.await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("The operation was canceled")),
                };
                if result.is_err() {
                    break;
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(()) if reset_form => this.reset_form(window, cx),
                    Ok(()) => {}
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                this.reload(window, cx);
            })
            .log_err();
        })
        .detach();
    }

    fn render_remote(
        &self,
        ix: usize,
        name: &SharedString,
        url: &SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_editing = self.editing.as_ref() == Some(name);
        h_flex()
            .id(("remote-row", ix))
            .px_3()
            .py_1()
            .gap_2()
            .w_full()
            .when(is_editing, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .child(
                div()
                    .flex_shrink_0()
                    .child(Label::new(name.clone()).size(LabelSize::Small)),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(url.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .child(
                IconButton::new(("remote-edit", ix), IconName::Pencil)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Edit the name or URL"))
                    .disabled(self.busy)
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |this, _, window, cx| this.edit(name.clone(), window, cx)
                    })),
            )
            .child(
                IconButton::new(("remote-remove", ix), IconName::Trash)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Remove this remote"))
                    .disabled(self.busy)
                    .on_click(cx.listener({
                        let name = name.clone();
                        move |this, _, window, cx| this.remove(name.clone(), window, cx)
                    })),
            )
    }

    fn render_field(
        &self,
        label: &'static str,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(
                Label::new(label)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_1()
                    .py_0p5()
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_sm()
                    .child(editor.clone()),
            )
    }
}

impl EventEmitter<DismissEvent> for RemotesModal {}
impl ModalView for RemotesModal {}

impl Focusable for RemotesModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl Render for RemotesModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus_handle = self.name_editor.focus_handle(cx);
        let rows: Vec<_> = self
            .remotes
            .iter()
            .enumerate()
            .map(|(ix, (name, url))| self.render_remote(ix, name, url, cx).into_any_element())
            .collect();
        let form_title = match &self.editing {
            Some(name) => format!("Edit {name}"),
            None => "Add a Remote".to_string(),
        };

        v_flex()
            .key_context("GitRemotesModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(40.))
            .pb_3()
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::Server).size(IconSize::XSmall))
                    .child(Headline::new("Remotes").size(HeadlineSize::XSmall)),
            )
            .child(
                v_flex()
                    .w_full()
                    .when(self.loaded && rows.is_empty(), |this| {
                        this.child(
                            div().px_3().py_1().child(
                                Label::new("This repository has no remotes yet.")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                        )
                    })
                    .children(rows),
            )
            .child(
                v_flex()
                    .mx_3()
                    .mt_2()
                    .pt_2()
                    .gap_1p5()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Label::new(form_title)
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                    .child(self.render_field("Name", &self.name_editor, cx))
                    .child(self.render_field("URL", &self.url_editor, cx))
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            Label::new(error)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    })
                    .child(
                        h_flex()
                            .pt_1()
                            .gap_1()
                            .justify_end()
                            .when(self.editing.is_some(), |this| {
                                this.child(
                                    Button::new("remote-form-cancel", "Cancel")
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reset_form(window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new(
                                    "remote-form-save",
                                    if self.editing.is_some() {
                                        "Save"
                                    } else {
                                        "Add Remote"
                                    },
                                )
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Filled)
                                .disabled(self.busy)
                                .key_binding(ui::KeyBinding::for_action_in(
                                    &Confirm,
                                    &focus_handle,
                                    cx,
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save(window, cx);
                                })),
                            ),
                    ),
            )
    }
}
