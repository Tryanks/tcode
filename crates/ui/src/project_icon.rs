//! Shared project artwork and an in-app picker over the attached host's files.
use crate::{
    icon::{Icon, IconName},
    overlay::OverlayExt as _,
    scroll::ScrollableElement as _,
    sizing::fit_viewport,
    store::{WorkspaceStore, images},
    theme::ActiveTheme as _,
    widgets::{
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
};
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _, Subscription, Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_base::{h_flex, v_flex};
use std::path::PathBuf;
use tcode_core::project::Project;
use tcode_protocol::{PathEntry, QueryResponse};

pub(crate) fn artwork(project: &Project, size: f32) -> impl IntoElement + use<> {
    img(images::project_icon(project, size))
        .size(px(size))
        .flex_none()
        .with_fallback(move || {
            Icon::new(IconName::Folder)
                .size(px(size))
                .into_any_element()
        })
}

pub(crate) fn open(
    store: Entity<WorkspaceStore>,
    project: Project,
    window: &mut Window,
    cx: &mut App,
) {
    let picker = cx.new(|cx| Picker::new(store, project, window, cx));
    picker.update(cx, |picker, cx| {
        picker.browse(picker.project.root.clone(), window, cx)
    });
    window.open_dialog(cx, move |dialog, window, _| {
        let content = picker.clone();
        dialog
            // Enter in a path or filter field must not confirm the dialog.
            .on_ok(|_, _, _| false)
            .title(crate::tr!("project_icon.title"))
            .w(fit_viewport(680., window.viewport_size().width))
            .content(move |el, _, _| el.child(content.clone()))
    });
}

struct Picker {
    store: Entity<WorkspaceStore>,
    project: Project,
    path: Entity<InputState>,
    search: Entity<InputState>,
    directory: PathBuf,
    parent: Option<PathBuf>,
    entries: Vec<PathEntry>,
    selected: Option<PathBuf>,
    loading: bool,
    saving: bool,
    error: Option<String>,
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl Picker {
    fn new(
        store: Entity<WorkspaceStore>,
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = cx.new(|cx| InputState::new(window, cx));
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(crate::tr!("project_icon.search")));
        let subscriptions = vec![
            cx.subscribe_in(&path, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) && !this.saving {
                    this.browse(
                        PathBuf::from(this.path.read(cx).value().as_str()),
                        window,
                        cx,
                    );
                }
            }),
            cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        Self {
            directory: project.root.clone(),
            store,
            project,
            path,
            search,
            parent: None,
            entries: Vec::new(),
            selected: None,
            loading: false,
            saving: false,
            error: None,
            generation: 0,
            _subscriptions: subscriptions,
        }
    }

    fn browse(&mut self, directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        self.selected = None;
        self.entries.clear();
        self.path.update(cx, |input, cx| {
            input.set_value(directory.to_string_lossy().to_string(), window, cx)
        });
        self.search
            .update(cx, |input, cx| input.set_value("", window, cx));
        let task = self
            .store
            .update(cx, |store, cx| store.browse_icon_images(directory, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(QueryResponse::IconImages {
                        directory,
                        parent,
                        entries,
                    }) => {
                        this.path.update(cx, |input, cx| {
                            input.set_value(directory.to_string_lossy().to_string(), window, cx)
                        });
                        this.directory = directory;
                        this.parent = parent;
                        this.entries = entries;
                    }
                    Err(error) => this.error = Some(error.message),
                    _ => this.error = Some(crate::tr!("project_icon.load_failed").into_owned()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save(&mut self, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.saving = true;
        self.error = None;
        let reset = path.is_none();
        let task = self.store.update(cx, |store, cx| {
            store.set_project_icon(self.project.id.clone(), path, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(_) => {
                        if reset {
                            let mut default_project = this.project.clone();
                            default_project.icon_path = None;
                            images::invalidate_project_icon(&default_project, cx);
                        }
                        window.close_dialog(cx);
                    }
                    Err(error) => this.error = Some(error.message),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for Picker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.search.read(cx).value().to_lowercase();
        let selected = self.selected.clone();
        let busy = self.saving;
        let parent = self.parent.clone();
        let root = self.project.root.clone();
        let host = self
            .store
            .read(cx)
            .remote_host_name()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::tr!("project_icon.this_machine").into_owned());
        let mut grid = h_flex().flex_wrap().gap_2().items_start();
        let mut count = 0;
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.basename.to_lowercase().contains(&search))
        {
            count += 1;
            let path = self.directory.join(&entry.basename);
            let is_dir = entry.is_dir;
            let is_selected = selected.as_ref() == Some(&path);
            let content = if is_dir {
                Icon::new(IconName::Folder)
                    .size(px(32.))
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element()
            } else {
                img(images::icon_thumbnail(path.clone()))
                    .size(px(64.))
                    .with_fallback(|| {
                        Icon::empty()
                            .path("icons/image.svg")
                            .size(px(28.))
                            .into_any_element()
                    })
                    .into_any_element()
            };
            grid = grid.child(
                crate::material::accessible_clickable(
                    v_flex(),
                    SharedString::from(format!("icon-file-{}", entry.basename)),
                    Role::Button,
                    entry.basename.clone(),
                    cx,
                )
                .debug_selector({
                    let name = entry.basename.clone();
                    move || format!("icon-file-{name}")
                })
                .w(px(96.))
                .h(px(108.))
                .p_2()
                .gap_1()
                .items_center()
                .justify_center()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(if is_selected {
                    cx.theme().primary
                } else {
                    cx.theme().border
                })
                .bg(if is_selected {
                    cx.theme().accent
                } else {
                    cx.theme().background
                })
                .hover(|el| el.bg(cx.theme().accent))
                .cursor_pointer()
                .child(
                    div()
                        .h(px(68.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(content),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(11.))
                        .child(entry.basename.clone()),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.saving {
                        return;
                    }
                    if is_dir {
                        this.browse(path.clone(), window, cx);
                    } else {
                        this.selected = Some(path.clone());
                        this.error = None;
                        cx.notify();
                    }
                })),
            );
        }
        let viewport_height =
            (f32::from(window.viewport_size().height) * 0.9 - 360.).clamp(48., 320.);
        let content = if self.loading {
            div()
                .p_4()
                .child(crate::tr!("project_icon.loading"))
                .into_any_element()
        } else if count == 0 {
            div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(crate::tr!("project_icon.empty"))
                .into_any_element()
        } else {
            grid.into_any_element()
        };
        v_flex()
            .id("project-icon-picker")
            .debug_selector(|| "project-icon-picker".into())
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(artwork(&self.project, 32.))
                    .child(
                        v_flex().child(self.project.name.clone()).child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(host),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("icon-project-root")
                            .ghost()
                            .icon(IconName::Folder)
                            .tooltip(crate::tr!("project_icon.project_folder"))
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.browse(root.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new("icon-parent")
                            .ghost()
                            .icon(IconName::ArrowUp)
                            .tooltip(crate::tr!("project_icon.parent"))
                            .disabled(parent.is_none() || busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(parent) = &parent {
                                    this.browse(parent.clone(), window, cx);
                                }
                            })),
                    )
                    .child(
                        div()
                            .id("icon-path")
                            .debug_selector(|| "icon-path".into())
                            .flex_1()
                            .min_w_0()
                            .child(
                                Input::new(&self.path)
                                    .aria_label(crate::tr!("project_icon.folder"))
                                    .disabled(busy),
                            ),
                    )
                    .child(
                        Button::new("icon-go")
                            .label(crate::tr!("project_icon.go"))
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.browse(
                                    PathBuf::from(this.path.read(cx).value().as_str()),
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .child(Input::new(&self.search).aria_label(crate::tr!("project_icon.search")))
            .child(
                div()
                    .id("icon-file-grid")
                    .h(px(viewport_height))
                    .overflow_y_scrollbar()
                    .child(content),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        selected
                            .as_ref()
                            .and_then(|path| path.file_name())
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| crate::tr!("project_icon.hint").into_owned()),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .justify_between()
                    .child(
                        Button::new("icon-reset")
                            .ghost()
                            .label(crate::tr!("project_icon.default"))
                            .disabled(busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save(None, window, cx)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("icon-cancel")
                                    .label(crate::tr!("sidebar.cancel"))
                                    .disabled(busy)
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            )
                            .child(
                                Button::new("icon-save")
                                    .primary()
                                    .label(if busy {
                                        crate::tr!("project_icon.saving")
                                    } else {
                                        crate::tr!("project_icon.choose")
                                    })
                                    .disabled(selected.is_none() || busy)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(path) = &selected {
                                            this.save(Some(path.clone()), window, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, size};
    use tcode_runtime::pipe::{HostServices, spawn_host};
    use tcode_services::store::SessionStore;

    struct EmptyView;
    impl Render for EmptyView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn entering_a_folder_keeps_the_picker_open_and_lists_only_images(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        let root = std::env::temp_dir().join(format!(
            "tcode-icon-picker-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let folder = root.join("pictures");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("logo.png"),
            include_bytes!("../../../assets/icons/app/tcode.png"),
        )
        .unwrap();
        std::fs::write(folder.join("notes.txt"), "not an image").unwrap();
        let project = Project::from_root(root.clone());
        let disk = SessionStore::open_at(root.join("data")).unwrap();
        disk.upsert_project(&project).unwrap();
        let host = spawn_host(disk, HostServices::default()).unwrap();
        let store = cx.new(|cx| WorkspaceStore::new(host.link(), cx));
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| EmptyView);
            crate::overlay::OverlayHost::new(view, window, cx)
        });
        cx.simulate_resize(size(px(390.), px(700.)));
        cx.update(|window, cx| open(store.clone(), project, window, cx));
        draw(cx);
        let path = cx.debug_bounds("icon-path").unwrap();
        cx.simulate_click(path.center(), gpui::Modifiers::default());
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input(folder.to_str().unwrap());
        cx.simulate_keystrokes("enter");
        draw(cx);
        assert!(
            cx.debug_bounds("project-icon-picker").is_some(),
            "Enter dismissed the picker"
        );
        assert!(cx.debug_bounds("icon-file-logo.png").is_some());
        assert!(cx.debug_bounds("icon-file-notes.txt").is_none());
        cx.update(|window, cx| window.close_dialog(cx));
        host.shutdown_blocking().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
