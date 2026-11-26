use super::*;
use crate::chat::tools;
use crate::widgets::input::{InputEvent, TextareaState};
use tcode_core::notice::CallbackEvent;

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a row drawn here takes to fold in or out, or to move to a new place.
const ROWS: std::time::Duration = std::time::Duration::from_millis(200);

/// How long a part drawn here takes to cross-fade to another of its states.
const CHANGE: std::time::Duration = std::time::Duration::from_millis(200);

/// A whole tool output fetched on request.
pub(in crate::chat) enum FullOutput {
    /// Dropping the task cancels the fetch.
    Loading {
        _request: Task<()>,
    },
    Loaded(SharedString),
    Failed(String),
}

/// A withheld record being read whole on request, or why that failed; once
/// read it takes its placeholder's place in the store's log.
pub(in crate::chat) enum WithheldLoad {
    /// Dropping the task stops the read.
    Loading {
        _request: Task<()>,
    },
    Failed(String),
}

/// A timeline row as drawn: what its segment shows, and on the turn's last
/// row the turn's trailer below it, the gap between the two its own.
pub(in crate::chat) struct RenderedRow {
    pub(in crate::chat) content: AnyElement,
    pub(in crate::chat) trailer: Option<gpui::Div>,
}

/// How one activity row is drawn where it stands.
pub(in crate::chat) struct ActivityRow {
    pub(in crate::chat) compact: bool,
    pub(in crate::chat) live_reasoning: bool,
    pub(in crate::chat) auto_expand: bool,
    pub(in crate::chat) recency: AutoActivityRecency,
    /// A completed call's images show as tiles above its row instead.
    pub(in crate::chat) images_inline: bool,
}

impl TimelineView {
    /// `model` by the name the host's model list gives it for this thread's
    /// profile, or as the harness gave it where the list does not know it.
    pub(super) fn model_name(&self, model: &str, cx: &App) -> String {
        self.session_id
            .as_deref()
            .and_then(|session_id| {
                self.workspace_store
                    .read(cx)
                    .thread_model_name(session_id, model)
            })
            .unwrap_or_else(|| model.to_owned())
    }

    /// Render one timeline row: the segment it spans (a message, an error, a
    /// Work Log run or a part of an expanded one) and, on the turn's last
    /// row, the turn's trailer.
    ///
    /// The trailer reads the whole turn (pending steers, the last
    /// timestamp). `pinned` carries the ids of the last user / last assistant
    /// message in the whole timeline: their action rows stay visible instead
    /// of waiting for a hover, except a turn footer's, which always waits.
    pub(in crate::chat) fn render_row(
        &mut self,
        args: RowRenderArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> RenderedRow {
        let (row, turn, cwd, own, entries, trailer_gap, pinned) = args;
        let index = row.turn;
        let mut column = v_flex().w_full().gap(px(SEGMENT_GAP));

        // A row's entries re-segment to the one segment it was indexed from.
        let segmented = segment_entries(own, turn.running);
        let segments = &segmented.flow;
        // The actions of the message the turn ends on, which its footer draws,
        // and the text they copy.
        let mut ending = None;

        for segment in segments.iter() {
            match segment {
                Segment::Relay(entry) => {
                    column = column.child(self.compose_relay(index, entry, cx));
                }
                Segment::ModelChange(entry) => {
                    column = column.child(self.compose_model_change(entry, cx));
                }
                Segment::ContextCompacted(entry) => {
                    let EntryContent::ContextCompacted(compaction) = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::context_compacted_divider(
                        &entry.id,
                        compaction,
                        &self.motion,
                        cx,
                    ));
                }
                Segment::ContextWindowChanged(entry) | Segment::SettingsKept(entry) => {
                    let tags = Vec::from_iter(components::notices::MessageTag::of(&entry.content));
                    column = column.child(components::notices::message_tags(&entry.id, &tags, cx));
                }
                Segment::SessionMoved(entry) => {
                    let EntryContent::SessionMoved { from, to } = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::session_moved_divider(
                        &entry.id, from, to, cx,
                    ));
                }
                Segment::Worktree(entry) => {
                    let EntryContent::Worktree { request, answer } = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::worktree_divider(
                        &entry.id, request, answer, cx,
                    ));
                }
                Segment::ControlAction(run) => {
                    column = column.child(self.compose_control_actions(run, cx));
                }
                Segment::Rewind(entry) => {
                    column = column.child(self.compose_rewound(index, entry, cx));
                }
                Segment::ActivityRun(activities) => {
                    column = column.child(self.compose_work_log(
                        (index, &row.part, turn, cwd, activities, row.live_activity),
                        window,
                        cx,
                    ));
                }
                Segment::Images(images) => {
                    // Images are read through their thread; a timeline with
                    // none has nothing to read them from.
                    if let Some(session) = self.session_key.clone() {
                        column = column.child(components::images::image_tiles(
                            self.workspace_store.read(cx).image_host(),
                            images,
                            cwd,
                            &session,
                            &self.motion.clone(),
                            window,
                            cx,
                        ));
                    }
                    for entry in images {
                        if matches!(
                            &entry.content,
                            EntryContent::Item(ItemContent::ToolCall { .. })
                        ) {
                            let row = self.compose_activity_row(
                                (entry, cwd),
                                ActivityRow {
                                    compact: true,
                                    live_reasoning: false,
                                    auto_expand: false,
                                    recency: AutoActivityRecency::Older,
                                    images_inline: false,
                                },
                                window,
                                cx,
                            );
                            column = column.child(self.shared_call(entry, "images", row, cx));
                        }
                    }
                }
                Segment::User(entry) => {
                    column = column.child(self.compose_user_entry(
                        (index, entry, cwd, pinned.0 == Some(entry.id.as_str())),
                        window,
                        cx,
                    ));
                }
                Segment::Assistant(entry) => {
                    let EntryContent::Item(ItemContent::AssistantMessage { text, summarized }) =
                        &entry.content
                    else {
                        unreachable!();
                    };
                    let (markdown, copy_text) = self.md_states.get(&entry.id).map_or_else(
                        || (None, Arc::from(text.as_str())),
                        |md| (Some(md.state.clone()), md.synced.clone()),
                    );
                    let copy_key = format!("assistant:{}", entry.id);
                    let copied = self.copied.is(&copy_key);
                    let mark = copy_key;
                    let compact = self.window_state.read(cx).compact;
                    let pinned = pinned.1 == Some(entry.id.as_str());
                    let actions = match (turn.running, row.last_assistant, row.last_in_turn) {
                        (false, true, true) => components::assistant::MessageActions::InFooter,
                        (false, true, false) => components::assistant::MessageActions::Below,
                        _ => components::assistant::MessageActions::None,
                    };
                    let fork = (actions != components::assistant::MessageActions::None)
                        .then(|| self.turn_fork(index, cx))
                        .flatten();
                    if actions == components::assistant::MessageActions::InFooter {
                        ending = Some((
                            components::assistant::EndingActions {
                                id: entry.id.clone(),
                                copied,
                                compact,
                                fork: fork.clone(),
                            },
                            copy_text.clone(),
                        ));
                    }
                    column = column.child(components::assistant::assistant(
                        components::assistant::AssistantData {
                            image_host: self.workspace_store.read(cx).image_host(),
                            id: &entry.id,
                            text,
                            copy_text: copy_text.clone(),
                            summarized: *summarized,
                            cwd,
                            markdown,
                            compact,
                            pinned,
                            actions,
                            fork,
                            copied,
                            hover: self.hover.clone(),
                            motion: self.motion.clone(),
                        },
                        cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                            this.mark_copied(mark.clone(), cx);
                        }),
                        cx,
                    ));
                }
                Segment::Plan(entry) => {
                    column = column.child(self.compose_plan(entry, cwd, cx));
                }
                Segment::Visualize(entry) => {
                    column = column.child(self.compose_visualize(entry, cx));
                }
                Segment::PullRequest(entry) => {
                    column = column.child(self.compose_pull_request_record(entry, cx));
                }
                Segment::ThreadCard(entry) => {
                    column = column.child(self.compose_thread_card(entry, window, cx));
                }
                Segment::Withheld(entry) => {
                    let EntryContent::Withheld { bytes, row } = entry.content else {
                        unreachable!();
                    };
                    let read = row.map(|row| self.withheld_read(row, entry.turn, cx));
                    column = column.child(components::notices::withheld_placeholder(
                        &entry.id, bytes, read, cx,
                    ));
                }
                Segment::RequestRetry(entry) => {
                    let EntryContent::RequestRetry { message } = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::request_retry_divider(
                        &entry.id,
                        message,
                        &self.motion,
                        cx,
                    ));
                }
                Segment::Warnings(warnings) => {
                    column = column.child(v_flex().w_full().gap_1().children(
                        warnings.iter().filter_map(|entry| match &entry.content {
                            EntryContent::Warning { message } => Some(
                                components::dividers::warning_divider(&entry.id, message, cx),
                            ),
                            _ => None,
                        }),
                    ));
                }
                Segment::Error(entry) => {
                    let harness = self
                        .workspace_store
                        .read(cx)
                        .harness_name_for(self.session_id.as_deref(), cx);
                    let message = displayed_error_text(&entry.content, &harness);
                    let resume = match &entry.content {
                        EntryContent::Error {
                            limit_resets_at: Some(resets_at),
                            ..
                        } => {
                            let resets_at = *resets_at;
                            let store = self.workspace_store.read(cx);
                            let queued = store
                                .session_status_for(self.session_id.as_deref(), cx)
                                .map_or(&[][..], |status| &status.queued_messages);
                            let queued_id = queued
                                .iter()
                                .find(|message| message.fire_at_unix_secs == Some(resets_at))
                                .map(|message| message.id);
                            let queue_empty = queued.is_empty();
                            let ends_thread = store
                                .with_timeline_for(
                                    self.session_id.as_deref(),
                                    |timeline| {
                                        !timeline.turn_running && index + 1 == timeline.turns.len()
                                    },
                                    cx,
                                )
                                .unwrap_or(false);
                            match limit_resume_action(queued_id, ends_thread, queue_empty) {
                                LimitResumeAction::Scheduled { queued_id: id } => {
                                    Some(components::error_card::LimitResume::Scheduled {
                                        resets_at: self
                                            .workspace_store
                                            .read(cx)
                                            .host_moment_here_secs(resets_at),
                                        on_cancel: Box::new(cx.listener(move |this, _, _, cx| {
                                            let Some(session_id) = this.session_id.clone() else {
                                                return;
                                            };
                                            this.workspace_store.update(cx, |store, _| {
                                                store.drop_queued_for(session_id, id)
                                            });
                                        })),
                                    })
                                }
                                LimitResumeAction::Schedule => {
                                    Some(components::error_card::LimitResume::Offer {
                                        on_schedule: Box::new(cx.listener(
                                            move |this, _, _, cx| {
                                                let Some(session_id) = this.session_id.clone()
                                                else {
                                                    return;
                                                };
                                                this.workspace_store.update(cx, |store, _| {
                                                    store.schedule_turn_for(
                                                        session_id,
                                                        tcode_core::session::RESUME_PROMPT
                                                            .to_string(),
                                                        vec![],
                                                        tcode_protocol::TurnDue::AtUnixSecs(
                                                            resets_at,
                                                        ),
                                                    )
                                                });
                                            },
                                        )),
                                    })
                                }
                                LimitResumeAction::None => None,
                            }
                        }
                        _ => None,
                    };
                    let retry = self
                        .workspace_store
                        .read(cx)
                        .with_timeline_for(
                            self.session_id.as_deref(),
                            |timeline| {
                                timeline
                                    .turn_retry
                                    .as_ref()
                                    .filter(|retry| {
                                        !timeline.turn_running
                                            && turn.provider_turn_id.as_deref()
                                                == Some(retry.turn_id.as_str())
                                            && timeline
                                                .entries
                                                .iter()
                                                .rev()
                                                .find(|entry| {
                                                    matches!(
                                                        entry.content,
                                                        EntryContent::Error { .. }
                                                    )
                                                })
                                                .is_some_and(|last| last.id == entry.id)
                                    })
                                    .cloned()
                            },
                            cx,
                        )
                        .flatten();
                    let resume = retry
                        .map(|retry| {
                            let turn_id = retry.turn_id;
                            components::error_card::LimitResume::Retry {
                                retry_at: retry.retry_at.map(|at| {
                                    self.workspace_store.read(cx).host_moment_here_secs(at)
                                }),
                                on_retry: Box::new(cx.listener(move |this, _, _, cx| {
                                    let Some(session_id) = this.session_id.clone() else {
                                        return;
                                    };
                                    this.workspace_store.update(cx, |store, _| {
                                        store.retry_turn_for(session_id, turn_id.clone())
                                    });
                                })),
                                on_cancel: Box::new(cx.listener(move |this, _, _, cx| {
                                    let Some(session_id) = this.session_id.clone() else {
                                        return;
                                    };
                                    this.workspace_store
                                        .update(cx, |store, _| store.interrupt_for(session_id));
                                })),
                            }
                        })
                        .or(resume);
                    let copy_key = format!("error:{}", entry.id);
                    let copied = self.copied.is(&copy_key);
                    let mark = copy_key;
                    let copy_text = message.to_string();
                    column = column.child(components::error_card::error_card(
                        &entry.id,
                        &message,
                        copied,
                        resume,
                        cx.listener(move |this, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                            this.mark_copied(mark.clone(), cx);
                        }),
                        &self.motion,
                        cx,
                    ));
                }
            }
        }
        let content = column.into_any_element();

        if !row.last_in_turn {
            return RenderedRow {
                content,
                trailer: None,
            };
        }
        let mut column = v_flex().w_full().gap(px(SEGMENT_GAP));
        let mut trailing = false;
        // The trailer keeps its motion and what the reader opened in it under
        // the turn's number in the whole thread, not its place in the window:
        // a page of history renumbers the window's turns, and each would take
        // up the state of the turn whose place it took.
        let number = self
            .workspace_store
            .read(cx)
            .absolute_turn_for(self.session_id.as_deref(), index);
        let motion = self.motion.scope(format!("turn-{number}"));
        // The gap between the row's segment and the trailer.
        let gap_above = (!segments.is_empty()).then(|| px(SEGMENT_GAP) * trailer_gap);

        // Changed-file evidence is the turn's settled summary: while the turn
        // still runs, the live per-file rows inside the work log carry this
        // information, so the section folds in only once the turn finishes.
        let (changes, completeness) = self.workspace_store.read(cx).chat_turn_changes_for(
            self.session_id.as_deref(),
            index,
            cx,
        );
        let changes_fold = motion
            .shown(
                "changed-files-fold",
                (!turn.running && !changes.is_empty()).then_some(()),
                SHOW,
                window,
                cx,
            )
            .filter(|_| !changes.is_empty());
        let changes = changes_fold.map(|fold| {
            let more_key = format!("changed-files-more-{number}");
            let show_all = self.expanded.contains(&more_key);
            let toggle_more_key = more_key;
            let handlers = components::changed_files::ChangedFilesHandlers {
                view_diff: Box::new(cx.listener(move |this, _, _, cx| {
                    this.open_diff(index, None, cx);
                })),
                toggle_more: Box::new(cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(index, &toggle_more_key, cx);
                })),
                open_files: changes
                    .iter()
                    .map(|change| {
                        let path = change.path.clone();
                        Box::new(cx.listener(move |this, _, _, cx| {
                            this.open_diff(index, Some(path.clone()), cx);
                        })) as components::changed_files::ClickHandler
                    })
                    .collect(),
            };
            // The gap below it folds with it.
            fold.fold(
                div()
                    .pb(px(SEGMENT_GAP))
                    .child(components::changed_files::changed_files(
                        index,
                        cwd,
                        &changes,
                        completeness,
                        show_all,
                        handlers,
                        &motion,
                        cx,
                    )),
            )
        });
        trailing |= changes.is_some();

        // The turn's liveness, bare after everything it produced: no fold can
        // reach it, so collapsing the last disclosure never hides that we work.
        // It cross-fades into the finished turn's footer as the turn ends:
        // the actions of the message it ends on, and its time row.
        let finished_at = turn.end_ts.or(entries.last().and_then(|e| e.ts));
        let mut status = None;
        // The stop for all the harness still runs in the background stands
        // in the footer of the thread's last turn once that has finished,
        // and goes when the next turn starts; a turn that would have had no
        // footer has one for it.
        let last = self
            .rows
            .last()
            .is_some_and(|last| last.identity == row.identity);
        let stops_background = last
            && !turn.running
            && crate::chat::background::stops_any(
                self.workspace_store.read(cx),
                self.session_id.as_deref(),
                cx,
            );
        if turn.running || finished_at.is_some() || ending.is_some() || stops_background {
            let requested_model = self
                .workspace_store
                .read(cx)
                .chat_requested_model_for(self.session_id.as_deref(), cx);
            let served_model =
                divergent_served_model(turn.served_model.as_deref(), requested_model.as_deref())
                    .map(|served| self.model_name(served, cx));
            // After Stop the turn says it is stopping until it ends, however
            // long its harness takes to take the stop.
            let stopping = self
                .workspace_store
                .read(cx)
                .session_status_for(self.session_id.as_deref(), cx)
                .is_some_and(|status| status.stopping);
            // The host stamped the start; the timer ticks on this client.
            let started_here = turn
                .start_ts
                .map(|start| self.workspace_store.read(cx).host_moment_here(start));
            let turn = turn.clone();
            trailing = true;
            // The status moves with the trailer when the turn gains a last
            // row, as a steer landing after everything does.
            let status_key = SharedString::from(format!("turn-status-{number}"));
            self.shared.arrive(
                &status_key,
                &SharedString::from(row.identity.to_string()),
                cx,
            );
            let indicator_motion = motion.clone();
            let (store, session_id) = (self.workspace_store.clone(), self.session_id.clone());
            let touch = self.window_state.read(cx).compact;
            let stop_motion = self.motion.clone();
            let stop_background = move |window: &mut Window, cx: &mut App| {
                last.then(|| {
                    crate::chat::background::stop_all(
                        &store,
                        session_id.as_deref(),
                        touch,
                        &stop_motion,
                        window,
                        cx,
                    )
                })
                .flatten()
            };
            let hover = self.hover.clone();
            // The footer is one hover with the message whose actions it
            // draws, the gap above it included when it leads the trailer:
            // the pointer crossing from the message to Copy never leaves it.
            let on_hover = ending
                .as_ref()
                .and_then(|(actions, _)| actions.on_hover(&hover));
            let status_gap = gap_above.filter(|_| changes.is_none());
            let footer = self.shared.element(
                status_key,
                motion.cross_fade(
                    "turn-status",
                    turn.running,
                    CHANGE,
                    window,
                    cx,
                    move |running, window, cx| {
                        if *running {
                            return div()
                                .id(("working-status", index))
                                .child(components::indicator::turn_working_indicator(
                                    number,
                                    started_here,
                                    stopping,
                                    served_model.clone(),
                                    &indicator_motion,
                                    cx,
                                ))
                                .into_any_element();
                        }
                        let actions = ending.as_ref().map(|(actions, text)| {
                            let text = text.clone();
                            let mark = format!("assistant:{}", actions.id);
                            let on_copy = cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                                this.mark_copied(mark.clone(), cx);
                            });
                            actions.render(&hover, &indicator_motion, on_copy, window, cx)
                        });
                        components::indicator::finished_turn_time(
                            finished_at,
                            &turn,
                            served_model.as_deref(),
                            actions,
                            stop_background(window, cx),
                            window,
                            cx,
                        )
                        .into_any_element()
                    },
                ),
            );
            status = Some(
                v_flex()
                    .id(("turn-status", index))
                    .w_full()
                    .when_some(status_gap, |status, gap| status.pt(gap))
                    .when_some(on_hover, |status, on_hover| status.on_hover(on_hover))
                    .child(footer),
            );
        }

        // Without the footer to lead it, the trailer keeps the gap above it.
        let lead_gap = gap_above.filter(|_| changes.is_some() || status.is_none());

        // Pending steers float below every live transcript/work-log element.
        // They are read from the whole turn, not this row's segment, so FIFO
        // order holds without making their request-time position look
        // model-visible.
        // A steer the agent takes leaves them: its place folds away, and the
        // message travels to where it now stands in the turn.
        let pending: Vec<_> = segment_entries(entries, turn.running)
            .pending_steers
            .into_iter()
            .map(|entry| (SharedString::from(entry.id.clone()), entry.id.clone()))
            .collect();
        trailing |= !pending.is_empty();
        let gap = px(SEGMENT_GAP);
        let steers = crate::motion::keyed::keyed_owned(
            &motion,
            "pending-steers",
            crate::motion::keyed::Fold::Down,
            pending,
            ROWS,
            window,
            cx,
            |key, id, window, cx| {
                let Some(entry) = entries.iter().find(|entry| entry.id == *id) else {
                    return div().into_any_element();
                };
                let steering = matches!(
                    entry.content,
                    EntryContent::Steer {
                        status: SteeringStatus::Pending,
                        ..
                    }
                );
                let message = if steering {
                    self.compose_user_entry(
                        (index, entry, cwd, pinned.0 == Some(entry.id.as_str())),
                        window,
                        cx,
                    )
                } else if self
                    .shared
                    .departed(&format!("steer-{key}").into(), &"trailer".into())
                {
                    // It is on its way to its place in the turn.
                    div().into_any_element()
                } else {
                    // Its place in the turn is not drawn: it fades here.
                    let shared = std::mem::take(&mut self.shared);
                    let message = self.compose_user_entry(
                        (index, entry, cwd, pinned.0 == Some(entry.id.as_str())),
                        window,
                        cx,
                    );
                    self.shared = shared;
                    message
                };
                div().w_full().pt(gap).child(message).into_any_element()
            },
        );
        // Each steer brings the gap above it, so none stays behind when the
        // last has gone.
        column = column.child(
            v_flex()
                .w_full()
                .children(status)
                .child(steers.render(v_flex().w_full())),
        );

        RenderedRow {
            content,
            trailer: trailing.then(|| {
                v_flex()
                    .w_full()
                    .when_some(lead_gap, |trailer, gap| trailer.pt(gap))
                    .children(changes)
                    .child(column)
            }),
        }
    }

    pub(in crate::chat) fn on_copy_whole_output(
        &mut self,
        action: &super::super::CopyWholeOutput,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.full_outputs.get(&action.item_id) {
            Some(FullOutput::Loaded(output)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(output.to_string()));
            }
            Some(FullOutput::Loading { .. }) => {
                self.copy_when_loaded = Some(action.item_id.clone());
            }
            None | Some(FullOutput::Failed(_)) => {
                self.copy_when_loaded = Some(action.item_id.clone());
                self.load_full_output(action.item_id.clone(), action.turn, cx);
            }
        }
    }

    pub(in crate::chat) fn on_open_rewind(
        &mut self,
        action: &OpenRewind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let OpenRewind { key, turn, steer } = action.clone();
        self.open_rewind(key, turn, steer, window, cx);
    }

    /// The fork an answer's context menu offers: it carries over the words
    /// the reader has selected, when any are.
    pub(in crate::chat) fn on_fork_through_turn(
        &mut self,
        action: &ForkThroughTurn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let quote = Some(self.selection.read(cx).selected_text(cx))
            .filter(|selected| !selected.trim().is_empty());
        self.fork_through_turn(action.turn, quote, window, cx);
    }

    /// Fork the thread through `turn` once the reader agrees to.
    fn fork_through_turn(
        &mut self,
        turn: usize,
        quote: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let store = self.workspace_store.clone();
        let of = if quote.is_some() {
            crate::chat::ForkOf::TurnWithQuote
        } else {
            crate::chat::ForkOf::Turn
        };
        crate::chat::confirm_fork(of, window, cx, move |cx| {
            let (session_id, quote) = (session_id.clone(), quote.clone());
            store.update(cx, |store, cx| {
                let target = store
                    .chat_fork_target_for(Some(&session_id), turn, cx)
                    .filter(|target| !target.blocked);
                if let Some(target) = target {
                    store.fork_through_turn(
                        session_id,
                        target.turn,
                        target.checkpoint_id,
                        quote,
                        cx,
                    );
                }
            });
        });
    }

    /// The fork finished turn `turn` offers beside its answer's Copy.
    fn turn_fork(
        &self,
        turn: usize,
        cx: &Context<Self>,
    ) -> Option<components::assistant::TurnFork> {
        let target = self.workspace_store.read(cx).chat_fork_target_for(
            self.session_id.as_deref(),
            turn,
            cx,
        )?;
        let this = cx.entity().downgrade();
        Some(components::assistant::TurnFork {
            turn,
            blocked: target.blocked,
            on_fork: Rc::new(move |turn, window, cx| {
                let _ = this.update(cx, |timeline, cx| {
                    timeline.fork_through_turn(turn, None, window, cx)
                });
            }),
        })
    }

    /// A rewind's divider, with its removed turns to read and the Harness's
    /// undo while it lasts, then the branch it left where the Harness keeps
    /// one.
    fn compose_rewound(
        &self,
        turn: usize,
        entry: &TimelineEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let EntryContent::Rewound(rewound) = &entry.content else {
            unreachable!("a rewind segment holds a rewind entry");
        };
        let undo_failure = rewound
            .undo_token
            .as_ref()
            .and_then(|_| self.rewind_failure_label(&rewound.checkpoint_id, true, cx));
        let store = self.workspace_store.read(cx);
        let harness = store.harness_name_for(self.session_id.as_deref(), cx);
        let status = store
            .session_status_for(self.session_id.as_deref(), cx)
            .and_then(|status| status.rewind.as_ref());
        let checkpoint = rewound.checkpoint_id.clone();
        let this = cx.entity().downgrade();
        let undo = rewound
            .undo_token
            .as_ref()
            .and(status)
            .and_then(|status| match &status.pending {
                Some(pending) if pending.undo && pending.checkpoint_id == checkpoint => {
                    Some(components::rewind::DividerUndo::InProgress)
                }
                _ if status.blocked.is_none() => {
                    let this = this.clone();
                    Some(components::rewind::DividerUndo::Available(Rc::new(
                        move |_, cx| {
                            let checkpoint = checkpoint.clone();
                            let _ = this
                                .update(cx, |timeline, cx| timeline.undo_rewind(checkpoint, cx));
                        },
                    )))
                }
                _ => None,
            })
            .map(|undo| match (undo, undo_failure.clone()) {
                (components::rewind::DividerUndo::Available(on_undo), Some(failure)) => {
                    components::rewind::DividerUndo::Failed(failure, Some(on_undo))
                }
                (undo, _) => undo,
            })
            .or_else(|| {
                undo_failure
                    .clone()
                    .map(|failure| components::rewind::DividerUndo::Failed(failure, None))
            })
            .or_else(|| {
                rewound
                    .branch
                    .as_ref()
                    .filter(|_| rewound.undo_token.is_none())?;
                let absolute = store.absolute_turn_for(self.session_id.as_deref(), turn);
                let session_id = self.session_id.clone()?;
                let checkpoint = rewound.checkpoint_id.clone();
                let store = self.workspace_store.clone();
                Some(components::rewind::DividerUndo::OpenBranch(Rc::new(
                    move |_, cx| {
                        store.update(cx, |store, cx| {
                            store.open_rewound_branch(
                                session_id.clone(),
                                absolute,
                                checkpoint.clone(),
                                cx,
                            )
                        });
                    },
                )))
            });
        let key = format!("rewound:{}", entry.id);
        let expanded = self.expanded.contains(&key);
        let on_toggle: components::rewind::Handler = Rc::new(move |_, cx| {
            let _ = this.update(cx, |timeline, cx| timeline.toggle_expanded(turn, &key, cx));
        });
        components::rewind::rewound_divider(
            entry,
            expanded,
            on_toggle,
            undo,
            harness,
            &self.motion,
            cx,
        )
    }

    /// The message's rewind: its action opening the dialog, or what shows
    /// while the rewind is made.
    fn compose_rewind(
        &self,
        menu: &components::bubble::RewindMenu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if menu.in_progress {
            return components::bubble::rewind_in_progress(&menu.key, cx);
        }
        let compact = self.window_state.read(cx).compact;
        let (key, turn, steer) = (menu.key.clone(), menu.turn, menu.steer.clone());
        components::bubble::rewind_button(menu, compact, &self.motion, cx)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_rewind(key.clone(), turn, steer.clone(), window, cx);
            }))
            .into_any_element()
    }

    pub(in crate::chat) fn open_diff(
        &mut self,
        turn: usize,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        if let Some(lead) = self.side_lead() {
            lead.update(cx, |lead, cx| {
                lead.show_agent_in_drawer(session_id.clone(), cx)
            });
        }
        self.workspace_store
            .update(cx, |store, cx| store.open_diff(&session_id, turn, path, cx));
        if self.window_state.read(cx).compact {
            self.window_state
                .update(cx, |_, cx| cx.emit(OpenPanel(NavigationPanel::Diff)));
        }
    }

    pub(in crate::chat) fn on_open_file_diff(
        &mut self,
        action: &OpenFileDiff,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let OpenFileDiff { turn, path } = action.clone();
        self.open_diff(turn, Some(path), cx);
    }

    fn compose_user(
        &self,
        args: components::bubble::UserMessageArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (turn, entry_id, visible, cwd, attachments, steering, pinned) = args;
        if self
            .workspace_store
            .read(cx)
            .delivery_messages_for(self.session_id.as_deref(), cx)
            .iter()
            .any(|(key, _, _, _)| {
                entry_id == format!("local-user-{key}") || entry_id == format!("local-steer-{key}")
            })
        {
            return div().into_any_element();
        }

        // A steer is a target only once its harness has a restore point for it.
        let steer = matches!(steering, Some(SteeringStatus::Accepted)).then(|| entry_id.to_owned());
        let rewind_target = (steering.is_none() || steer.is_some())
            .then(|| {
                self.workspace_store.read(cx).chat_rewind_target_for(
                    self.session_id.as_deref(),
                    turn,
                    steer.as_deref(),
                    cx,
                )
            })
            .flatten();
        // What the last rewind to the message failed at stands in its action
        // row until a message is sent, while no other is being made.
        let rewind_failure = rewind_target
            .as_ref()
            .filter(|target| !target.in_progress)
            .and_then(|target| self.rewind_failure_label(&target.checkpoint_id, false, cx))
            .map(|label| components::notices::rewind_failure(entry_id, label, cx));
        let rewind_menu = rewind_target.map(|target| components::bubble::RewindMenu {
            key: entry_id.to_owned(),
            turn,
            steer,
            blocked: target
                .rewind
                .blocked
                .filter(|_| !target.in_progress)
                .map(|blocked| components::rewind::blocked_reason(blocked, &target.harness)),
            in_progress: target.in_progress,
        });
        let rewind = rewind_menu.map(|menu| {
            let action = self.compose_rewind(&menu, cx);
            (menu, action)
        });
        let tags = self.message_tags_of(turn, entry_id, cx);
        let tags =
            (!tags.is_empty()).then(|| components::notices::message_tags(entry_id, &tags, cx));
        let markdown = self.md_states.get(entry_id).map(|md| md.state.clone());
        let copy_key = format!("user:{entry_id}");
        let copied = self.copied.is(&copy_key);
        let mark = copy_key;
        let copy_text: Arc<str> = Arc::from(visible);
        let image_host = self.workspace_store.read(cx).image_host();
        let images = attachments
            .iter()
            .filter(|path| components::bubble::is_image_path(path))
            .map(|path| {
                let path = PathBuf::from(path);
                let title = path
                    .file_name()
                    .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
                    .into_owned();
                Box::new(cx.listener(move |_, _, window, cx| {
                    crate::image_viewer::open(
                        crate::image_viewer::ViewerSource::original(
                            image_host,
                            tcode_protocol::OriginalTarget::File { path: path.clone() },
                            crate::store::image_preview(image_host, path.clone(), 120., true),
                        ),
                        title.clone(),
                        window,
                        cx,
                    );
                })) as components::bubble::ClickHandler
            })
            .collect();
        // A steer is one message from the moment it is sent: pending in the
        // turn's trailer, then taken where the agent took it.
        let steer = steering.map(|steering| {
            let key = SharedString::from(format!("steer-{entry_id}"));
            let site = SharedString::new_static(match steering {
                SteeringStatus::Pending => "trailer",
                SteeringStatus::Accepted => "flow",
            });
            let transit = self.shared.arrive(&key, &site, cx);
            (key, transit)
        });
        let taken = steer
            .as_ref()
            .and_then(|(_, transit)| transit.as_ref())
            .filter(|transit| transit.from == "trailer")
            .map(|transit| transit.progress);
        let bubble = components::bubble::user_bubble(
            components::bubble::BubbleData {
                image_host,
                entry_id,
                taken,
                visible,
                cwd,
                attachments,
                steering,
                compact: self.window_state.read(cx).compact,
                pinned,
                copied,
                markdown,
                rewind,
                rewind_failure,
                tags,
                hover: self.hover.clone(),
                motion: self.motion.clone(),
            },
            components::bubble::BubbleHandlers {
                copy: Box::new(cx.listener(move |this, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                    this.mark_copied(mark.clone(), cx);
                })),
                images,
            },
            window,
            cx,
        );
        let bubble = match steer {
            Some((key, _)) => self.shared.element(key, bubble).into_any_element(),
            None => bubble,
        };

        if let Some(author) = self.workspace_store.read(cx).message_byline_for(
            self.session_id.as_deref(),
            entry_id,
            cx,
        ) {
            // Inset by the bubble's own padding, so the name lines up with the
            // message text rather than the bubble's rounded corner.
            v_flex()
                .w_full()
                .items_end()
                .gap(px(2.))
                .child(
                    div()
                        .max_w_3_4()
                        .px(cx.theme().tokens.spacing.md)
                        .text_secondary(false, cx)
                        .overflow_hidden()
                        .child(crate::widgets::fade_text(author)),
                )
                .child(bubble)
                .into_any_element()
        } else {
            bubble
        }
    }

    /// A visualization: the page its call asked for, once the timeline
    /// holds it.
    fn compose_visualize(&mut self, entry: &TimelineEntry, cx: &mut Context<Self>) -> AnyElement {
        use components::visualize::{FileRead, Shown};
        let Some(visualization) = tools::visualize::card(entry) else {
            unreachable!();
        };
        let page = |visual: &components::visualize::Visual| Shown::Page {
            state: visual.state.clone(),
            html: visual.html.clone(),
        };
        let shown = match (self.visuals.get(&entry.id), &visualization.page) {
            (None, _) => Shown::Building,
            (Some(_), tools::visualize::Page::Inline(html))
                if visualization.running && html.trim().is_empty() =>
            {
                Shown::Building
            }
            (Some(visual), tools::visualize::Page::Inline(_)) => page(visual),
            (Some(visual), tools::visualize::Page::File(path)) => match &visual.file {
                None | Some(FileRead::Reading { .. }) => Shown::Building,
                Some(FileRead::Read) => page(visual),
                Some(FileRead::Failed(why)) => Shown::Unreadable(why.message(path)),
            },
        };
        components::visualize::visualization(&entry.id, shown, self.visual_width(), cx)
    }

    /// How wide a visualization's page is drawn: as a reply's text is, the
    /// body within its margins, up to the column's width.
    pub(super) fn visual_width(&self) -> Option<f32> {
        let body = f32::from(self.layout.width);
        (body > 0.0).then(|| {
            (body - 2. * crate::material::CHAT_CONTENT_MIN_PADDING)
                .min(crate::material::CHAT_CONTENT_MAX_WIDTH)
        })
    }

    /// A proposed plan's block, carrying the decision its agent waits on.
    fn compose_plan(
        &self,
        entry: &TimelineEntry,
        cwd: &Path,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let EntryContent::Item(ItemContent::Plan { markdown: text }) = &entry.content else {
            unreachable!();
        };
        let (markdown, copy_text) = self.md_states.get(&entry.id).map_or_else(
            || (None, Arc::from(text.as_str())),
            |md| (Some(md.state.clone()), md.synced.clone()),
        );
        let store = self.workspace_store.read(cx);
        let request = self.pending_plan_request(&entry.id, cx);
        let enabled = request.as_ref().is_some_and(|request| {
            !store.approval_delivery_pending_for(self.session_id.as_deref(), &request.id)
                && !store.native_subagent_readonly_for(self.session_id.as_deref(), cx)
                && store.connection_state().is_connected()
        });
        let session_id = self.session_id.clone();
        let store_entity = self.workspace_store.clone();
        let request_id = request.as_ref().map(|request| request.id.clone());
        let on_choose: components::plan::ChooseOption = Rc::new(move |option, _, cx| {
            let (Some(session_id), Some(request_id)) = (session_id.clone(), request_id.clone())
            else {
                return;
            };
            store_entity.update(cx, |store, _| {
                store.respond_approval_for(
                    session_id,
                    request_id,
                    agent::ApprovalDecision::Option(option.to_owned()),
                )
            });
        });
        let copy_key = format!("plan:{}", entry.id);
        let copied = self.copied.is(&copy_key);
        components::plan::plan_block(
            components::plan::PlanData {
                image_host: self.workspace_store.read(cx).image_host(),
                id: &entry.id,
                text,
                copy_text: copy_text.clone(),
                cwd,
                markdown,
                compact: self.window_state.read(cx).compact,
                copied,
                decision: request
                    .as_ref()
                    .map(|request| components::plan::PlanDecision {
                        options: &request.options,
                        enabled,
                        on_choose,
                    }),
                motion: self.motion.clone(),
            },
            cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.to_string()));
                this.mark_copied(copy_key.clone(), cx);
            }),
            cx,
        )
    }

    pub(in crate::chat) fn sync_deciding_plan(&mut self, cx: &mut Context<Self>) {
        let deciding = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)
            .and_then(|status| {
                status
                    .pending_approvals
                    .iter()
                    .find_map(|request| match &request.kind {
                        agent::ApprovalKind::Plan { item_id, .. } => Some(item_id.clone()),
                        _ => None,
                    })
            });
        if deciding == self.deciding_plan {
            return;
        }
        let previous = std::mem::replace(&mut self.deciding_plan, deciding);
        for id in previous.iter().chain(&self.deciding_plan) {
            if let Some(row) = self
                .markdown_row(id, cx)
                .filter(|row| *row < self.rows.len())
            {
                self.list_state.remeasure_items(row..row + 1);
            }
        }
    }

    /// The plan decision this thread's agent waits on for plan item `item_id`.
    fn pending_plan_request(&self, item_id: &str, cx: &App) -> Option<agent::ApprovalRequest> {
        self.workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)?
            .pending_approvals
            .iter()
            .find(|request| {
                matches!(&request.kind, agent::ApprovalKind::Plan { item_id: id, .. } if id == item_id)
            })
            .cloned()
    }

    /// A user-role entry, sent or still steering: the notice the host wrote,
    /// or else the user's bubble.
    fn compose_user_entry(
        &self,
        (turn, entry, cwd, pinned): (usize, &TimelineEntry, &Path, bool),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (text, steering, attachments) = user_content(&entry.content).expect("a user entry");
        let Some(notice) = user_notice(&entry.content) else {
            return self.compose_user(
                (turn, &entry.id, text, cwd, attachments, steering, pinned),
                window,
                cx,
            );
        };
        // A notice is the host's, never the user's words: a card, or the
        // message coming in, without an action row.
        let pending = steering == Some(SteeringStatus::Pending);
        let mut column = v_flex().w_full().gap_2();
        let mut keys = notice_keys(&notice, &entry.id).into_iter();
        let mut key = || keys.next().expect("a key per notice row");
        match &notice {
            HostNotice::Orchestrate(callbacks) => {
                for callback in callbacks {
                    column = column.child(self.compose_callback_card(
                        turn,
                        key(),
                        pending,
                        callback,
                        window,
                        cx,
                    ));
                }
            }
            HostNotice::PullRequest { number, notice } => {
                column = column.child(self.compose_pull_request_update(
                    (turn, key(), pending),
                    (*number, notice),
                    text,
                    cx,
                ));
            }
            HostNotice::ReportReminder | HostNotice::Control(_) => {
                column = column.child(self.compose_host_message(
                    (entry, text, cwd, steering),
                    &notice,
                    window,
                    cx,
                ));
            }
        }
        column.into_any_element()
    }

    fn compose_callback_card(
        &self,
        turn: usize,
        key: String,
        steering: bool,
        callback: &OrchestrateCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let request = self.callback_request(turn, &key, callback, window, cx);
        let expanded = self.expanded.contains(&key);
        let child_id = callback.child_id.clone();
        let (glyph, detail) = {
            let store = self.workspace_store.read(cx);
            let meta = store.thread_meta(&child_id).or_else(|| {
                store
                    .archived_sessions()
                    .iter()
                    .find(|meta| meta.id == child_id)
            });
            let glyph = meta.map(|meta| {
                crate::provider_card::profile_glyph(
                    store,
                    meta.provider,
                    meta.profile_id.as_deref(),
                )
            });
            let detail = meta.map(|meta| {
                std::iter::once(meta.provider.display_name().to_owned())
                    .chain(meta.model.as_deref().map(|model| {
                        store
                            .thread_model_name(&child_id, model)
                            .unwrap_or_else(|| model.to_owned())
                    }))
                    .collect::<Vec<_>>()
                    .join(" · ")
            });
            (glyph, detail)
        };
        let exists = glyph.is_some();
        let store = self.workspace_store.clone();
        let host = self.agent_host(cx);
        // The agent opens out of the card that opens it.
        let origin = SharedString::from(format!("callback-{key}"));
        let on_open = exists.then(|| {
            let origin = origin.clone();
            Box::new(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                crate::agents_panel::open_agent(
                    &store,
                    child_id.clone(),
                    host.clone(),
                    Some(origin.clone()),
                    window,
                    cx,
                );
            }) as components::changed_files::ClickHandler
        });
        let toggle_key = key.clone();
        let card = components::callback_card::callback_card(
            &key,
            callback,
            components::callback_card::CallbackChild {
                glyph,
                detail,
                on_open,
            },
            request,
            steering,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            self.hover.clone(),
            &self.motion,
        );
        if exists {
            self.side_origins
                .mark(origin, px(crate::material::event_card::RADIUS), card)
                .into_any_element()
        } else {
            card
        }
    }

    /// Where the request `callback` carries stands: how the lead's log says
    /// it was settled, else the child waits on it in the lead's status, which
    /// the card then answers, else it is no longer pending. `None` for an
    /// ending.
    fn callback_request(
        &self,
        turn: usize,
        key: &str,
        callback: &OrchestrateCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<components::callback_card::CallbackRequest> {
        use components::callback_card::{CallbackRequest, QuestionForm};
        let request_id = callback.request_id()?.to_owned();
        let child_id = callback.child_id.clone();
        let store = self.workspace_store.read(cx);
        let settled = store
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    timeline
                        .child_request_answers
                        .get(&(child_id.clone(), request_id.clone()))
                        .cloned()
                },
                cx,
            )
            .flatten();
        if let Some(answer) = settled {
            return Some(CallbackRequest::Settled(
                components::callback_card::settled_lines(&answer, callback),
            ));
        }
        let Some(status) = store.session_status_for(self.session_id.as_deref(), cx) else {
            return Some(CallbackRequest::Unknown);
        };
        let waiting = status
            .child_requests
            .iter()
            .find(|waiting| waiting.child_id == child_id);
        let online = !matches!(store.connection_state(),
            tcode_client::ConnectionState::Offline { reason } if reason.is_terminal());
        Some(match callback.event {
            CallbackEvent::Approval => {
                let Some(request) = waiting
                    .and_then(|waiting| waiting.approvals.iter().find(|r| r.id == request_id))
                    .cloned()
                else {
                    return Some(CallbackRequest::Gone);
                };
                let enabled =
                    online && !store.approval_delivery_pending_for(Some(&child_id), &request.id);
                let answerable = crate::composer::approval_answerable(&request, store);
                let store = self.workspace_store.clone();
                CallbackRequest::Approval {
                    request,
                    answerable,
                    enabled,
                    decide: Rc::new(move |request_id, decision, _, cx| {
                        store.update(cx, |store, _| {
                            store.respond_approval_for(child_id.clone(), request_id, decision)
                        });
                    }),
                }
            }
            CallbackEvent::Question => {
                let Some(pending) = waiting
                    .and_then(|waiting| waiting.user_input.as_ref())
                    .filter(|pending| pending.request_id == request_id)
                    .cloned()
                else {
                    return Some(CallbackRequest::Gone);
                };
                let enabled =
                    online && !store.user_input_delivery_pending_for(&child_id, &request_id);
                let (index, selections, input) =
                    self.callback_draft(turn, key, &child_id, &pending, window, cx);
                let act = cx.listener({
                    let key = key.to_owned();
                    move |this, action: &components::callback_card::QuestionAction, window, cx| {
                        this.act_on_callback_question(
                            turn, &key, &child_id, &pending, action, window, cx,
                        );
                    }
                });
                CallbackRequest::Question(QuestionForm {
                    questions: self
                        .callback_drafts
                        .borrow()
                        .get(key)
                        .map(|draft| draft.questions.clone())
                        .unwrap_or_default(),
                    index,
                    selections,
                    input,
                    enabled,
                    act: Rc::new(move |action, window, cx| act(&action, window, cx)),
                })
            }
            CallbackEvent::Ended(_) => return None,
        })
    }

    /// The answers chosen so far to the question card `key` shows, begun for
    /// `pending` when the card first shows it.
    fn callback_draft(
        &self,
        turn: usize,
        key: &str,
        child_id: &str,
        pending: &tcode_core::session::PendingUserInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (usize, HashMap<String, Vec<String>>, Entity<TextareaState>) {
        let mut drafts = self.callback_drafts.borrow_mut();
        if let Some(draft) = drafts
            .get(key)
            .filter(|draft| draft.request_id == pending.request_id)
        {
            return (draft.index, draft.selections.clone(), draft.input.clone());
        }
        let prefill = pending
            .questions
            .first()
            .and_then(|question| question.prefill.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .input_action(gpui::TextInputAction::Enter)
                .auto_grow(1, 6)
                .submit_on_enter(true)
                .default_value(prefill)
        });
        let events = cx.subscribe_in(&input, window, {
            let key = key.to_owned();
            let child_id = child_id.to_owned();
            let pending = pending.clone();
            move |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } => this.act_on_callback_question(
                    turn,
                    &key,
                    &child_id,
                    &pending,
                    &components::callback_card::QuestionAction::SendTyped,
                    window,
                    cx,
                ),
                // A line typed or wrapped grows the card.
                InputEvent::Change => {
                    this.remeasure_turn(turn);
                    cx.notify();
                }
                _ => {}
            }
        });
        drafts.insert(
            key.to_owned(),
            CallbackDraft {
                child_id: child_id.to_owned(),
                request_id: pending.request_id.clone(),
                questions: pending.questions.clone(),
                index: 0,
                selections: HashMap::new(),
                input: input.clone(),
                _events: events,
            },
        );
        (0, HashMap::new(), input)
    }

    /// A step taken in the question card `key`, by the composer's rules:
    /// a single-select choice or a typed answer moves on, the set goes to
    /// the child once every question is answered.
    #[allow(clippy::too_many_arguments)]
    fn act_on_callback_question(
        &mut self,
        turn: usize,
        key: &str,
        child_id: &str,
        pending: &tcode_core::session::PendingUserInput,
        action: &components::callback_card::QuestionAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::composer::model::{
            UserInputStep, record_typed_user_input, submit_typed_user_input,
            toggle_user_input_option, user_input_step,
        };
        use components::callback_card::QuestionAction;
        let mut drafts = self.callback_drafts.borrow_mut();
        let Some(draft) = drafts
            .get_mut(key)
            .filter(|draft| draft.request_id == pending.request_id)
        else {
            return;
        };
        let questions = &pending.questions;
        let at = draft.index.min(questions.len().saturating_sub(1));
        let Some(question) = questions.get(at) else {
            return;
        };
        let typed = draft.input.read(cx).value().to_string();
        let moves_on = match action {
            QuestionAction::Choose(label) => {
                toggle_user_input_option(&mut draft.selections, question, label.clone());
                !question.multi_select
            }
            QuestionAction::Go(delta) => {
                draft.index = at
                    .saturating_add_signed(*delta)
                    .min(questions.len().saturating_sub(1));
                false
            }
            QuestionAction::SendTyped => {
                submit_typed_user_input(&mut draft.selections, question, &typed)
            }
            QuestionAction::Confirm => {
                // What is typed and not yet sent is part of the answer.
                if !typed.trim().is_empty() {
                    record_typed_user_input(&mut draft.selections, question, typed.trim().into());
                }
                true
            }
        };
        let prefill = |index: usize| {
            questions
                .get(index)
                .and_then(|question| question.prefill.clone())
                .unwrap_or_default()
        };
        let mut answers = None;
        let mut value = (draft.index != at).then(|| prefill(draft.index));
        match moves_on
            .then(|| user_input_step(questions, &draft.selections, at))
            .flatten()
        {
            Some(UserInputStep::Send(set)) => {
                answers = Some(set);
                draft.selections.clear();
                draft.index = 0;
                value = Some(String::new());
            }
            Some(UserInputStep::Question(next)) => {
                draft.index = next;
                value = Some(prefill(next));
            }
            None => {}
        }
        let input = draft.input.clone();
        drop(drafts);
        if let Some(value) = value {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        if let Some(answers) = answers {
            self.workspace_store.update(cx, |store, _| {
                store.respond_user_input_for(
                    child_id.to_owned(),
                    pending.request_id.clone(),
                    answers,
                )
            });
        }
        self.remeasure_turn(turn);
        cx.notify();
    }

    /// The row signature sees neither the lead's status nor what its log
    /// says of a child's request, so when either changes, the turns that
    /// hold a child's callback are measured again, and the answers begun for
    /// a request no longer pending go.
    pub(in crate::chat) fn sync_callback_requests(&mut self, cx: &mut Context<Self>) {
        let store = self.workspace_store.read(cx);
        let status = store.session_status_for(self.session_id.as_deref(), cx);
        let live: Vec<(String, String)> = status
            .into_iter()
            .flat_map(|status| &status.child_requests)
            .flat_map(|waiting| {
                waiting
                    .approvals
                    .iter()
                    .map(|request| request.id.clone())
                    .chain(
                        waiting
                            .user_input
                            .iter()
                            .map(|pending| pending.request_id.clone()),
                    )
                    .map(|id| (waiting.child_id.clone(), id))
            })
            .collect();
        let answered = store
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| timeline.child_request_answers.len(),
                cx,
            )
            .unwrap_or_default();
        let signature = (status.is_some(), live, answered);
        if signature == self.callback_requests {
            return;
        }
        let turns: std::collections::BTreeSet<usize> = store
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| {
                    timeline
                        .entries
                        .iter()
                        .filter(|entry| {
                            matches!(
                                user_notice(&entry.content),
                                Some(HostNotice::Orchestrate(_))
                            )
                        })
                        .map(|entry| entry.turn)
                        .collect()
                },
                cx,
            )
            .unwrap_or_default();
        let live = &signature.1;
        self.callback_drafts.borrow_mut().retain(|_, draft| {
            live.iter()
                .any(|(child, request)| *child == draft.child_id && *request == draft.request_id)
        });
        self.callback_requests = signature;
        for turn in turns {
            self.remeasure_turn(turn);
        }
    }

    /// Prepare one stateless Work Log capsule, or the part of an expanded one
    /// that `part` names.
    fn compose_work_log(
        &mut self,
        args: components::work_log::WorkLogArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (index, part, turn, cwd, activities, is_last) = args;
        let segment_id = activities[0].id.as_str();
        let section_key = work_log_key(segment_id);
        let running = is_last && turn.running;
        let live_reasoning_id = running
            .then(|| activities.last().copied())
            .flatten()
            .filter(|entry| {
                matches!(
                    entry.content,
                    EntryContent::Item(ItemContent::Reasoning { .. })
                )
            })
            .map(|entry| entry.id.as_str());
        match part {
            RowPart::WorkLogActivity => {
                let rows = self.compose_work_log_rows(activities, cwd, None, false, window, cx);
                return components::work_log::work_log_body(rows).into_any_element();
            }
            RowPart::WorkLogLive => {
                return self.compose_live_window(
                    (index, segment_id),
                    (activities, activities),
                    cwd,
                    live_reasoning_id,
                    running,
                    window,
                    cx,
                );
            }
            RowPart::Segment | RowPart::WorkLogHeader { .. } => {}
        }
        let header_only = matches!(part, RowPart::WorkLogHeader { .. });
        let (folded, visible) = partition_activity_run(activities, running);
        let expanded = self.expanded.contains(&section_key);

        let mut flow = v_flex().w_full().gap_1();
        if !folded.is_empty() {
            let segment_counts = work_log_counts(folded);
            let mut capsule_label = work_log_capsule_label(&segment_counts, folded.len());
            if capsule_label.is_empty() {
                capsule_label = crate::tr!("chat.work_log").into_owned();
            }
            let duration = activity_run_duration_ms(folded).map(format_elapsed);
            let mut outcome = work_log_outcome(turn, is_last);
            // The turn was stopped, but what a call of this run started goes
            // on in the background, as its row says: the run is not over.
            if outcome == agent::TurnStatus::Interrupted
                && folded.iter().any(|entry| {
                    let status = tools::activity(entry, false, cwd).status;
                    self.runs_in_background(entry, status, cx)
                })
            {
                outcome = agent::TurnStatus::Completed;
            }
            let rows = if expanded && !header_only {
                self.compose_work_log_rows(folded, cwd, live_reasoning_id, false, window, cx)
            } else {
                Vec::new()
            };

            let toggle_section_key = section_key;
            flow = flow.child(components::work_log::work_log(
                components::work_log::WorkLogData {
                    compact: self.window_state.read(cx).compact,
                    index,
                    segment_id: segment_id.to_string(),
                    capsule_label,
                    duration,
                    outcome,
                    expanded,
                    running,
                    rows,
                    hover: self.hover.clone(),
                    motion: self.motion.clone(),
                },
                cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(index, &toggle_section_key, cx);
                }),
                cx,
            ));
        }

        if !visible.is_empty() && !header_only {
            // The activities a collapse folds away stood between the header
            // and the live window.
            if !folded.is_empty() {
                flow = flow.children(std::mem::take(&mut self.row_folds));
            }
            flow = flow.child(self.compose_live_window(
                (index, segment_id),
                (activities, visible),
                cwd,
                live_reasoning_id,
                running,
                window,
                cx,
            ));
        }

        flow.into_any_element()
    }

    fn compose_work_log_rows(
        &mut self,
        activities: &[&TimelineEntry],
        cwd: &Path,
        live_reasoning_id: Option<&str>,
        auto_expand: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        let windows = tools::run::windows(activities);
        let newer = self.successors(
            activities,
            &windows,
            cwd,
            (live_reasoning_id, auto_expand),
            cx,
        );
        for (range, newer) in windows.into_iter().zip(newer) {
            rows.extend(self.compose_activity_window(
                &activities[range],
                newer,
                cwd,
                (live_reasoning_id, auto_expand),
                window,
                cx,
            ));
        }
        rows
    }

    /// A live run's newest activities, one window each. As the run moves
    /// on, the oldest folds away into the Work Log's header, rebuilt from
    /// the run (`run`, which holds it still) while it goes, and a new one
    /// folds in.
    #[allow(clippy::too_many_arguments)]
    fn compose_live_window(
        &mut self,
        (index, segment_id): (usize, &str),
        (run, visible): (&[&TimelineEntry], &[&TimelineEntry]),
        cwd: &Path,
        live_reasoning_id: Option<&str>,
        auto_expand: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let windows = tools::run::windows(visible);
        let newer = self.successors(visible, &windows, cwd, (live_reasoning_id, auto_expand), cx);
        let items = windows.into_iter().zip(newer).map(|(range, newer)| {
            let ids: Vec<String> = visible[range]
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            (SharedString::from(ids[0].clone()), (ids, newer))
        });
        let gap = gpui::rems(0.25).to_pixels(window.rem_size());
        crate::motion::keyed::keyed_owned(
            &self.motion.clone(),
            format!("live-window-{index}-{segment_id}"),
            crate::motion::keyed::Fold::Down,
            items,
            ROWS,
            window,
            cx,
            |_, (ids, newer), window, cx| {
                let entries: Vec<&TimelineEntry> = ids
                    .iter()
                    .filter_map(|id| run.iter().find(|entry| entry.id == *id).copied())
                    .collect();
                if entries.is_empty() {
                    // It left the run: only its place folds away.
                    return div().into_any_element();
                }
                let rows = self.compose_activity_window(
                    &entries,
                    *newer,
                    cwd,
                    (live_reasoning_id, auto_expand),
                    window,
                    cx,
                );
                v_flex()
                    .w_full()
                    .gap_1()
                    .pb(gap)
                    .children(rows)
                    .into_any_element()
            },
        )
        // Each window brings its gap below it; the last one's is not the
        // list's.
        .render(v_flex().w_full().mb(-gap))
    }

    /// One window of a Work Log: a computer run, an edit's file windows, the
    /// one window of the files a command changed, or one activity, with
    /// `newer` windows after it in its list that have taken its place.
    fn compose_activity_window(
        &mut self,
        activities: &[&TimelineEntry],
        newer: usize,
        cwd: &Path,
        (live_reasoning_id, auto_expand): (Option<&str>, bool),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        if tools::run::is_run(activities, &(0..activities.len())) {
            return vec![self.compose_computer_run(activities, cwd, live_reasoning_id, window, cx)];
        }
        let entry = activities[0];
        let recency = AutoActivityRecency::from_newer_activity_count(newer);
        if let Some(edit) = tools::diff::edit(entry, cwd) {
            if edit.by_command && edit.files.len() > 1 {
                return vec![self.compose_changed_files(entry, cwd, &edit, recency, window, cx)];
            }
            return self.compose_edit_rows(entry, cwd, &edit, auto_expand, recency, window, cx);
        }
        let row = self.compose_activity_row(
            (entry, cwd),
            ActivityRow {
                compact: false,
                live_reasoning: live_reasoning_id == Some(entry.id.as_str()),
                auto_expand,
                recency,
                images_inline: true,
            },
            window,
            cx,
        );
        vec![self.shared_call(entry, "run", row, cx)]
    }

    /// A tool call's window, which leaves its run for the turn's images
    /// once the call completes with the pictures it read, and travels there.
    fn shared_call(
        &self,
        entry: &TimelineEntry,
        site: &'static str,
        row: AnyElement,
        cx: &App,
    ) -> AnyElement {
        if !matches!(
            entry.content,
            EntryContent::Item(ItemContent::ToolCall { .. })
        ) {
            return row;
        }
        let key = SharedString::from(format!("activity-{}", entry.id));
        self.shared
            .arrive(&key, &SharedString::new_static(site), cx);
        self.shared.element(key, row).into_any_element()
    }

    /// The one window of the files a command changed, which opens on a
    /// window per file.
    fn compose_changed_files(
        &mut self,
        entry: &TimelineEntry,
        cwd: &Path,
        edit: &tools::diff::Edit,
        recency: AutoActivityRecency,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("activity-{}", entry.id);
        let turn = entry.turn;
        let expanded = self.auto_activity_expanded((turn, &entry.id), &key, false, recency, cx);
        let detail = match tools::window::fold(&self.motion, &entry.id, expanded, window, cx) {
            Some(shown) => {
                let files = self.compose_edit_rows(entry, cwd, edit, false, recency, window, cx);
                tools::window::Detail::Fold(
                    shown,
                    v_flex().w_full().gap_1().children(files).into_any_element(),
                    self.activity_body_key(entry, edit.status),
                )
            }
            None => tools::window::Detail::Shut,
        };
        let entry_id = entry.id.clone();
        tools::window::tool_window(
            tools::window::WindowSpec {
                key: entry.id.clone(),
                header: tools::diff::files_header(edit),
                status: edit.status,
                error: None,
                elapsed_ms: None,
                compact: false,
                copy: Vec::new(),
                whole_output: None,
                path: None,
                hover: self.hover.clone(),
                motion: self.motion.clone(),
            },
            detail,
            cx.listener(move |this, _, _, cx| {
                this.toggle_activity_expanded((turn, &entry_id), &key, expanded, cx);
            }),
            cx,
        )
    }

    /// An edit's windows, one per file it touched. A harness's own diff
    /// opens by itself while it is the latest.
    #[allow(clippy::too_many_arguments)]
    fn compose_edit_rows(
        &mut self,
        entry: &TimelineEntry,
        cwd: &Path,
        edit: &tools::diff::Edit,
        auto_expand: bool,
        recency: AutoActivityRecency,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let turn = entry.turn;
        let mut rows = Vec::new();
        for (file_index, file) in edit.files.iter().enumerate() {
            let key = format!("activity-{}-file-{file_index}", entry.id);
            let enabled = file_opens_by_itself(file, auto_expand);
            let expanded =
                self.auto_activity_expanded((turn, &entry.id), &key, enabled, recency, cx);
            let window_key = format!("{}-file-{file_index}", entry.id);
            // What the harness gave back belongs to the edit, told once.
            let output = edit.output.filter(|_| file_index == 0);
            let detail = if file.row.counts.is_none() && output.is_none() {
                tools::window::Detail::None
            } else {
                match tools::window::fold(&self.motion, &window_key, expanded, window, cx) {
                    Some(shown) => {
                        let diff = file
                            .row
                            .counts
                            .is_some()
                            .then(|| self.inline_diffs.render(&key, file, cx));
                        let output = output.map(|output| {
                            tools::diff::edit_output(
                                entry,
                                output,
                                edit.status,
                                &mut tools::ToolCtx {
                                    chat: self,
                                    cx,
                                    cwd,
                                    images_inline: false,
                                },
                            )
                        });
                        tools::window::Detail::Fold(
                            shown,
                            tools::body(diff.into_iter().chain(output)),
                            0,
                        )
                    }
                    None => tools::window::Detail::Shut,
                }
            };
            let header = tools::diff::file_header(file, edit.status);
            // The edit's time belongs to it, told once.
            let elapsed_ms = (file_index == 0)
                .then(|| tools::window::elapsed(&header, entry))
                .flatten();
            let error = tools::window::failure_line(&header, edit.status, output);
            let mut copy = tools::diff::file_copy_items(file);
            if let Some(output) = output {
                tools::copy(&mut copy, "chat.copy_output", output);
            }
            let toggle_key = key.clone();
            let entry_id = entry.id.clone();
            rows.push(tools::window::tool_window(
                tools::window::WindowSpec {
                    key: window_key,
                    header,
                    status: edit.status,
                    error,
                    elapsed_ms,
                    compact: false,
                    copy,
                    whole_output: None,
                    path: Some((file.row.path.clone(), cwd.to_path_buf())),
                    hover: self.hover.clone(),
                    motion: self.motion.clone(),
                },
                detail,
                cx.listener(move |this, _, _, cx| {
                    this.toggle_activity_expanded((turn, &entry_id), &toggle_key, expanded, cx);
                }),
                cx,
            ));
        }
        rows
    }

    /// Whether the work `entry` started goes on in the background: the call
    /// returned to the model and is still running, or its harness lists its
    /// work as running there, whatever the call itself returned.
    fn runs_in_background(
        &self,
        entry: &TimelineEntry,
        status: agent::ItemStatus,
        cx: &App,
    ) -> bool {
        entry.background
            && (status == agent::ItemStatus::InProgress
                || self
                    .workspace_store
                    .read(cx)
                    .session_status_for(self.session_id.as_deref(), cx)
                    .is_some_and(|status| {
                        status
                            .background_tasks
                            .iter()
                            .any(|task| task.task.item_id.as_deref() == Some(entry.id.as_str()))
                    }))
    }

    /// One Work Log activity as its window: a header in the shared grammar,
    /// and its body while it is open or folding shut.
    pub(in crate::chat) fn compose_activity_row(
        &mut self,
        (entry, cwd): (&TimelineEntry, &Path),
        row: ActivityRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("activity-{}", entry.id);
        let turn = entry.turn;
        let auto_enabled = self.command_opens_by_itself(entry, row.auto_expand, cx);
        let expanded =
            self.auto_activity_expanded((turn, &entry.id), &key, auto_enabled, row.recency, cx);
        let activity = tools::activity(entry, row.live_reasoning, cwd);
        let error = activity.failure();
        let elapsed_ms = tools::window::elapsed(&activity.header, entry);
        let tools::Activity {
            mut header,
            mut status,
            mut copy,
            expandable,
            path,
            ..
        } = activity;
        // A running command the harness can move into the background, as
        // its own Ctrl+B does.
        let movable = self.moves_to_background(cx)
            && matches!(
                &entry.content,
                EntryContent::Item(ItemContent::CommandExecution { .. })
            );
        if self.runs_in_background(entry, status, cx) {
            status = agent::ItemStatus::InProgress;
            header.background = true;
        }
        if header.background {
            header.actions.push(self.background_link(&entry.id, cx));
        } else if movable && status == agent::ItemStatus::InProgress {
            header.actions.push(self.move_to_background(&entry.id));
        }
        // Of an output the record holds a preview of, Copy takes the whole.
        let elided = self
            .workspace_store
            .read(cx)
            .with_timeline_for(
                self.session_id.as_deref(),
                |timeline| timeline.elided_outputs.contains_key(&entry.id),
                cx,
            )
            .unwrap_or(false);
        let label = crate::tr!("chat.copy_output").into_owned();
        let whole_output = (elided && copy.iter().any(|(item, _)| *item == label)).then(|| {
            copy.retain(|(item, _)| *item != label);
            (
                label,
                super::super::CopyWholeOutput {
                    item_id: entry.id.clone(),
                    turn,
                },
            )
        });
        let detail = if !expandable {
            tools::window::Detail::None
        } else {
            match tools::window::fold(&self.motion, &entry.id, expanded, window, cx) {
                Some(shown) => {
                    let shows = self.activity_body_key(entry, status);
                    let body = self.activity_body(entry, cwd, row.images_inline, cx);
                    tools::window::Detail::Fold(shown, body, shows)
                }
                None => tools::window::Detail::Shut,
            }
        };
        let click_key = key;
        let entry_id = entry.id.clone();
        let on_click = cx.listener(move |this, _, _, cx| {
            this.toggle_activity_expanded((turn, &entry_id), &click_key, expanded, cx);
        });
        let tool = tools::window::tool_window(
            tools::window::WindowSpec {
                key: entry.id.clone(),
                header,
                status,
                error,
                elapsed_ms,
                compact: row.compact,
                copy,
                whole_output,
                path: path.map(|path| (path, cwd.to_path_buf())),
                hover: self.hover.clone(),
                motion: self.motion.clone(),
            },
            detail,
            on_click,
            cx,
        );
        self.ringed_when_located(
            &entry.id,
            crate::material::radius_chip(cx),
            tool,
            window,
            cx,
        )
    }

    /// `element`, the call `item_id`'s, ringed a while once a row of the
    /// background work that names it is pressed.
    fn ringed_when_located(
        &self,
        item_id: &str,
        radius: gpui::Pixels,
        element: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let located = self
            .motion
            .shown(
                format!("located-{item_id}"),
                (self.located.as_deref() == Some(item_id)).then_some(()),
                CHANGE,
                window,
                cx,
            )
            .map(|shown| shown.progress());
        match located {
            Some(progress) => div()
                .relative()
                .w_full()
                .child(element)
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(radius)
                        .border_2()
                        .border_color(cx.theme().primary.opacity(progress)),
                )
                .into_any_element(),
            None => element,
        }
    }

    /// Whether the thread's harness can move a running call into the
    /// background.
    fn moves_to_background(&self, cx: &App) -> bool {
        self.workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx)
            .is_some_and(|status| {
                status
                    .background_control
                    .is_some_and(|control| control.move_running)
            })
    }

    /// The way from a call's window to its row among the background work.
    fn background_link(&self, item_id: &str, cx: &mut Context<Self>) -> AnyElement {
        let item = item_id.to_owned();
        let selector = item.clone();
        let chat = self.chat.clone();
        h_flex()
            .id(SharedString::from(format!("background-link-{item_id}")))
            .debug_selector(move || format!("background-link-{selector}"))
            .flex_none()
            .gap_0p5()
            .items_center()
            .whitespace_nowrap()
            .cursor_pointer()
            .text_secondary(false, cx)
            .text_color(cx.theme().muted_foreground)
            .hover(|link| link.text_color(cx.theme().foreground))
            // Not a chevron, which would read as the window's own that
            // opens its body.
            .child(Icon::new(IconName::Layers).xsmall())
            .child(crate::widgets::fade_text(crate::tr!("background.in_list")))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let item = item.clone();
                let _ = chat.update(cx, |chat, cx| chat.reveal_background(item, cx));
            })
            .into_any_element()
    }

    /// Moves a running call into the background.
    fn move_to_background(&self, item_id: &str) -> AnyElement {
        let item = item_id.to_owned();
        let selector = item.clone();
        let store = self.workspace_store.clone();
        let session_id = self.session_id.clone();
        Button::new(SharedString::from(format!("move-to-background-{item_id}")))
            .motion(&self.motion)
            .debug_selector(move || format!("move-to-background-{selector}"))
            .ghost()
            .xsmall()
            // The window's header is the one stop the keyboard makes on it,
            // as for the way to its background row beside it.
            .focusable(false)
            .label(crate::tr!("background.move").into_owned())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                if let Some(session_id) = session_id.clone() {
                    store.update(cx, |store, _| {
                        store.move_to_background(session_id, item.clone())
                    });
                }
            })
            .into_any_element()
    }

    /// What an open activity window's body shows, as far as its height goes:
    /// whether the call is done, how much of its output this client holds,
    /// and which of its parts the reader opened.
    fn activity_body_key(&self, entry: &TimelineEntry, status: agent::ItemStatus) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        std::mem::discriminant(&status).hash(&mut hash);
        self.full_outputs
            .get(&entry.id)
            .map(std::mem::discriminant)
            .hash(&mut hash);
        let mut open: Vec<_> = self
            .expanded
            .iter()
            .filter(|key| key.contains(entry.id.as_str()))
            .collect();
        open.sort();
        open.hash(&mut hash);
        hash.finish()
    }

    /// The body of an open activity window.
    fn activity_body(
        &mut self,
        entry: &TimelineEntry,
        cwd: &Path,
        images_inline: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let EntryContent::Item(item) = &entry.content else {
            return div().into_any_element();
        };
        match item {
            ItemContent::CommandExecution {
                command,
                input,
                output,
                status,
                ..
            } => tools::shell::shell_body(
                tools::shell::ShellRun {
                    entry,
                    command,
                    input,
                    output,
                    running: *status == agent::ItemStatus::InProgress,
                },
                &mut tools::ToolCtx {
                    chat: self,
                    cx,
                    cwd,
                    images_inline,
                },
            ),
            ItemContent::ToolCall { .. } => {
                let call = tools::Call::of(entry).expect("a tool call");
                let view = tools::view(&call);
                let mut ctx = tools::ToolCtx {
                    chat: self,
                    cx,
                    cwd,
                    images_inline,
                };
                view.body(&call, &mut ctx)
            }
            ItemContent::Reasoning { text } => div()
                .w_full()
                .text_secondary(false, cx)
                .whitespace_normal()
                .child(text.clone())
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    /// How much of a shortened tool output the reader has, or `None` when the
    /// record carried it whole.
    pub(in crate::chat) fn elided_output(
        &mut self,
        item_id: &str,
        turn: usize,
        cx: &mut Context<Self>,
    ) -> Option<tools::ElidedOutput> {
        use tools::ElidedOutput;
        let full_bytes = self.workspace_store.read(cx).with_timeline_for(
            self.session_id.as_deref(),
            |timeline| timeline.elided_outputs.get(item_id).copied(),
            cx,
        )??;
        let load = || -> tools::ClickHandler {
            let item_id = item_id.to_owned();
            Box::new(cx.listener(move |this, _, _, cx| {
                this.load_full_output(item_id.clone(), turn, cx);
            }))
        };
        Some(match self.full_outputs.get(item_id) {
            None => ElidedOutput::Preview {
                full_bytes,
                on_load: load(),
            },
            Some(FullOutput::Loading { .. }) => ElidedOutput::Loading,
            Some(FullOutput::Loaded(output)) => ElidedOutput::Loaded(output.clone()),
            Some(FullOutput::Failed(error)) => ElidedOutput::Failed {
                error: error.clone(),
                on_load: load(),
            },
        })
    }

    /// The control of the record withheld from `row`, in `turn`.
    fn withheld_read(
        &mut self,
        row: u64,
        turn: usize,
        cx: &mut Context<Self>,
    ) -> components::notices::WithheldRead {
        use components::notices::WithheldRead;
        let load = || -> components::changed_files::ClickHandler {
            Box::new(cx.listener(move |this, _, _, cx| this.load_withheld(row, turn, cx)))
        };
        match self.withheld_loads.get(&row) {
            None => WithheldRead::Offered(load()),
            Some(WithheldLoad::Loading { .. }) => {
                WithheldRead::Loading(Box::new(cx.listener(move |this, _, _, cx| {
                    this.withheld_loads.remove(&row);
                    this.remeasure_turn(turn);
                    cx.notify();
                })))
            }
            Some(WithheldLoad::Failed(error)) => WithheldRead::Failed(error.clone(), load()),
        }
    }

    fn load_withheld(&mut self, row: u64, turn: usize, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_key.clone() else {
            return;
        };
        let read = self.workspace_store.update(cx, |store, cx| {
            store.read_withheld_record(session_id.clone(), row, cx)
        });
        let task = cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                if this.session_key.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                match result {
                    Ok(record) => {
                        this.withheld_loads.remove(&row);
                        this.workspace_store.update(cx, |store, cx| {
                            store.restore_withheld_record(&session_id, row, record, cx);
                        });
                    }
                    Err(error) => {
                        this.withheld_loads.insert(row, WithheldLoad::Failed(error));
                    }
                }
                this.remeasure_turn(turn);
                cx.notify();
            });
        });
        self.withheld_loads
            .insert(row, WithheldLoad::Loading { _request: task });
        self.remeasure_turn(turn);
        cx.notify();
    }

    fn load_full_output(&mut self, item_id: String, turn: usize, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_key.clone() else {
            return;
        };
        let request = self.workspace_store.update(cx, |store, cx| {
            store.read_item_output(session_id.clone(), item_id.clone(), cx)
        });
        let id = item_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |this, cx| {
                if this.session_key.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                let output = match result {
                    Ok(output) => {
                        if this.copy_when_loaded.as_deref() == Some(id.as_str()) {
                            this.copy_when_loaded = None;
                            cx.write_to_clipboard(ClipboardItem::new_string(output.clone()));
                        }
                        FullOutput::Loaded(output.into())
                    }
                    Err(error) => FullOutput::Failed(error),
                };
                this.remeasure_activity(turn, &id, cx);
                this.full_outputs.insert(id, output);
                cx.notify();
            });
        });
        self.remeasure_activity(turn, &item_id, cx);
        self.full_outputs
            .insert(item_id, FullOutput::Loading { _request: task });
        cx.notify();
    }

    /// Release the screens of panels no longer laid out and remeasure the
    /// rows whose panel changed height with a new screen.
    pub(in crate::chat) fn apply_command_panel_changes(
        &mut self,
        changes: tools::shell::PanelChanges,
        cx: &mut Context<Self>,
    ) {
        if !changes.unmounted.is_empty() {
            self.workspace_store.update(cx, |store, _| {
                for id in &changes.unmounted {
                    store.unwatch_command_output_for(self.session_id.as_deref(), id);
                }
            });
        }
        if changes.resized.is_empty() {
            return;
        }
        for (turn, id) in &changes.resized {
            self.remeasure_activity(*turn, id, cx);
        }
        cx.notify();
    }

    /// Show the "Copied!" confirmation on `key` for 2s; a second copy re-arms the timer.
    fn mark_copied(&mut self, key: String, cx: &mut Context<Self>) {
        self.copied.mark(key, |this| &mut this.copied, cx);
    }

    pub(in crate::chat) fn on_link_url(
        &mut self,
        action: &crate::pull_requests::LinkUrl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        let task = self.workspace_store.update(cx, |store, cx| {
            store.command(
                tcode_protocol::Command::LinkPullRequest {
                    session_id,
                    reference: action.0.clone(),
                },
                cx,
            )
        });
        cx.spawn_in(window, async move |_, cx| {
            let result = task.await;
            _ = cx.update(|window, cx| {
                window.push_notification(
                    match result {
                        Ok(tcode_protocol::CommandResponse::PullRequestLinked {
                            key,
                            already_linked,
                        }) => crate::overlay::Notification::info(
                            crate::tr!(
                                if already_linked {
                                    "pull_requests.already_linked"
                                } else {
                                    "pull_requests.linked_toast"
                                },
                                number = key.number.to_string()
                            )
                            .into_owned(),
                        ),
                        Ok(_) => return,
                        Err(error) => crate::overlay::Notification::error(
                            crate::tr!("pull_requests.link_failed", reason = error.message)
                                .into_owned(),
                        ),
                    },
                    cx,
                )
            });
        })
        .detach();
    }
}

fn file_opens_by_itself(file: &tools::diff::EditedFile, auto_expand: bool) -> bool {
    auto_expand && file.row.counts.is_some()
}

/// Whether `entry` is still going on: a call that has not returned, a
/// thought still streaming.
fn goes_on(entry: &TimelineEntry, live_reasoning_id: Option<&str>) -> bool {
    match &entry.content {
        EntryContent::Item(ItemContent::Reasoning { .. }) => {
            live_reasoning_id == Some(entry.id.as_str())
        }
        EntryContent::Item(
            ItemContent::CommandExecution { status, .. }
            | ItemContent::FileChange { status, .. }
            | ItemContent::ToolCall { status, .. },
        ) => *status == agent::ItemStatus::InProgress,
        _ => false,
    }
}

impl TimelineView {
    fn command_opens_by_itself(&self, entry: &TimelineEntry, auto_expand: bool, cx: &App) -> bool {
        auto_expand
            && matches!(
                &entry.content,
                EntryContent::Item(ItemContent::CommandExecution { .. })
            )
            && self.workspace_store.read(cx).live_command_panel()
    }

    /// For each of `windows`, how many of the windows after it have taken
    /// its place: those that ended, and those still going on that already
    /// show something of their own, as a running command's output or an
    /// edit's streamed diff. One still going on with nothing to show yet —
    /// an edit before its diff, a thought, a call not yet returned — leaves
    /// what is open before it open.
    fn successors(
        &self,
        activities: &[&TimelineEntry],
        windows: &[std::ops::Range<usize>],
        cwd: &Path,
        (live_reasoning_id, auto_expand): (Option<&str>, bool),
        cx: &App,
    ) -> Vec<usize> {
        let mut after = 0;
        let mut counts = vec![0; windows.len()];
        for (index, range) in windows.iter().enumerate().rev() {
            counts[index] = after;
            let window = &activities[range.clone()];
            let shows = if tools::run::is_run(activities, range) {
                false
            } else if let Some(edit) = tools::diff::edit(window[0], cwd) {
                !(edit.by_command && edit.files.len() > 1)
                    && edit
                        .files
                        .iter()
                        .any(|file| file_opens_by_itself(file, auto_expand))
            } else {
                self.command_opens_by_itself(window[0], auto_expand, cx)
            };
            if shows || !window.iter().any(|entry| goes_on(entry, live_reasoning_id)) {
                after += 1;
            }
        }
        counts
    }
}

pub(in crate::chat) mod thread_card;

#[cfg(test)]
mod tests;
