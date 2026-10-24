//! How a `tcode_control` grant reads and is edited: one editor for Settings →
//! Control's default and project grants and for a thread's control chip.

use std::rc::Rc;

use gpui::{
    AnyElement, App, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div,
};
use gpui_base::v_flex;
use tcode_core::control::{
    ControlCategory, ControlGrant, ControlGrantSource, ControlScope, ControlTier,
};
use tcode_core::project::Project;

use crate::typography::TextRole as _;
use crate::widgets::checkbox::Checkbox;
use crate::widgets::choice::{Choice, ChoiceGroup};
use crate::widgets::switch::Switch;

pub(crate) type GrantChange = Rc<dyn Fn(ControlGrant, &mut Window, &mut App)>;

pub(crate) fn tier_label(tier: ControlTier) -> String {
    match tier {
        ControlTier::Observe => crate::tr!("control.tier.observe"),
        ControlTier::Organize => crate::tr!("control.tier.organize"),
        ControlTier::Operate => crate::tr!("control.tier.operate"),
    }
    .into_owned()
}

fn tier_description(tier: ControlTier) -> String {
    match tier {
        ControlTier::Observe => crate::tr!("control.tier.observe_description"),
        ControlTier::Organize => crate::tr!("control.tier.organize_description"),
        ControlTier::Operate => crate::tr!("control.tier.operate_description"),
    }
    .into_owned()
}

pub(crate) fn category_label(category: ControlCategory) -> String {
    match category {
        ControlCategory::AnswerInput => crate::tr!("control.category.answer_input"),
        ControlCategory::Terminal => crate::tr!("control.category.terminal"),
        ControlCategory::Delete => crate::tr!("control.category.delete"),
        ControlCategory::GitRemote => crate::tr!("control.category.git_remote"),
        ControlCategory::PrWrite => crate::tr!("control.category.pr_write"),
        ControlCategory::HistoryEdit => crate::tr!("control.category.history_edit"),
        ControlCategory::SettingsWrite => crate::tr!("control.category.settings_write"),
    }
    .into_owned()
}

fn category_description(category: ControlCategory) -> String {
    match category {
        ControlCategory::AnswerInput => crate::tr!("control.category.answer_input_description"),
        ControlCategory::Terminal => crate::tr!("control.category.terminal_description"),
        ControlCategory::Delete => crate::tr!("control.category.delete_description"),
        ControlCategory::GitRemote => crate::tr!("control.category.git_remote_description"),
        ControlCategory::PrWrite => crate::tr!("control.category.pr_write_description"),
        ControlCategory::HistoryEdit => crate::tr!("control.category.history_edit_description"),
        ControlCategory::SettingsWrite => {
            crate::tr!("control.category.settings_write_description")
        }
    }
    .into_owned()
}

/// "project", "2 projects" or "host".
pub(crate) fn scope_label(scope: &ControlScope) -> String {
    match scope {
        ControlScope::Project => crate::tr!("control.scope.project_short"),
        ControlScope::Projects(ids) => {
            crate::tr!("control.scope.projects_short", count = ids.len())
        }
        ControlScope::Host => crate::tr!("control.scope.host_short"),
    }
    .into_owned()
}

/// "project · observe+organize+operate", as the chip and the lists name a grant.
pub(crate) fn grant_summary(grant: &ControlGrant) -> String {
    let parts: Vec<String> = grant
        .tiers
        .iter()
        .map(|tier| tier_label(*tier))
        .chain(
            grant
                .categories
                .iter()
                .map(|category| category_label(*category)),
        )
        .collect();
    let allowed = if parts.is_empty() {
        crate::tr!("control.nothing").into_owned()
    } else {
        parts.join("+")
    };
    format!("{} · {allowed}", scope_label(&grant.scope))
}

pub(crate) fn source_label(source: ControlGrantSource) -> String {
    match source {
        ControlGrantSource::Default => crate::tr!("control.source.default"),
        ControlGrantSource::Project => crate::tr!("control.source.project"),
        ControlGrantSource::Thread => crate::tr!("control.source.thread"),
    }
    .into_owned()
}

/// A `ControlAction`'s operation in words; an operation this client does not
/// know keeps the name the host recorded.
pub(crate) fn action_label(action: &str) -> String {
    let key = match action {
        "settle" => "control.action.settle",
        "unsettle" => "control.action.unsettle",
        "archive" => "control.action.archive",
        "unarchive" => "control.action.unarchive",
        "pin" => "control.action.pin",
        "unpin" => "control.action.unpin",
        "rename" => "control.action.rename",
        "mark_read" => "control.action.mark_read",
        "mark_unread" => "control.action.mark_unread",
        "follow_up" => "control.action.follow_up",
        "follow_up off" => "control.action.follow_up_off",
        "park" => "control.action.park",
        "park off" => "control.action.park_off",
        "keep" => "control.action.keep",
        "keep off" => "control.action.keep_off",
        "settle_self" => "control.action.settle_self",
        "record_review" => "control.action.record_review",
        "request_grant" => "control.action.request_grant",
        "notify_when" => "control.action.notify_when",
        "cancel_notify" => "control.action.cancel_notify",
        "respond_input" => "control.action.respond_input",
        "terminal" => "control.action.terminal",
        "delete_threads" => "control.action.delete_threads",
        "rewind" => "control.action.rewind",
        "patch_settings" => "control.action.patch_settings",
        "start_thread" => "control.action.start_thread",
        "send_message" => "control.action.send_message",
        "control_thread" => "control.action.control_thread",
        "pull_request" => "control.action.pull_request",
        "git" => "control.action.git",
        "create_project" => "control.action.create_project",
        "reference" => "control.action.reference",
        _ => return action.to_owned(),
    };
    crate::tr!(key).into_owned()
}

/// Scope, tiers and categories of one grant. Every edit reports the whole new
/// grant through `on_change`; the caller decides where it is stored.
pub(crate) fn grant_editor(
    id: impl Into<SharedString>,
    grant: &ControlGrant,
    projects: &[Project],
    compact: bool,
    on_change: GrantChange,
    cx: &App,
) -> gpui::Div {
    let id: SharedString = id.into();
    let caption = |text: String| div().pt_1().text_caption(cx).child(text);
    let selected_projects = match &grant.scope {
        ControlScope::Projects(ids) => ids.clone(),
        _ => Vec::new(),
    };
    let scopes = [
        (ControlScope::Project, crate::tr!("control.scope.project")),
        (
            ControlScope::Projects(selected_projects.clone()),
            crate::tr!("control.scope.projects"),
        ),
        (ControlScope::Host, crate::tr!("control.scope.host")),
    ];
    let scope_choices = scopes
        .into_iter()
        .enumerate()
        .map(|(index, (scope, label))| {
            let selected = std::mem::discriminant(&scope) == std::mem::discriminant(&grant.scope);
            let grant = grant.clone();
            let on_change = on_change.clone();
            Choice::new(SharedString::from(format!("{id}-scope-{index}")), label)
                .selected(selected)
                .on_select(move |window, cx| {
                    let mut next = grant.clone();
                    next.scope = scope.clone();
                    on_change(next, window, cx);
                })
        });
    let mut editor = v_flex()
        .w_full()
        .gap_2()
        .child(caption(crate::tr!("control.scope.title").into_owned()))
        .child(
            ChoiceGroup::new(SharedString::from(format!("{id}-scope")))
                .compact(compact)
                .aria_label(crate::tr!("control.scope.title"))
                .choices(scope_choices),
        );
    if matches!(grant.scope, ControlScope::Projects(_)) {
        let mut list = v_flex().gap_1();
        if projects.is_empty() {
            list = list.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("control.scope.no_projects")),
            );
        }
        for project in projects {
            let checked = selected_projects.contains(&project.id);
            let grant = grant.clone();
            let on_change = on_change.clone();
            let project_id = project.id.clone();
            list = list.child(
                Checkbox::new(SharedString::from(format!("{id}-project-{}", project.id)))
                    .label(project.name.clone())
                    .checked(checked)
                    .on_click(move |checked, window, cx| {
                        let mut next = grant.clone();
                        if let ControlScope::Projects(ids) = &mut next.scope {
                            ids.retain(|id| id != &project_id);
                            if *checked {
                                ids.push(project_id.clone());
                            }
                        }
                        on_change(next, window, cx);
                    }),
            );
        }
        editor = editor.child(list);
    }
    let tier_rows = ControlTier::ALL
        .into_iter()
        .map(|tier| {
            let grant = grant.clone();
            let on_change = on_change.clone();
            switch_row(
                SharedString::from(format!("{id}-tier-{}", tier.name())),
                tier_label(tier),
                tier_description(tier),
                grant.allows_tier(tier),
                compact,
                Rc::new(move |on, window, cx| {
                    let mut next = grant.clone();
                    if on {
                        next.tiers.insert(tier);
                    } else {
                        next.tiers.remove(&tier);
                    }
                    on_change(next, window, cx);
                }),
                cx,
            )
        })
        .collect();
    let category_rows = ControlCategory::ALL
        .into_iter()
        .map(|category| {
            let grant = grant.clone();
            let on_change = on_change.clone();
            switch_row(
                SharedString::from(format!("{id}-category-{}", category.name())),
                category_label(category),
                category_description(category),
                grant.allows_category(category),
                compact,
                Rc::new(move |on, window, cx| {
                    let mut next = grant.clone();
                    if on {
                        next.categories.insert(category);
                    } else {
                        next.categories.remove(&category);
                    }
                    on_change(next, window, cx);
                }),
                cx,
            )
        })
        .collect();
    editor
        .child(caption(crate::tr!("control.tiers").into_owned()))
        .child(crate::material::grouped(tier_rows, cx))
        .child(caption(crate::tr!("control.categories").into_owned()))
        .child(crate::material::grouped(category_rows, cx))
}

type Toggle = Rc<dyn Fn(bool, &mut Window, &mut App)>;

fn switch_row(
    id: SharedString,
    title: String,
    description: String,
    checked: bool,
    compact: bool,
    on_toggle: Toggle,
    cx: &App,
) -> AnyElement {
    crate::material::group_row(cx)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_body(compact, cx).child(title.clone()))
                .child(div().text_secondary(compact, cx).child(description)),
        )
        .child(
            Switch::new(id)
                .checked(checked)
                .aria_label(title)
                .on_click(move |next, window, cx| on_toggle(*next, window, cx)),
        )
        .into_any_element()
}

/// A grant's summary over where it came from.
pub(crate) fn grant_heading(
    grant: &ControlGrant,
    source: ControlGrantSource,
    compact: bool,
    cx: &App,
) -> gpui::Div {
    v_flex()
        .gap_0p5()
        .child(div().text_body(compact, cx).child(grant_summary(grant)))
        .child(div().text_secondary(compact, cx).child(crate::tr!(
            "control.source.label",
            source = source_label(source)
        )))
}

pub(crate) fn when_label(when: &tcode_core::control::SelfSettleWhen) -> String {
    use tcode_core::control::SelfSettleWhen;
    match when {
        SelfSettleWhen::EndOfTurn => crate::tr!("control.self_settle.end_of_turn").into_owned(),
        SelfSettleWhen::PrMerged { url } => {
            crate::tr!("control.self_settle.pr_merged", url = url.as_str()).into_owned()
        }
        SelfSettleWhen::PrClosed { url } => {
            crate::tr!("control.self_settle.pr_closed", url = url.as_str()).into_owned()
        }
    }
}

/// Whether a reference from `from` to `to` sends `to`'s content to a different
/// project's or provider's model vendor, which the user is told once.
pub(crate) fn crosses_boundary(
    from: &tcode_core::project::SessionMeta,
    to: &tcode_core::project::SessionMeta,
) -> bool {
    from.project_id != to.project_id || from.provider != to.provider
}
