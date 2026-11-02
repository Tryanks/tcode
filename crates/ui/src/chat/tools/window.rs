//! The grammar every activity window shares: a one-line header — what the
//! call is, what it acts on, how it stands — over a body that folds open in
//! place.

use std::ops::Range;

use agent::ItemStatus;
use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, HighlightStyle, InteractiveElement as _,
    IntoElement, ParentElement as _, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use crate::icon::{Icon, IconName, IconNamed};
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};
use crate::widgets::spinner::Spinner;

pub(in crate::chat) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A verb in the three forms a call's state reads in: going on ("Reading"),
/// done ("Read"), and asked for but not carried out ("Read", declined).
/// Each names a locale key under `tools.verb`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::chat) struct Verb(pub(in crate::chat) &'static str);

impl Verb {
    pub(in crate::chat) fn text(self, status: ItemStatus) -> SharedString {
        let form = match status {
            ItemStatus::InProgress => "running",
            ItemStatus::Completed | ItemStatus::Failed | ItemStatus::Interrupted => "done",
            ItemStatus::Declined => "asked",
        };
        crate::tr!(&format!("tools.verb.{}.{form}", self.0))
            .into_owned()
            .into()
    }
}

/// What the header says the call acted on.
#[derive(Clone, Default)]
pub(in crate::chat) struct Argument {
    pub(in crate::chat) text: String,
    pub(in crate::chat) mono: bool,
    /// Ranges painted faint: the marks that stand for line breaks.
    pub(in crate::chat) markers: Vec<Range<usize>>,
    /// Muted context after the argument: where a search looked.
    pub(in crate::chat) detail: Option<String>,
}

impl Argument {
    pub(in crate::chat) fn plain(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        (!text.is_empty()).then(|| Self {
            text,
            ..Self::default()
        })
    }

    pub(in crate::chat) fn mono(text: impl Into<String>) -> Option<Self> {
        Self::plain(text).map(|argument| Self {
            mono: true,
            ..argument
        })
    }

    pub(in crate::chat) fn detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail.filter(|detail| !detail.is_empty());
        self
    }
}

/// A fact about the result, beside the argument: a count, an exit code.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::chat) enum Chip {
    Count(String),
    Lines {
        added: u32,
        deleted: u32,
    },
    Exit(i32),
    /// An outcome the call itself reports short of success, in its words: a
    /// wait that timed out, an action that did not work.
    Warning(String),
}

/// The header of one window.
pub(in crate::chat) struct Header {
    /// The type icon's asset path.
    pub(in crate::chat) icon: SharedString,
    pub(in crate::chat) verb: Option<Verb>,
    /// The verb shimmers while the call runs: a live thought.
    pub(in crate::chat) shimmer: bool,
    /// A name the verb applies to, in the verb's weight: a tool the window
    /// knows only by name.
    pub(in crate::chat) subject: Option<SharedString>,
    /// The call's own words for what it does, verbatim, in the verb's weight
    /// and in its place: the title a computer-use script was given.
    pub(in crate::chat) title: Option<SharedString>,
    pub(in crate::chat) argument: Option<Argument>,
    pub(in crate::chat) chips: Vec<Chip>,
    /// The line a failure is told by, verbatim. `None` takes the output's
    /// first line.
    pub(in crate::chat) error: Option<String>,
    /// How long the call took, as its harness reports it.
    pub(in crate::chat) elapsed_ms: Option<u64>,
}

impl Header {
    pub(in crate::chat) fn new(icon: impl IconNamed, verb: Verb) -> Self {
        Self::with_icon(icon.path(), verb)
    }

    pub(in crate::chat) fn with_icon(icon: impl Into<SharedString>, verb: Verb) -> Self {
        Self {
            icon: icon.into(),
            verb: Some(verb),
            shimmer: false,
            subject: None,
            title: None,
            argument: None,
            chips: Vec::new(),
            error: None,
            elapsed_ms: None,
        }
    }

    /// A header with no verb: an item whose kind is all the row knows.
    pub(in crate::chat) fn bare(icon: impl IconNamed) -> Self {
        Self {
            verb: None,
            ..Self::new(icon, Verb("call"))
        }
    }

    pub(in crate::chat) fn shimmer(mut self) -> Self {
        self.shimmer = true;
        self
    }

    pub(in crate::chat) fn subject(mut self, subject: impl Into<SharedString>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// The call's own title, which stands for the verb.
    pub(in crate::chat) fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.verb = None;
        self.title = Some(title.into());
        self
    }

    pub(in crate::chat) fn argument(mut self, argument: Option<Argument>) -> Self {
        self.argument = argument;
        self
    }

    pub(in crate::chat) fn chip(mut self, chip: Option<Chip>) -> Self {
        self.chips.extend(chip);
        self
    }

    pub(in crate::chat) fn error(mut self, error: Option<String>) -> Self {
        self.error = error;
        self
    }

    /// The header as one line of words, for assistive technology.
    pub(in crate::chat) fn label(&self, status: ItemStatus) -> String {
        let mut words: Vec<String> = Vec::new();
        words.extend(self.verb.map(|verb| verb.text(status).to_string()));
        words.extend(self.subject.as_ref().map(|subject| subject.to_string()));
        words.extend(self.title.as_ref().map(|title| title.to_string()));
        words.extend(self.argument.as_ref().map(|argument| argument.text.clone()));
        words.join(" ")
    }
}

/// The first line of `text` with something on it.
pub(in crate::chat) fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The line a failed call is told by: the header's own, else the first line
/// of what it returned.
pub(in crate::chat) fn failure_line(
    header: &Header,
    status: ItemStatus,
    output: Option<&str>,
) -> Option<String> {
    if status != ItemStatus::Failed {
        return None;
    }
    header.error.clone().or_else(|| output.and_then(first_line))
}

/// The body of a window as its header asks for it.
pub(in crate::chat) enum Detail {
    /// Nothing to open.
    None,
    /// Shut, with something to open.
    Shut,
    /// Open, or folding shut, as the fold samples it; the key names what
    /// the body shows, and the body's height glides when it changes.
    Fold(crate::motion::Shown<()>, AnyElement, u64),
    /// Open, drawn without a fold: a surface that has no window to sample.
    Open(AnyElement),
}

impl Detail {
    pub(in crate::chat) fn expandable(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// The fold of a window's body: `Some` while it is open or still folding
/// shut, when the body has to be built.
pub(in crate::chat) fn fold(
    id: &str,
    open: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<crate::motion::Shown<()>> {
    crate::motion::shown(
        SharedString::from(format!("tool-fold-{id}")),
        open.then_some(()),
        window,
        cx,
    )
}

/// An open body whose height glides when what it shows changes at rest:
/// the whole output replacing the preview, the output arriving, a part of it
/// opening.
fn glide_body(key: &str, shows: u64, body: AnyElement) -> AnyElement {
    let id = SharedString::from(format!("tool-body-glide-{key}"));
    crate::motion::Sampled::new(move |window, cx| {
        let glide = crate::motion::GlideState::keyed(id, window, cx);
        crate::motion::SizeGlide::new(&glide, body)
            .on_change(shows)
            .into_any_element()
    })
    .into_any_element()
}

pub(in crate::chat) struct WindowSpec {
    /// The row's identity in element ids and test selectors: `activity-row-{key}`.
    pub(in crate::chat) key: String,
    pub(in crate::chat) header: Header,
    pub(in crate::chat) status: ItemStatus,
    /// The line a failure is told by.
    pub(in crate::chat) error: Option<String>,
    pub(in crate::chat) elapsed_ms: Option<u64>,
    pub(in crate::chat) compact: bool,
    pub(in crate::chat) copy: Vec<(String, String)>,
    /// A file the call is about, which the menu offers to open, and the
    /// directory a relative one is under.
    pub(in crate::chat) path: Option<(String, std::path::PathBuf)>,
    /// The timeline's hover, which the header's fill follows.
    pub(in crate::chat) hover: crate::motion::ListHover,
}

/// How long an item took, when its start and its end were both seen.
pub(in crate::chat) fn elapsed_of(entry: &tcode_core::session::TimelineEntry) -> Option<u64> {
    Some(entry.finished_ts?.saturating_sub(entry.ts?))
}

/// How long the call took: as its harness reports it, else when its start
/// and its end were both seen.
pub(in crate::chat) fn elapsed(
    header: &Header,
    entry: &tcode_core::session::TimelineEntry,
) -> Option<u64> {
    header.elapsed_ms.or_else(|| elapsed_of(entry))
}

/// A whole window: the header, a click on which toggles the body, then the
/// body in a rail when open. The context menu offers the window's copies.
pub(in crate::chat) fn tool_window(
    spec: WindowSpec,
    detail: Detail,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let WindowSpec {
        key,
        header,
        status,
        error,
        elapsed_ms,
        compact,
        copy,
        path,
        hover,
    } = spec;
    let expandable = detail.expandable();
    let open = match &detail {
        Detail::Fold(shown, ..) => !shown.leaving(),
        Detail::Open(_) => true,
        Detail::None | Detail::Shut => false,
    };
    let label = header.label(status);
    let radius = crate::material::radius_chip(cx);
    let fill =
        expandable.then(|| hover.layer(format!("activity-row-{key}"), cx.theme().accent, radius));
    let row = header_row(
        &key, header, status, elapsed_ms, compact, expandable, open, fill, cx,
    );
    let row: AnyElement = if expandable {
        crate::material::accessible_clickable(
            row,
            SharedString::from(format!("activity-row-{key}")),
            Role::Button,
            label,
            cx,
        )
        .debug_selector({
            let key = key.clone();
            move || format!("activity-row-{key}")
        })
        .aria_expanded(open)
        .rounded(radius)
        .cursor_pointer()
        .on_click(on_toggle)
        .into_any_element()
    } else {
        row.debug_selector({
            let key = key.clone();
            move || format!("activity-row-{key}")
        })
        .into_any_element()
    };

    let body = |body: AnyElement| {
        let key = key.clone();
        crate::material::rail_detail(
            div()
                .w_full()
                .min_w_0()
                .debug_selector(move || format!("tool-detail-{key}"))
                .child(body),
            cx,
        )
        .debug_selector(|| "activity-detail".into())
        .text_token(&cx.theme().tokens.typography.xs)
        .into_any_element()
    };
    // The space above the body is the body's own, so it folds with it and
    // nothing below moves by it as the fold starts or ends.
    let detail = match detail {
        Detail::Fold(shown, content, shows) => Some(
            shown.fold(
                div()
                    .w_full()
                    .pt_1()
                    .child(glide_body(&key, shows, body(content))),
            ),
        ),
        Detail::Open(content) => Some(
            div()
                .w_full()
                .pt_1()
                .child(body(content))
                .into_any_element(),
        ),
        Detail::None | Detail::Shut => None,
    };
    v_flex()
        .id(SharedString::from(format!("tool-window-{key}")))
        .w_full()
        .child(row)
        // An open body tells the failure in full; the line is for a shut one.
        .when_some(error.filter(|_| !open), |window, error| {
            window.child(div().w_full().pt_1().child(error_line(&key, error, cx)))
        })
        .children(detail)
        .context_menu(move |menu, _, _| {
            let menu = match &path {
                Some((path, base)) => {
                    let full = base.join(path);
                    menu.path_items(&full.to_string_lossy(), Some(base))
                        .separator()
                }
                None => menu,
            };
            copy.iter().cloned().fold(menu, |menu, (label, text)| {
                menu.menu(label, Box::new(CopyText(text)))
            })
        })
        .into_any_element()
}

/// The failure, told by its own first line, under the header.
fn error_line(key: &str, error: String, cx: &App) -> AnyElement {
    let key = key.to_owned();
    div()
        .w_full()
        .min_w_0()
        // In line with the verb, past the status glyph.
        .pl(px(30.))
        .overflow_hidden()
        .text_secondary(false, cx)
        .text_color(cx.theme().danger)
        .font_family(cx.theme().mono_font_family.clone())
        .debug_selector(move || format!("tool-error-{key}"))
        .child(crate::widgets::fade_text(error))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn header_row(
    key: &str,
    header: Header,
    status: ItemStatus,
    elapsed_ms: Option<u64>,
    compact: bool,
    expandable: bool,
    open: bool,
    fill: Option<crate::motion::Sampled>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let Header {
        icon,
        verb,
        shimmer,
        subject,
        title,
        argument,
        chips,
        ..
    } = header;
    let mono = theme.mono_font_family.clone();
    let verb = verb.map(|verb| {
        let text = verb.text(status);
        if shimmer && status == ItemStatus::InProgress {
            super::super::components::indicator::shimmer_label(
                SharedString::from(format!("tool-verb-{key}")),
                text.to_string().into(),
                cx,
            )
        } else {
            text.into_any_element()
        }
    });
    h_flex()
        .w_full()
        .min_h(px(crate::material::bar_height(
            crate::material::Bar::Section,
            false,
        )))
        .px_1()
        .gap_2()
        .items_center()
        .when(!compact, |row| row.py_0p5())
        .text_body(false, cx)
        .when_some(fill, |row, fill| row.relative().child(fill))
        .child(status_glyph(key, icon, status, cx))
        .child(
            h_flex()
                .min_w_0()
                .gap_2()
                .overflow_hidden()
                .when_some(verb, |row, verb| {
                    row.child(
                        div()
                            .flex_none()
                            .font_medium()
                            .text_color(theme.foreground)
                            .whitespace_nowrap()
                            .child(verb),
                    )
                })
                .when_some(subject, |row, subject| {
                    row.child(
                        div()
                            .flex_none()
                            .max_w(px(260.))
                            .overflow_hidden()
                            .font_medium()
                            .text_color(theme.foreground)
                            .child(crate::widgets::fade_text(subject)),
                    )
                })
                .when_some(title, |row, title| {
                    let key = key.to_owned();
                    row.child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .font_medium()
                            .text_color(theme.foreground)
                            .debug_selector(move || format!("tool-title-{key}"))
                            .child(crate::widgets::fade_text(title)),
                    )
                })
                .when_some(argument, |row, argument| {
                    let Argument {
                        text,
                        mono: monospace,
                        markers,
                        detail,
                    } = argument;
                    let faint = HighlightStyle {
                        color: Some(muted.opacity(0.45)),
                        ..Default::default()
                    };
                    let highlights = markers.into_iter().map(move |range| (range, faint));
                    let key = key.to_owned();
                    row.child(
                        div()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .when(monospace, |text| text.font_family(mono.clone()))
                            .overflow_hidden()
                            .debug_selector(move || format!("tool-argument-{key}"))
                            .child(crate::widgets::fade_text(text).with_highlights(highlights)),
                    )
                    .when_some(detail, |row, detail| {
                        row.child(
                            // Where it looked, or which lines: short, so the
                            // argument gives way first.
                            div()
                                .flex_none()
                                .max_w(px(320.))
                                .text_secondary(false, cx)
                                .text_color(muted.opacity(0.75))
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(detail)),
                        )
                    })
                }),
        )
        .children(chips.into_iter().map(|chip| chip_element(key, chip, cx)))
        .when_some(status_chip(key, status, cx), |row, chip| row.child(chip))
        .when_some(
            elapsed_ms.filter(|elapsed| *elapsed >= 100),
            |row, elapsed| {
                let key = key.to_owned();
                row.child(
                    div()
                        .flex_none()
                        .text_meta_mono(false, cx)
                        .debug_selector(move || format!("tool-elapsed-{key}"))
                        .child(super::super::model::format_elapsed(elapsed)),
                )
            },
        )
        .when(expandable, |row| {
            row.child(
                crate::widgets::Chevron::new(
                    SharedString::from(format!("tool-chevron-{key}")),
                    open,
                )
                .text_color(muted),
            )
        })
}

fn chip_element(key: &str, chip: Chip, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let mono = theme.mono_font_family.clone();
    let key = key.to_owned();
    let content: AnyElement = match chip {
        Chip::Count(text) => div()
            .text_meta_mono(false, cx)
            .child(text)
            .into_any_element(),
        Chip::Lines { added, deleted } => h_flex()
            .gap_2()
            .text_token(&theme.tokens.typography.xs)
            .font_family(mono)
            .child(div().text_color(theme.success).child(format!("+{added}")))
            .child(div().text_color(theme.danger).child(format!("-{deleted}")))
            .into_any_element(),
        Chip::Exit(code) => crate::material::semantic_chip(
            crate::tr!("tools.exit_code", code = code).into_owned(),
            theme.danger.opacity(0.12),
            theme.danger,
            cx,
        )
        .font_family(mono)
        .into_any_element(),
        Chip::Warning(text) => {
            crate::material::semantic_chip(text, theme.warning.opacity(0.12), theme.warning, cx)
                .into_any_element()
        }
    };
    div()
        .flex_none()
        .debug_selector(move || format!("tool-chip-{key}"))
        .child(content)
        .into_any_element()
}

/// Interrupted and declined calls say so in words: neither failed, and a
/// declined one never ran.
pub(in crate::chat) fn status_chip(key: &str, status: ItemStatus, cx: &App) -> Option<AnyElement> {
    let theme = cx.theme();
    let (label, color) = match status {
        ItemStatus::Interrupted => (crate::tr!("tools.status.interrupted"), theme.warning),
        ItemStatus::Declined => (crate::tr!("tools.status.declined"), theme.muted_foreground),
        ItemStatus::InProgress | ItemStatus::Completed | ItemStatus::Failed => return None,
    };
    Some(
        crate::material::semantic_chip(label.into_owned(), color.opacity(0.12), color, cx)
            .debug_selector({
                let key = key.to_owned();
                move || format!("tool-status-chip-{key}")
            })
            .into_any_element(),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Glyph {
    Running,
    Settled { failed: bool },
}

/// The type icon at row scale, or a spinner while the call runs; one
/// cross-fades into the other. Failure never recolours the glyph — it hangs
/// a small red X off its corner, on a disc of the row background so the
/// badge stays legible over whatever it overlaps.
pub(in crate::chat) fn status_glyph(
    key: &str,
    icon: SharedString,
    status: ItemStatus,
    cx: &App,
) -> AnyElement {
    let glyph = match status {
        ItemStatus::InProgress => Glyph::Running,
        status => Glyph::Settled {
            failed: status == ItemStatus::Failed,
        },
    };
    let id = ElementId::from(SharedString::from(format!("tool-status-{key}")));
    let primary = cx.theme().primary;
    let muted = cx.theme().muted_foreground;
    let background = cx.theme().background;
    let danger = cx.theme().danger;
    crate::motion::Sampled::new(move |window, cx| {
        crate::motion::cross_fade_part(id, glyph, window, cx, move |glyph, _, _| match glyph {
            Glyph::Running => Spinner::new().color(primary).into_any_element(),
            Glyph::Settled { failed } => {
                let icon = Icon::empty()
                    .path(icon.clone())
                    .text_color(muted)
                    .into_any_element();
                if !failed {
                    return icon;
                }
                div()
                    .relative()
                    .flex_none()
                    .child(icon)
                    .child(
                        div()
                            .absolute()
                            .right(px(-3.))
                            .bottom(px(-3.))
                            .rounded_full()
                            .bg(background)
                            .child(Icon::new(IconName::CircleX).size(px(7.)).text_color(danger)),
                    )
                    .into_any_element()
            }
        })
    })
    .into_any_element()
}

/// A section of a body: a small tracked caption over its content.
pub(in crate::chat) fn section(label: &str, content: impl IntoElement, cx: &App) -> Div {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(
            div()
                .text_size(px(10.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(crate::material::tracked_uppercase(label)),
        )
        .child(content)
}
