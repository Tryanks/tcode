use super::*;
use crate::chat::tools;
use tcode_core::pull_request_watch::WatchNotice;

impl TimelineView {
    /// The record of a call that linked a pull request to this thread.
    pub(super) fn compose_pull_request_record(
        &self,
        entry: &TimelineEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(linked) = tools::pull_request::card(entry) else {
            unreachable!();
        };
        let key = linked.key;
        let store = self.workspace_store.read(cx);
        let links = self
            .session_id
            .as_deref()
            .map_or(&[][..], |session_id| store.pull_requests(session_id));
        let snapshot = links
            .iter()
            .find(|link| link.key == key)
            .and_then(|link| link.snapshot.as_ref());
        let title = snapshot.map_or_else(
            || format!("{}#{}", key.repository, key.number),
            |snapshot| snapshot.title.clone(),
        );
        let (place, branches) = match snapshot {
            Some(snapshot) => (
                format!("{} #{}", key.repository, key.number),
                Some((
                    snapshot.head_branch.clone().into(),
                    snapshot.base_branch.clone().into(),
                )),
            ),
            None => (key.host.clone(), None),
        };
        let said = if linked.already {
            crate::tr!("pull_requests.record_already_linked")
        } else {
            crate::tr!("pull_requests.record_opened")
        };
        let at = entry
            .ts
            .map(|at| crate::chat::model::format_local_time(at).into());
        let shown = tcode_core::pull_request::shown(links, &key);
        let on_open = self.session_id.clone().filter(|_| shown).map(|session_id| {
            let store = self.workspace_store.clone();
            let window_state = self.window_state.clone();
            Box::new(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                // On a phone the pull request opens as its own page.
                if !window_state.read(cx).compact {
                    store.update(cx, |store, cx| {
                        store.open_tab_for(&session_id, tcode_core::ui::RightTab::PullRequests, cx)
                    });
                }
                crate::pull_requests::open_pull_request(
                    &store,
                    &window_state,
                    session_id.clone(),
                    key.clone(),
                    window,
                    cx,
                );
            }) as components::changed_files::ClickHandler
        });
        components::pull_request::record_card(
            &entry.id,
            components::pull_request::PullRequestRecord {
                said: said.into_owned().into(),
                at,
                title: title.into(),
                place: place.into(),
                branches,
                on_open,
            },
            cx,
        )
    }

    /// A watch's wake of this thread, which opens to the message it sent.
    pub(super) fn compose_pull_request_update(
        &self,
        (turn, key, steering): (usize, String, bool),
        (number, notice): (u64, &WatchNotice),
        message: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let links = self
            .session_id
            .as_deref()
            .map_or(&[][..], |session_id| store.pull_requests(session_id));
        // The wake names only the number: a title shows where one linked
        // pull request has it.
        let mut numbered = links.iter().filter(|link| link.key.number == number);
        let pull_request_title = match (numbered.next(), numbered.next()) {
            (Some(link), None) => link
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.title.clone()),
            _ => None,
        };
        let title = std::iter::once(
            crate::tr!("pull_requests.update_title", number = number.to_string()).into_owned(),
        )
        .chain(pull_request_title)
        .chain(steering.then(|| crate::tr!("chat.steering").into_owned()))
        .collect::<Vec<_>>()
        .join(" · ");
        let expanded = self.expanded.contains(&key);
        let toggle_key = key.clone();
        components::pull_request::update_card(
            &key,
            title.into(),
            notice,
            message,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            &self.motion,
            cx,
        )
    }
}
