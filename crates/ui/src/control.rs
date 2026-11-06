//! How a `tcode_control` grant reads and is edited: the presets, the words a
//! grant or a request is put in, how a thread's grant differs from the one it
//! would otherwise have, and the one editor Settings → Control opens for the
//! default and for a project's exception.

use std::collections::BTreeSet;
use std::rc::Rc;

use gpui::{
    AnyElement, App, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div,
};
use gpui_base::v_flex;
use tcode_core::control::{
    ControlCategory, ControlGrant, ControlGrantRequest, ControlScope, ControlTier,
};
use tcode_core::project::{Project, SessionMeta};

use crate::typography::TextRole as _;
use crate::widgets::checkbox::Checkbox;
use crate::widgets::choice::{Choice, ChoiceGroup};
use crate::widgets::switch::Switch;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

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

/// The privileged categories by what follows from allowing them, which is
/// what the user weighs; the editor groups them this way.
pub(crate) const CONSEQUENCES: [(&str, &[ControlCategory]); 3] = [
    (
        "control.consequence.leaves_machine",
        &[ControlCategory::GitRemote, ControlCategory::PrWrite],
    ),
    (
        "control.consequence.irreversible",
        &[ControlCategory::Delete, ControlCategory::HistoryEdit],
    ),
    (
        "control.consequence.on_your_behalf",
        &[
            ControlCategory::AnswerInput,
            ControlCategory::Terminal,
            ControlCategory::SettingsWrite,
        ],
    ),
];

/// "own project", "2 projects" or "whole machine".
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

/// The grants Settings → Control offers by name. Each is one of the tiers
/// added to the ones before it, reaching the thread's own project or the
/// whole machine, with no privileged category; any other grant is custom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantPreset {
    ReadOnly,
    ReadAndOrganize,
    Standard,
}

impl GrantPreset {
    pub(crate) const ALL: [GrantPreset; 3] =
        [Self::ReadOnly, Self::ReadAndOrganize, Self::Standard];

    fn tiers(self) -> BTreeSet<ControlTier> {
        let count = match self {
            Self::ReadOnly => 1,
            Self::ReadAndOrganize => 2,
            Self::Standard => 3,
        };
        ControlTier::ALL.into_iter().take(count).collect()
    }

    /// The preset `grant` is, if it is one.
    pub(crate) fn of(grant: &ControlGrant) -> Option<Self> {
        if !matches!(grant.scope, ControlScope::Project | ControlScope::Host)
            || !grant.categories.is_empty()
        {
            return None;
        }
        Self::ALL
            .into_iter()
            .find(|preset| preset.tiers() == grant.tiers)
    }

    /// This preset with `grant`'s scope, when that is one a preset has, and
    /// otherwise the thread's own project.
    pub(crate) fn apply(self, grant: &ControlGrant) -> ControlGrant {
        let scope = match grant.scope {
            ControlScope::Host => ControlScope::Host,
            _ => ControlScope::Project,
        };
        ControlGrant {
            scope,
            tiers: self.tiers(),
            categories: BTreeSet::new(),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadAndOrganize => "read-and-organize",
            Self::Standard => "standard",
        }
    }

    pub(crate) fn label(self) -> String {
        match self {
            Self::ReadOnly => crate::tr!("control.preset.read_only"),
            Self::ReadAndOrganize => crate::tr!("control.preset.organize"),
            Self::Standard => crate::tr!("control.preset.standard"),
        }
        .into_owned()
    }

    pub(crate) fn description(self) -> String {
        match self {
            Self::ReadOnly => crate::tr!("control.preset.read_only_description"),
            Self::ReadAndOrganize => crate::tr!("control.preset.organize_description"),
            Self::Standard => crate::tr!("control.preset.standard_description"),
        }
        .into_owned()
    }
}

/// "Read only · own project", as a project's exception names its grant.
pub(crate) fn grant_summary(grant: &ControlGrant) -> String {
    let name = GrantPreset::of(grant).map_or_else(
        || crate::tr!("control.preset.custom_short").into_owned(),
        GrantPreset::label,
    );
    format!("{name} · {}", scope_label(&grant.scope))
}

/// One way a thread's own grant differs from the grant it would have
/// without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GrantDelta {
    Scope(ControlScope),
    Tier {
        tier: ControlTier,
        added: bool,
    },
    Category {
        category: ControlCategory,
        added: bool,
    },
}

impl GrantDelta {
    /// A privileged category the baseline does not allow.
    pub(crate) fn sensitive(&self) -> bool {
        matches!(self, Self::Category { added: true, .. })
    }
}

/// How `grant` differs from `baseline`: its scope when that is another, then
/// each tier and category one allows and the other does not.
pub(crate) fn grant_deltas(grant: &ControlGrant, baseline: &ControlGrant) -> Vec<GrantDelta> {
    let mut deltas = Vec::new();
    if grant.scope != baseline.scope {
        deltas.push(GrantDelta::Scope(grant.scope.clone()));
    }
    for tier in ControlTier::ALL {
        if grant.allows_tier(tier) != baseline.allows_tier(tier) {
            deltas.push(GrantDelta::Tier {
                tier,
                added: grant.allows_tier(tier),
            });
        }
    }
    for category in ControlCategory::ALL {
        if grant.allows_category(category) != baseline.allows_category(category) {
            deltas.push(GrantDelta::Category {
                category,
                added: grant.allows_category(category),
            });
        }
    }
    deltas
}

pub(crate) fn delta_label(delta: &GrantDelta, projects: &[Project]) -> String {
    match delta {
        GrantDelta::Scope(ControlScope::Host) => crate::tr!("control.scope.host").into_owned(),
        GrantDelta::Scope(ControlScope::Project) => {
            crate::tr!("control.scope.project").into_owned()
        }
        GrantDelta::Scope(ControlScope::Projects(ids)) => {
            crate::i18n::join_list(project_names(ids, projects))
        }
        GrantDelta::Tier { tier, added: true } => tier_label(*tier),
        GrantDelta::Tier { tier, added: false } => {
            crate::tr!("control.settings.no_tier", tier = tier_label(*tier)).into_owned()
        }
        GrantDelta::Category {
            category,
            added: true,
        } => category_label(*category),
        GrantDelta::Category {
            category,
            added: false,
        } => crate::tr!("control.settings.no_tier", tier = category_label(*category)).into_owned(),
    }
}

fn project_names(ids: &[String], projects: &[Project]) -> Vec<String> {
    ids.iter()
        .map(|id| {
            projects
                .iter()
                .find(|project| &project.id == id)
                .map_or_else(|| id.clone(), |project| project.name.clone())
        })
        .collect()
}

/// "a, b and c" in the UI language.
fn join_list(mut items: Vec<String>) -> String {
    let Some(last) = items.pop() else {
        return String::new();
    };
    if items.is_empty() {
        return last;
    }
    format!(
        "{}{}{last}",
        crate::i18n::join_list(items),
        crate::tr!("control.phrase.and")
    )
}

/// What `grant` lets a thread do, in a phrase that completes "wants to …"
/// and "can …": `own_project` names the thread's project where it is known.
/// `None` when it allows nothing.
pub(crate) fn grant_phrase(
    grant: &ControlGrant,
    own_project: Option<&str>,
    projects: &[Project],
) -> Option<String> {
    let mut parts = Vec::new();
    if !grant.tiers.is_empty() {
        let verbs = grant
            .tiers
            .iter()
            .map(|tier| {
                match tier {
                    ControlTier::Observe => crate::tr!("control.phrase.observe"),
                    ControlTier::Organize => crate::tr!("control.phrase.organize"),
                    ControlTier::Operate => crate::tr!("control.phrase.operate"),
                }
                .into_owned()
            })
            .collect();
        let scope = match &grant.scope {
            ControlScope::Host => crate::tr!("control.phrase.scope_host").into_owned(),
            ControlScope::Project => match own_project {
                Some(name) => crate::tr!("control.phrase.scope_named", names = name).into_owned(),
                None => crate::tr!("control.phrase.scope_project").into_owned(),
            },
            ControlScope::Projects(ids) => {
                let mut names: Vec<String> = own_project.map(str::to_owned).into_iter().collect();
                for name in project_names(ids, projects) {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                crate::tr!(
                    "control.phrase.scope_named",
                    names = crate::i18n::join_list(names)
                )
                .into_owned()
            }
        };
        parts.push(
            crate::tr!(
                "control.phrase.tiers",
                verbs = join_list(verbs),
                scope = scope
            )
            .into_owned(),
        );
    }
    for category in &grant.categories {
        parts.push(
            match category {
                ControlCategory::AnswerInput => crate::tr!("control.phrase.answer_input"),
                ControlCategory::Terminal => crate::tr!("control.phrase.terminal"),
                ControlCategory::Delete => crate::tr!("control.phrase.delete"),
                ControlCategory::GitRemote => crate::tr!("control.phrase.git_remote"),
                ControlCategory::PrWrite => crate::tr!("control.phrase.pr_write"),
                ControlCategory::HistoryEdit => crate::tr!("control.phrase.history_edit"),
                ControlCategory::SettingsWrite => crate::tr!("control.phrase.settings_write"),
            }
            .into_owned(),
        );
    }
    (!parts.is_empty()).then(|| join_list(parts))
}

/// "Now it can view threads in infra.": what the thread holds while it asks.
pub(crate) fn current_grant_sentence(
    grant: &ControlGrant,
    own_project: Option<&str>,
    projects: &[Project],
) -> String {
    match grant_phrase(grant, own_project, projects) {
        Some(what) => crate::tr!("control.request.now", what = what).into_owned(),
        None => crate::tr!("control.request.now_nothing").into_owned(),
    }
}

/// Every grant request waiting for the user, across the threads the client
/// holds, newest first.
pub(crate) fn pending_requests(
    sessions: impl IntoIterator<Item = SessionMeta>,
) -> Vec<(SessionMeta, ControlGrantRequest)> {
    let mut pending: Vec<(SessionMeta, ControlGrantRequest)> = sessions
        .into_iter()
        .flat_map(|meta| {
            meta.control_grant_requests
                .clone()
                .into_iter()
                .map(move |request| (meta.clone(), request))
        })
        .collect();
    pending.sort_by_key(|(_, request)| std::cmp::Reverse(request.requested_at));
    pending
}

/// The privileged category an operation needed, for the activity list to
/// mark; an operation whose need depends on its target is not marked.
pub(crate) fn action_category(key: &str) -> Option<ControlCategory> {
    use tcode_core::control::action;
    Some(match key {
        action::TERMINAL_OPEN | action::TERMINAL_INPUT | action::TERMINAL_CLOSE => {
            ControlCategory::Terminal
        }
        action::DELETE_THREADS => ControlCategory::Delete,
        action::REWIND => ControlCategory::HistoryEdit,
        action::PATCH_SETTINGS => ControlCategory::SettingsWrite,
        action::GIT_PUSH
        | action::GIT_PULL
        | action::GIT_COMMIT_PUSH
        | action::GIT_PUBLISH_BRANCH
        | action::GIT_MERGE_WORKTREE => ControlCategory::GitRemote,
        action::PR_COMMENT | action::PR_REPLY | action::PR_RESOLVE => ControlCategory::PrWrite,
        _ => return None,
    })
}

/// Whether an operation's recorded detail is words a reader can use rather
/// than an id only the host can resolve.
pub(crate) fn action_detail_readable(key: &str) -> bool {
    use tcode_core::control::action;
    !matches!(
        key,
        action::NOTIFY_WHEN
            | action::CANCEL_NOTIFY
            | action::RESPOND_INPUT
            | action::RESPOND_APPROVAL
            | action::TERMINAL_INPUT
            | action::TERMINAL_CLOSE
            | action::REWIND
            | action::RETRY
            | action::SET_OPTION
            | action::STEER_QUEUED
            | action::DROP_QUEUED
    )
}

/// Which day an activity entry falls on, as the list groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ActivityDay {
    Today,
    Yesterday,
    Earlier,
}

impl ActivityDay {
    /// The day of `at_millis` seen from `now`, by the calendar of `now`'s
    /// time zone.
    pub(crate) fn of<Tz: chrono::TimeZone>(at_millis: u64, now: &chrono::DateTime<Tz>) -> Self {
        let Some(at) = chrono::DateTime::from_timestamp_millis(at_millis as i64) else {
            return Self::Earlier;
        };
        let day = at.with_timezone(&now.timezone()).date_naive();
        let today = now.date_naive();
        if day >= today {
            Self::Today
        } else if today.pred_opt() == Some(day) {
            Self::Yesterday
        } else {
            Self::Earlier
        }
    }

    pub(crate) fn label(self) -> String {
        match self {
            Self::Today => crate::tr!("control.settings.activity_today"),
            Self::Yesterday => crate::tr!("control.settings.activity_yesterday"),
            Self::Earlier => crate::tr!("control.settings.activity_earlier"),
        }
        .into_owned()
    }
}

/// A `ControlAction`'s operation in words; an operation this client does not
/// know keeps the key the host recorded.
pub(crate) fn action_label(key: &str) -> String {
    use tcode_core::control::action;
    let key = match key {
        action::SETTLE => "control.action.settle",
        action::SETTLED_ITSELF => "control.action.settled_itself",
        action::UNSETTLE => "control.action.unsettle",
        action::ARCHIVE => "control.action.archive",
        action::UNARCHIVE => "control.action.unarchive",
        action::PIN => "control.action.pin",
        action::UNPIN => "control.action.unpin",
        action::RENAME => "control.action.rename",
        action::MARK_READ => "control.action.mark_read",
        action::MARK_UNREAD => "control.action.mark_unread",
        action::FOLLOW_UP => "control.action.follow_up",
        action::FOLLOW_UP_OFF => "control.action.follow_up_off",
        action::PARK => "control.action.park",
        action::PARK_OFF => "control.action.park_off",
        action::KEEP => "control.action.keep",
        action::KEEP_OFF => "control.action.keep_off",
        action::SETTLE_SELF => "control.action.settle_self",
        action::RECORD_REVIEW => "control.action.record_review",
        action::REQUEST_GRANT => "control.action.request_grant",
        action::NOTIFY_WHEN => "control.action.notify_when",
        action::CANCEL_NOTIFY => "control.action.cancel_notify",
        action::RESPOND_INPUT => "control.action.respond_input",
        action::RESPOND_APPROVAL => "control.action.respond_approval",
        action::TERMINAL_OPEN => "control.action.terminal_open",
        action::TERMINAL_INPUT => "control.action.terminal_input",
        action::TERMINAL_CLOSE => "control.action.terminal_close",
        action::DELETE_THREADS => "control.action.delete_threads",
        action::REWIND => "control.action.rewind",
        action::PATCH_SETTINGS => "control.action.patch_settings",
        action::START_THREAD => "control.action.start_thread",
        action::SEND_QUEUED => "control.action.send_queued",
        action::SEND_STEERED => "control.action.send_steered",
        action::SEND_SCHEDULED => "control.action.send_scheduled",
        action::INTERRUPT => "control.action.interrupt",
        action::CANCEL => "control.action.cancel",
        action::RETRY => "control.action.retry",
        action::COMPACT => "control.action.compact",
        action::SET_MODEL => "control.action.set_model",
        action::SET_OPTION => "control.action.set_option",
        action::FORK => "control.action.fork",
        action::STEER_QUEUED => "control.action.steer_queued",
        action::DROP_QUEUED => "control.action.drop_queued",
        action::PR_LINK => "control.action.pr_link",
        action::PR_UNLINK => "control.action.pr_unlink",
        action::PR_WATCH => "control.action.pr_watch",
        action::PR_UNWATCH => "control.action.pr_unwatch",
        action::PR_REFRESH => "control.action.pr_refresh",
        action::PR_COMMENT => "control.action.pr_comment",
        action::PR_REPLY => "control.action.pr_reply",
        action::PR_RESOLVE => "control.action.pr_resolve",
        action::GIT_CHECKOUT => "control.action.git_checkout",
        action::GIT_MERGE_WORKTREE => "control.action.git_merge_worktree",
        action::GIT_COMMIT => "control.action.git_commit",
        action::GIT_COMMIT_PUSH => "control.action.git_commit_push",
        action::GIT_PUSH => "control.action.git_push",
        action::GIT_PULL => "control.action.git_pull",
        action::GIT_PUBLISH_BRANCH => "control.action.git_publish_branch",
        action::GIT_INITIALIZE => "control.action.git_initialize",
        action::CREATE_PROJECT => "control.action.create_project",
        action::MOVE => "control.action.move",
        action::REFERENCE => "control.action.reference",
        _ => return key.to_owned(),
    };
    crate::tr!(key).into_owned()
}

/// Scope, the everyday tiers and the privileged categories of one grant,
/// the categories grouped by what follows from them. Every edit reports the
/// whole new grant through `on_change`; the caller decides where it is
/// stored.
pub(crate) fn grant_editor(
    id: impl Into<SharedString>,
    grant: &ControlGrant,
    projects: &[Project],
    compact: bool,
    on_change: GrantChange,
    cx: &App,
) -> gpui::Div {
    let id: SharedString = id.into();
    let caption = |text: String| {
        div()
            .pt_1()
            .pl(gpui::px(crate::material::CARD_INSET))
            .text_caption(cx)
            .child(text)
    };
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
    // The checklist folds in under the scope choice and away again, drawn
    // with the projects it had checked while it goes.
    let checklist = {
        let (id, grant, on_change) = (id.clone(), grant.clone(), on_change.clone());
        let projects = projects.to_vec();
        let shown_projects =
            matches!(grant.scope, ControlScope::Projects(_)).then(|| selected_projects.clone());
        crate::motion::Sampled::new(move |window, cx| {
            let Some(shown) = crate::motion::shown(
                SharedString::from(format!("{id}-projects")),
                shown_projects,
                SHOW,
                window,
                cx,
            ) else {
                return div().into_any_element();
            };
            let selected_projects = &shown.value;
            let mut list = v_flex().gap_1();
            if projects.is_empty() {
                list = list.child(
                    div()
                        .text_secondary(compact, cx)
                        .child(crate::tr!("control.scope.no_projects")),
                );
            }
            for project in &projects {
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
            list = list.child(
                div()
                    .text_secondary(compact, cx)
                    .child(crate::tr!("control.scope.own_always")),
            );
            shown.fold(list.pt_2())
        })
    };
    let mut editor = v_flex()
        .w_full()
        .gap_2()
        .child(caption(crate::tr!("control.scope.title").into_owned()))
        .child(
            v_flex()
                .child(
                    ChoiceGroup::new(SharedString::from(format!("{id}-scope")))
                        .compact(compact)
                        .aria_label(crate::tr!("control.scope.title"))
                        .choices(scope_choices),
                )
                .child(checklist),
        );
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
    editor = editor
        .child(caption(crate::tr!("control.daily").into_owned()))
        .child(crate::material::grouped(tier_rows, cx))
        .child(caption(
            crate::tr!("control.consequence.title").into_owned(),
        ));
    for (consequence, categories) in CONSEQUENCES {
        let rows = categories
            .iter()
            .map(|&category| {
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
        editor = editor
            .child(
                div()
                    .pl(gpui::px(crate::material::CARD_INSET))
                    .text_secondary(compact, cx)
                    .child(crate::tr!(consequence)),
            )
            .child(crate::material::grouped(rows, cx));
    }
    editor
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

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(
        scope: ControlScope,
        tiers: &[ControlTier],
        categories: &[ControlCategory],
    ) -> ControlGrant {
        ControlGrant {
            scope,
            tiers: tiers.iter().copied().collect(),
            categories: categories.iter().copied().collect(),
        }
    }

    /// The presets are named grants: the factory default is Standard, each
    /// preset is its tiers at the thread's own project or the whole machine,
    /// and anything else — a category, a project list, an odd set of tiers —
    /// is custom. Choosing a preset keeps a whole-machine scope and drops what
    /// only Custom can express.
    #[test]
    fn presets_are_the_tier_ladder_at_own_project_or_whole_machine() {
        use ControlTier::*;
        assert_eq!(
            GrantPreset::of(&ControlGrant::default_grant()),
            Some(GrantPreset::Standard)
        );
        for (tiers, preset) in [
            (&[Observe][..], Some(GrantPreset::ReadOnly)),
            (&[Observe, Organize][..], Some(GrantPreset::ReadAndOrganize)),
            (
                &[Observe, Organize, Operate][..],
                Some(GrantPreset::Standard),
            ),
            (&[Organize][..], None),
            (&[][..], None),
        ] {
            for scope in [ControlScope::Project, ControlScope::Host] {
                assert_eq!(
                    GrantPreset::of(&grant(scope, tiers, &[])),
                    preset,
                    "{tiers:?}"
                );
            }
            assert_eq!(
                GrantPreset::of(&grant(ControlScope::Projects(vec!["p".into()]), tiers, &[])),
                None
            );
        }
        assert_eq!(
            GrantPreset::of(&grant(
                ControlScope::Project,
                &[Observe],
                &[ControlCategory::Terminal]
            )),
            None
        );

        let custom = grant(
            ControlScope::Projects(vec!["p".into()]),
            &[Observe],
            &[ControlCategory::GitRemote],
        );
        assert_eq!(
            GrantPreset::ReadAndOrganize.apply(&custom),
            grant(ControlScope::Project, &[Observe, Organize], &[])
        );
        let host = grant(ControlScope::Host, &[Observe], &[]);
        assert_eq!(
            GrantPreset::Standard.apply(&host),
            grant(ControlScope::Host, &[Observe, Organize, Operate], &[])
        );
        for preset in GrantPreset::ALL {
            assert_eq!(GrantPreset::of(&preset.apply(&custom)), Some(preset));
        }
    }

    /// A thread's own grant is shown by how it differs from what its project
    /// would give it: wider scope, privileged categories (the ones flagged)
    /// and missing tiers, and nothing it shares with the exception.
    #[test]
    fn a_threads_grant_differs_from_its_projects_exception_not_the_default() {
        use ControlTier::*;
        let mut control = tcode_core::control::ControlSettings::default();
        control
            .project_overrides
            .insert("web".into(), grant(ControlScope::Project, &[Observe], &[]));
        let baseline = control.resolve(None, Some("web")).0;
        let own = grant(
            ControlScope::Host,
            &[Observe, Organize],
            &[ControlCategory::GitRemote],
        );
        let deltas = grant_deltas(&own, &baseline);
        assert_eq!(
            deltas,
            vec![
                GrantDelta::Scope(ControlScope::Host),
                GrantDelta::Tier {
                    tier: Organize,
                    added: true
                },
                GrantDelta::Category {
                    category: ControlCategory::GitRemote,
                    added: true
                },
            ]
        );
        assert_eq!(
            deltas.iter().filter(|delta| delta.sensitive()).count(),
            1,
            "only the privileged category is flagged"
        );
        let narrower = grant(ControlScope::Project, &[Observe, Organize], &[]);
        assert_eq!(
            grant_deltas(&narrower, &control.default_grant),
            vec![GrantDelta::Tier {
                tier: Operate,
                added: false
            }]
        );
        assert!(grant_deltas(&baseline, &baseline).is_empty());
    }

    /// Activity is grouped by the calendar of the user's time zone: a write
    /// late yesterday evening is yesterday's even when it falls on today's
    /// date in UTC.
    #[test]
    fn activity_days_follow_the_local_calendar() {
        use chrono::TimeZone as _;
        let zone = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let now = zone.with_ymd_and_hms(2026, 11, 7, 0, 30, 0).unwrap();
        let at = |day, hour, minute| {
            zone.with_ymd_and_hms(2026, 11, day, hour, minute, 0)
                .unwrap()
                .timestamp_millis() as u64
        };
        assert_eq!(ActivityDay::of(at(7, 0, 10), &now), ActivityDay::Today);
        assert_eq!(ActivityDay::of(at(6, 23, 50), &now), ActivityDay::Yesterday);
        assert_eq!(ActivityDay::of(at(6, 0, 5), &now), ActivityDay::Yesterday);
        assert_eq!(ActivityDay::of(at(5, 23, 59), &now), ActivityDay::Earlier);
    }
}
