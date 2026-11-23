//! A thread's row in the threads view: its project and marks, its title, and
//! what the thread does before its harness and the model it runs. The desktop
//! list and the phone's draw it from one [`RowShows`] and one
//! [`SessionsSidebar::row_lines`], so the two say the same.

use super::*;

/// A desktop row's height, whatever it shows.
pub(super) const DESKTOP_ROW_HEIGHT: f32 = 74.;
/// Between one desktop row and the next.
pub(super) const DESKTOP_ROW_GAP: f32 = 6.;
const DESKTOP_ROW_RADIUS: f32 = 12.;
/// A phone row's height, whatever it shows.
pub(super) const PHONE_ROW_HEIGHT: f32 = 84.;
/// A phone row's inset from the page's edges.
pub(super) const PHONE_ROW_INSET: f32 = 20.;
/// The fill a pinned row rests on, of the muted foreground.
const PINNED_FILL: f32 = 0.05;
/// Between a row's status and the harness's glyph after it.
const STATUS_GAP: gpui::Pixels = px(8.);
/// How far a settled row's project artwork is greyed.
const SETTLED_ARTWORK: f32 = 0.5;

/// The sizes of a row's three lines. The desktop row's type falls between
/// gpui-base's `xs` (12/16) and `sm` (14/20), steps its scale does not have;
/// the phone's is a step up, on gpui-base's own.
struct Scale {
    small: (gpui::Pixels, gpui::Pixels),
    title: (gpui::Pixels, gpui::Pixels),
    /// The first line's height, which its hover controls fill.
    first_line: gpui::Pixels,
    artwork: f32,
    glyph: gpui::Pixels,
    gap: gpui::Pixels,
}

impl Scale {
    fn of(compact: bool, cx: &App) -> Self {
        if compact {
            let typography = &cx.theme().tokens.typography;
            Self {
                small: (typography.xs.size, typography.xs.line_height),
                title: (typography.sm.size, typography.sm.line_height),
                first_line: px(22.),
                artwork: 18.,
                glyph: px(14.),
                gap: px(4.),
            }
        } else {
            Self {
                small: (px(11.), px(14.)),
                title: (px(13.), px(18.)),
                first_line: px(20.),
                artwork: 16.,
                glyph: px(12.),
                gap: px(3.),
            }
        }
    }
}

/// What a row's third line says its thread does, before the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StatusKind {
    Approval,
    Answer,
    Working,
    Failed,
    Background,
    /// Nothing of it runs, and something will wake it.
    Waiting,
    Draft,
}

impl StatusKind {
    fn verb(self) -> Cow<'static, str> {
        match self {
            StatusKind::Approval => crate::tr!("sidebar.waiting_approval"),
            StatusKind::Answer => crate::tr!("sidebar.waiting_input"),
            StatusKind::Working => crate::tr!("sidebar.working"),
            StatusKind::Failed => crate::tr!("sidebar.failed"),
            StatusKind::Background => crate::tr!("sidebar.background"),
            StatusKind::Waiting => crate::tr!("sidebar.waiting"),
            StatusKind::Draft => crate::tr!("sidebar.draft"),
        }
    }

    /// Amber waits on the user, so the lamp alone tells it from work.
    fn color(self, cx: &App) -> gpui::Hsla {
        match self {
            StatusKind::Approval | StatusKind::Answer => cx.theme().warning,
            StatusKind::Working => cx.theme().primary,
            StatusKind::Failed => cx.theme().danger,
            StatusKind::Background | StatusKind::Waiting | StatusKind::Draft => {
                cx.theme().muted_foreground
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RowStatus {
    pub(super) kind: StatusKind,
    /// Who waits on the user, after the verb.
    pub(super) name: Option<SharedString>,
}

impl RowStatus {
    /// What a thread's row says it does; `None` for a thread at rest.
    pub(super) fn of(state: &ThreadRowState, working: bool) -> Option<Self> {
        let status = |kind| Some(Self { kind, name: None });
        if state.draft {
            return state.draft_kept.then_some(Self {
                kind: StatusKind::Draft,
                name: None,
            });
        }
        if let Some((kind, name)) = &state.native_asking {
            return Some(Self {
                kind: *kind,
                name: Some(name.clone()),
            });
        }
        if state.waiting() {
            return Some(Self {
                kind: Self::waiting_kind(state.waiting_for_approval),
                name: None,
            });
        }
        if let Some(agent) = &state.waiting_agent {
            return Some(Self {
                kind: Self::waiting_kind(agent.approval),
                name: Some(agent.title.clone()),
            });
        }
        if state.failed {
            status(StatusKind::Failed)
        } else if working {
            status(StatusKind::Working)
        } else if state.background != (0, 0) {
            status(StatusKind::Background)
        } else if !state.wakes.is_empty() {
            status(StatusKind::Waiting)
        } else {
            None
        }
    }

    /// What a thread with a turn running or a request open says of itself:
    /// the request it waits on the user for, else that it works.
    pub(super) fn busy(state: &ThreadRowState) -> Self {
        Self {
            kind: if state.waiting() {
                Self::waiting_kind(state.waiting_for_approval)
            } else {
                StatusKind::Working
            },
            name: None,
        }
    }

    fn waiting_kind(approval: bool) -> StatusKind {
        if approval {
            StatusKind::Approval
        } else {
            StatusKind::Answer
        }
    }

    pub(super) fn verb(&self) -> Cow<'static, str> {
        self.kind.verb()
    }

    pub(super) fn color(&self, cx: &App) -> gpui::Hsla {
        self.kind.color(cx)
    }

    /// Only background work keeps the thread going.
    pub(super) fn breathes(&self) -> bool {
        self.kind == StatusKind::Background
    }

    fn spoken(&self) -> String {
        match &self.name {
            Some(name) => format!("{} · {name}", self.verb()),
            None => self.verb().into_owned(),
        }
    }
}

/// Which of the harness's own subagents waits on the user through
/// `session_id`, and for what: the first asking for an approval, else the
/// first asking a question, named by its agent type.
pub(super) fn native_subagent_asking(
    store: &WorkspaceStore,
    session_id: &str,
) -> Option<(StatusKind, SharedString)> {
    let asking = store.asking(session_id);
    let named = |kind: tcode_protocol::AskingKind| {
        asking
            .iter()
            .find(|asker| asker.kind == kind)
            .map(|asker| SharedString::from(asker.agent_type.clone()))
    };
    named(tcode_protocol::AskingKind::Approval)
        .map(|name| (StatusKind::Approval, name))
        .or_else(|| {
            named(tcode_protocol::AskingKind::Question).map(|name| (StatusKind::Answer, name))
        })
}

/// What a thread's row in the threads view shows.
#[derive(Clone)]
pub(super) struct RowShows {
    pub(super) title: SharedString,
    pub(super) unread: bool,
    pub(super) settled: bool,
    pub(super) pinned: bool,
    pub(super) worktree: bool,
    pub(super) project: Option<SharedString>,
    /// Its host, over several.
    pub(super) host: Option<SharedString>,
    pub(super) harness: (agent::ProviderKind, gpui::Hsla),
    /// What its thread does, unless it is at rest.
    pub(super) status: Option<RowStatus>,
    /// The model the thread runs, by the name the host's list gives it.
    pub(super) model: SharedString,
}

impl RowShows {
    pub(super) fn of(
        store: &WorkspaceStore,
        meta: &SessionMeta,
        state: &ThreadRowState,
        working: bool,
        project: Option<SharedString>,
        host: Option<SharedString>,
    ) -> Self {
        let settled = meta.is_settled();
        // With no model chosen, the harness runs its own default: name the harness.
        let model = match meta.model.as_deref() {
            Some(model) => store
                .thread_model_name(&meta.id, model)
                .unwrap_or_else(|| model.to_owned())
                .into(),
            None => meta.provider.display_name().into(),
        };
        Self {
            title: meta.title.clone().into(),
            unread: state.show_unread,
            settled,
            pinned: meta.pinned_at.is_some() && !settled,
            worktree: meta.worktree.is_some(),
            project,
            host,
            harness: (meta.provider, gpui::rgb(store.provider_color(meta)).into()),
            status: RowStatus::of(state, working),
            model,
        }
    }

    /// What the row's accessible label says after the thread's title: its
    /// project, what it does, its model, and that it is pinned.
    pub(super) fn details(&self) -> Vec<String> {
        let mut details: Vec<String> = self.project.iter().map(|p| p.to_string()).collect();
        details.extend(self.status.as_ref().map(RowStatus::spoken));
        details.push(self.model.to_string());
        if self.pinned {
            details.push(crate::tr!("sidebar.pinned").into_owned());
        }
        details
    }
}

/// The fill a row rests on, the selected fill gliding to and from it.
pub(super) fn row_fill(light: RowLight, pinned: bool, cx: &App) -> gpui::Hsla {
    let active = cx.theme().list_active;
    match light {
        RowLight::Current => active,
        RowLight::Shown => active.opacity(SHOWN_FILL),
        RowLight::None if pinned => cx.theme().muted_foreground.opacity(PINNED_FILL),
        RowLight::None => active.opacity(0.),
    }
}

/// The surface of a desktop row.
pub(super) fn desktop_row_surface(selected: bool, cx: &App) -> gpui::Div {
    crate::material::selectable_row_shaped(
        v_flex(),
        DESKTOP_ROW_HEIGHT,
        px(DESKTOP_ROW_RADIUS),
        selected,
        cx,
    )
}

/// A status's dot, breathing while its work goes on in the background.
pub(super) fn lamp(color: gpui::Hsla, breathes: bool) -> gpui::AnyElement {
    let diameter = px(6.);
    if breathes {
        crate::widgets::BreathingDot::new(color)
            .diameter(diameter)
            .into_any_element()
    } else {
        div()
            .size(diameter)
            .rounded_full()
            .bg(color)
            .into_any_element()
    }
}

fn sized_text<E: gpui::Styled>(el: E, (size, line): (gpui::Pixels, gpui::Pixels)) -> E {
    el.text_size(size).line_height(line)
}

impl SessionsSidebar {
    /// The three lines of `state`'s row as `shows` has them, `time` at the
    /// first line's end. On the desktop its marks offer what they mark on
    /// hover; on the phone they are only shown.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn row_lines(
        &self,
        store: &Entity<WorkspaceStore>,
        meta: &SessionMeta,
        state: &ThreadRowState,
        shows: &RowShows,
        time: gpui::AnyElement,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let scale = Scale::of(compact, cx);
        let name = &state.name;
        let muted = cx.theme().muted_foreground;
        let renaming = state.renaming.is_some();

        let artwork = shows.project.as_ref().map(|_| {
            let store = store.read(cx);
            store
                .project(meta.project_id.as_deref().unwrap_or_default())
                .map(|project| {
                    crate::project_icon::artwork(store.image_host(), project, scale.artwork)
                })
                .unwrap_or_else(|| {
                    Icon::new(IconName::Folder)
                        .size(px(scale.artwork))
                        .into_any_element()
                })
        });
        let pin = if compact {
            crate::motion::shown(
                SharedString::from(format!("compact-pin-{name}")),
                shows.pinned.then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                shown.fold_across_gap(
                    scale.gap,
                    Icon::new(IconName::Pin).flex_none().text_color(muted),
                )
            })
        } else {
            self.pin_glyph(meta, state, window, cx)
                .map(IntoElement::into_any_element)
        };
        let line_one = sized_text(h_flex(), scale.small)
            .w_full()
            .min_w_0()
            .h(scale.first_line)
            .items_center()
            .gap(scale.gap)
            .text_color(muted)
            .children(artwork.map(|artwork| {
                div()
                    .flex_none()
                    .when(shows.settled, |artwork| artwork.opacity(SETTLED_ARTWORK))
                    .child(artwork)
            }))
            .children(shows.project.clone().map(|project| {
                div()
                    .flex_initial()
                    .min_w_0()
                    .overflow_hidden()
                    .when(compact, |label| {
                        let project = project.clone();
                        label.debug_selector(move || format!("compact-project-{project}"))
                    })
                    .child(crate::widgets::fade_text(project))
            }))
            .children(shows.host.clone().map(|host| {
                div()
                    .debug_selector({
                        let name = name.clone();
                        move || format!("flat-host-{name}")
                    })
                    .min_w_0()
                    // The host gives up its width before the project.
                    .flex_shrink(1000.)
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(host))
            }))
            .child(div().flex_1())
            .children(Self::follow_up_glyph(meta, name, window, cx))
            .children(if compact {
                crate::pull_requests::badge(&meta.pull_requests, true, cx)
            } else {
                Self::pr_badge(
                    store,
                    meta,
                    name,
                    !renaming,
                    scale.gap,
                    "pr-badge-line",
                    true,
                    window,
                    cx,
                )
            })
            .when(shows.worktree, |line| {
                line.child(
                    Icon::empty()
                        .path("icons/git-branch.svg")
                        .flex_none()
                        .text_color(muted),
                )
            })
            .children(pin)
            .when(!compact, |line| {
                line.children(self.pane_glyph(state, window, cx))
            })
            .child(time);

        let line_two = if let Some(input) = &state.renaming {
            self.rename_input(input, cx).into_any_element()
        } else {
            sized_text(div(), scale.title)
                .w_full()
                .min_w_0()
                .truncate()
                .font_medium()
                .when(shows.unread, |title| title.font_semibold())
                .text_color(if shows.settled {
                    muted
                } else if compact {
                    cx.theme().foreground
                } else {
                    cx.theme().sidebar_foreground
                })
                .debug_selector({
                    let name = name.clone();
                    let list = if compact { "compact" } else { "flat" };
                    move || format!("{list}-title-{name}")
                })
                .child(shows.title.clone())
                .into_any_element()
        };

        let (provider, color) = shows.harness;
        let keeps = keeps_going(state);
        let failed = state.failed && !state.waiting();
        let status = shows.status.as_ref();
        let lamp = folded_status(
            format!("row-lamp-{name}"),
            status
                .map(|status| (status.color(cx), status.breathes()))
                .or_else(|| shows.unread.then(|| (cx.theme().primary, false))),
            div().flex_none().debug_selector({
                let name = name.clone();
                move || format!("row-lamp-{name}")
            }),
            window,
            cx,
            |(color, breathes), _, _| {
                div()
                    .pr(px(5.))
                    .child(lamp(*color, *breathes))
                    .into_any_element()
            },
        );
        let verb = crate::motion::shown(
            SharedString::from(format!("row-verb-{name}")),
            status.map(|status| (status.kind, status.name.is_some())),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            let glide = crate::motion::GlideState::keyed(
                SharedString::from(format!("row-verb-width-{name}")),
                window,
                cx,
            );
            let word = crate::motion::cross_fade(
                SharedString::from(format!("row-verb-swap-{name}")),
                shown.value,
                CHANGE,
                window,
                cx,
                |(kind, named), _, cx| {
                    div()
                        .whitespace_nowrap()
                        .text_color(kind.color(cx))
                        .pr(if *named { px(4.) } else { STATUS_GAP })
                        .child(kind.verb())
                        .into_any_element()
                },
            );
            // One verb's width glides to the next's, and what follows with it.
            let key = u64::from(shown.value.1) << 8 | shown.value.0 as u64;
            let word = crate::motion::SizeGlide::new(&glide, word)
                .axes(crate::motion::GlideAxes::Width)
                .on_change(key);
            shown.fold_across(
                div()
                    .flex_none()
                    .debug_selector({
                        let name = name.clone();
                        move || format!("row-status-verb-{name}")
                    })
                    .child(word),
            )
        });
        let who = folded_status(
            format!("row-who-{name}"),
            status.and_then(|status| Some((status.name.clone()?, status.color(cx)))),
            // Who waits gives up its width before the model.
            div().min_w_0().flex_shrink(1000.).overflow_hidden(),
            window,
            cx,
            |(who, color), _, _| {
                h_flex()
                    .min_w_0()
                    .gap(px(4.))
                    .pr(STATUS_GAP)
                    .text_color(*color)
                    .child(div().flex_none().child("·"))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .child(crate::widgets::fade_text(who.clone())),
                    )
                    .into_any_element()
            },
        );
        let model = crate::motion::cross_fade(
            SharedString::from(format!("row-model-{name}")),
            shows.model.clone(),
            CHANGE,
            window,
            cx,
            |model, _, _| {
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .child(crate::widgets::fade_text(model.clone()))
                    .into_any_element()
            },
        );
        let spinner = crate::motion::shown(
            SharedString::from(format!("title-spinner-{}", state.row_key)),
            state.title_generating.then_some(()),
            SHOW,
            window,
            cx,
        )
        .map(|shown| {
            shown.fold_across_gap(px(4.), Spinner::new().with_size(scale.glyph).color(muted))
        });
        let line_three = sized_text(h_flex(), scale.small)
            .id(SharedString::from(format!("thread-status-{name}")))
            .w_full()
            .min_w_0()
            .items_center()
            .when(status.is_some(), |line| {
                line.when_some(keeps, |line, keeps| {
                    line.with_tooltip(move |_, _| Tooltip::new(keeps.clone()))
                })
                .when(failed, |line| {
                    line.with_tooltip(|_, _| Tooltip::new(crate::tr!("sidebar.failed_tooltip")))
                })
            })
            .children(lamp)
            .children(verb)
            .children(who)
            .child(
                h_flex()
                    .flex_auto()
                    .min_w_0()
                    .items_center()
                    .gap(px(4.))
                    .text_color(muted)
                    .child(
                        crate::provider_card::provider_glyph(provider)
                            .flex_none()
                            .size(scale.glyph)
                            .text_color(color),
                    )
                    .children(spinner)
                    .child(
                        div()
                            .flex_auto()
                            .min_w_0()
                            .overflow_hidden()
                            .debug_selector({
                                let name = name.clone();
                                move || format!("row-model-{name}")
                            })
                            .child(model),
                    ),
            );
        v_flex()
            .w_full()
            .min_w_0()
            .gap(scale.gap)
            .child(line_one)
            .child(line_two)
            .child(line_three)
    }
}
