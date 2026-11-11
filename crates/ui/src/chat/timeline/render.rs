use super::*;
use crate::chat::tools;

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
                    let EntryContent::ProviderRelay {
                        from_provider,
                        to_provider,
                        prompt,
                        moved_from,
                        ..
                    } = &entry.content
                    else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::relay_divider(
                        &entry.id,
                        *from_provider,
                        *to_provider,
                        moved_from.as_deref(),
                        cx,
                    ));
                    if let Some(prompt) = prompt {
                        column = column.child(
                            self.compose_disclosure(
                                index,
                                format!("relay-handoff-{}", entry.id),
                                crate::tr!("chat.relay_handoff", to = to_provider.display_name())
                                    .into_owned()
                                    .into(),
                                prompt,
                                cx,
                            ),
                        );
                    }
                }
                Segment::ModelChange(entry) => {
                    let EntryContent::ModelChanged { from, to, reason } = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::model_change_divider(
                        &entry.id,
                        from.as_deref(),
                        to,
                        reason.as_deref(),
                        cx,
                    ));
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
                Segment::ContextWindowChanged(entry) => {
                    let EntryContent::ContextWindowChanged { window } = entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::context_window_changed_divider(
                        &entry.id, window, cx,
                    ));
                }
                Segment::SettingsKept(entry) => {
                    column =
                        column.child(components::dividers::settings_kept_divider(&entry.id, cx));
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
                Segment::ControlAction(entry) => {
                    let EntryContent::ControlAction(action) = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::control_action_divider(
                        &entry.id, action, cx,
                    ));
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
                Segment::Withheld(entry) => {
                    let EntryContent::Withheld { bytes } = entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::record_withheld_divider(
                        &entry.id, bytes, cx,
                    ));
                }
                Segment::Unreadable(rows) => {
                    let label = if rows.len() == 1 {
                        crate::tr!("chat.unreadable_record_one")
                    } else {
                        crate::tr!("chat.unreadable_records", count = rows.len())
                    };
                    let lines = rows
                        .iter()
                        .filter_map(|entry| match &entry.content {
                            EntryContent::Unreadable { line } => Some(line.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    column = column.child(self.compose_disclosure(
                        index,
                        format!("unreadable-{}", rows[0].id),
                        label.into_owned().into(),
                        &lines,
                        cx,
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
                            match limit_resume_action(
                                resets_at,
                                queued_id,
                                now_secs(cx),
                                ends_thread,
                                queue_empty,
                            ) {
                                LimitResumeAction::Scheduled { queued_id: id } => {
                                    Some(components::error_card::LimitResume::Scheduled {
                                        resets_at,
                                        on_cancel: Box::new(cx.listener(move |this, _, _, cx| {
                                            let session_id =
                                                this.session_id.clone().unwrap_or_default();
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
                                                let session_id =
                                                    this.session_id.clone().unwrap_or_default();
                                                this.workspace_store.update(cx, |store, _| {
                                                    store.schedule_turn_for(
                                                        session_id,
                                                        tcode_core::session::RESUME_PROMPT
                                                            .to_string(),
                                                        vec![],
                                                        resets_at,
                                                    )
                                                });
                                            },
                                        )),
                                    })
                                }
                                LimitResumeAction::ContinueNow => {
                                    Some(components::error_card::LimitResume::Continue {
                                        on_continue: Box::new(cx.listener(
                                            move |this, _, _, cx| {
                                                let session_id =
                                                    this.session_id.clone().unwrap_or_default();
                                                this.workspace_store.update(cx, |store, _| {
                                                    store.send_turn_for(
                                                        session_id,
                                                        tcode_core::session::RESUME_PROMPT
                                                            .to_string(),
                                                        vec![],
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
                                retry_at: retry.retry_at,
                                on_retry: Box::new(cx.listener(move |this, _, _, cx| {
                                    let session_id = this.session_id.clone().unwrap_or_default();
                                    this.workspace_store.update(cx, |store, _| {
                                        store.retry_turn_for(session_id, turn_id.clone())
                                    });
                                })),
                                on_cancel: Box::new(cx.listener(move |this, _, _, cx| {
                                    let session_id = this.session_id.clone().unwrap_or_default();
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
        if turn.running || finished_at.is_some() || ending.is_some() {
            let requested_model = self
                .workspace_store
                .read(cx)
                .chat_requested_model_for(self.session_id.as_deref(), cx);
            let served_model =
                divergent_served_model(turn.served_model.as_deref(), requested_model.as_deref())
                    .map(str::to_owned);
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
                                    turn.start_ts,
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
                            requested_model.as_deref(),
                            actions,
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
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let OpenRewind { key, turn, steer } = action.clone();
        self.open_rewind(key, turn, steer, cx);
    }

    pub(in crate::chat) fn on_fork_through_turn(
        &mut self,
        action: &ForkThroughTurn,
        cx: &mut Context<Self>,
    ) {
        self.fork_through_turn(action.turn, cx);
    }

    fn fork_through_turn(&mut self, turn: usize, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        self.workspace_store.update(cx, |store, cx| {
            let target = store
                .chat_fork_target_for(Some(&session_id), turn, cx)
                .filter(|target| !target.blocked);
            if let Some(target) = target {
                store.fork_through_turn(session_id, target.turn, target.checkpoint_id, cx);
            }
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
            on_fork: Rc::new(move |turn, cx| {
                let _ = this.update(cx, |timeline, cx| timeline.fork_through_turn(turn, cx));
            }),
        })
    }

    /// A rewind's divider, with its removed turns to read and the Harness's
    /// undo while it lasts, then the branch it left where the Harness keeps
    /// one; or a rewind the Harness did not make.
    fn compose_rewound(
        &self,
        turn: usize,
        entry: &TimelineEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.workspace_store.read(cx);
        let harness = store.harness_name_for(self.session_id.as_deref(), cx);
        let rewound = match &entry.content {
            EntryContent::Rewound(rewound) => rewound,
            EntryContent::RewindFailed { mode, failure } => {
                return components::dividers::rewind_failed_divider(
                    &entry.id, *mode, failure, &harness, cx,
                );
            }
            _ => unreachable!("a rewind segment holds a rewind entry"),
        };
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

    /// The message's rewind: its action opening the panel (a popover, a
    /// sheet on a phone), or what shows while the rewind is made.
    fn compose_rewind(
        &self,
        menu: &components::bubble::RewindMenu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if menu.in_progress {
            return components::bubble::rewind_in_progress(&menu.key, cx);
        }
        let compact = self.window_state.read(cx).compact;
        let trigger = components::bubble::rewind_button(menu, compact, &self.motion, cx);
        let panel = self.rewind_panel_for(&menu.key, cx);
        let open = panel.is_some();
        let actions = self.rewind_panel_actions(cx);
        let this = cx.entity().downgrade();
        let (key, turn, steer) = (menu.key.clone(), menu.turn, menu.steer.clone());
        crate::material::overlay_popover(SharedString::from(format!("rewind-panel-{key}")), cx)
            .anchor(Anchor::TopRight)
            .when(compact, |popover| {
                popover.bottom_sheet(crate::tr!("rewind.title"))
            })
            .open(open)
            .on_open_change(move |opened, _, cx| {
                let _ = this.update(cx, |timeline, cx| {
                    if *opened {
                        timeline.open_rewind(key.clone(), turn, steer.clone(), cx);
                    } else {
                        timeline.close_rewind(cx);
                    }
                });
            })
            .trigger(trigger)
            .content(move |_, _, cx| match panel {
                Some(panel) => components::rewind::panel_body(panel, actions, compact, cx),
                None => div().into_any_element(),
            })
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
        let rewind_menu = (steering.is_none() || steer.is_some())
            .then(|| {
                self.workspace_store.read(cx).chat_rewind_target_for(
                    self.session_id.as_deref(),
                    turn,
                    steer.as_deref(),
                    cx,
                )
            })
            .flatten()
            .map(|target| components::bubble::RewindMenu {
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
        let rewind = rewind_menu
            .as_ref()
            .map(|menu| self.compose_rewind(menu, cx));
        let markdown = self.md_states.get(entry_id).map(|md| md.state.clone());
        let copy_key = format!("user:{entry_id}");
        let copied = self.copied.is(&copy_key);
        let mark = copy_key;
        let copy_text: Arc<str> = Arc::from(visible);
        let image_host = self.workspace_store.read(cx).image_host();
        let images = attachments
            .iter()
            .map(|path| {
                let path = PathBuf::from(path);
                let title = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image".to_string());
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
                rewind_menu,
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

    /// A user-role entry, sent or still steering: the notice the host wrote,
    /// or else the user's bubble.
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
        let session_id = self.session_id.clone().unwrap_or_default();
        let store_entity = self.workspace_store.clone();
        let request_id = request.as_ref().map(|request| request.id.clone());
        let on_choose: components::plan::ChooseOption = Rc::new(move |option, _, cx| {
            let Some(request_id) = request_id.clone() else {
                return;
            };
            store_entity.update(cx, |store, _| {
                store.respond_approval_for(
                    session_id.clone(),
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
        // A notice is the host's, never the user's words: centered disclosure
        // rows without an action row.
        let pending = steering == Some(SteeringStatus::Pending);
        let label = |label: String| -> SharedString {
            if pending {
                format!("{label} · {}", crate::tr!("chat.steering")).into()
            } else {
                label.into()
            }
        };
        let mut column = v_flex().w_full().gap_2();
        let mut keys = notice_keys(&notice, &entry.id).into_iter();
        let mut key = || keys.next().expect("a key per notice row");
        match &notice {
            HostNotice::Orchestrate(callbacks) => {
                for callback in callbacks {
                    let event =
                        label(components::disclosure::callback_event_label(callback).into_owned());
                    column =
                        column.child(self.compose_callback_row(turn, key(), event, callback, cx));
                }
            }
            HostNotice::PullRequest { .. }
            | HostNotice::ReportReminder
            | HostNotice::Control(_) => {
                column = column.child(self.compose_disclosure(
                    turn,
                    key(),
                    label(components::disclosure::notice_label(&notice)),
                    text,
                    cx,
                ));
            }
        }
        column.into_any_element()
    }

    fn compose_callback_row(
        &self,
        turn: usize,
        key: String,
        event: SharedString,
        callback: &OrchestrateCallback,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&key);
        let child_id = callback.child_id.clone();
        let exists = {
            let store = self.workspace_store.read(cx);
            store.thread_meta(&child_id).is_some()
                || store
                    .archived_sessions()
                    .iter()
                    .any(|meta| meta.id == child_id)
        };
        let store = self.workspace_store.clone();
        let host = self.agent_host(cx);
        // The row that opens the agent is what its card grows out of.
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
        let row = components::disclosure::callback_row(
            &key,
            event,
            callback,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            on_open,
            self.hover.clone(),
            &self.motion,
            cx,
        );
        if exists {
            self.side_origins
                .mark(origin, crate::material::radius_card(cx), row)
                .into_any_element()
        } else {
            row
        }
    }

    fn compose_disclosure(
        &self,
        turn: usize,
        key: String,
        label: SharedString,
        full_text: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&key);
        let toggle_key = key.clone();
        components::disclosure::disclosure(
            &key,
            label,
            full_text,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            &self.motion,
            cx,
        )
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
            let duration = format_elapsed(activity_run_duration_ms(folded, turn, is_last));
            let outcome = work_log_outcome(turn, is_last);
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
        for range in tools::run::windows(activities) {
            let newer = activities.len() - range.start - 1;
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
        let items = tools::run::windows(visible).into_iter().map(|range| {
            let newer = visible.len() - range.start - 1;
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

    /// One window of a Work Log: a computer run, an edit's file windows, or
    /// one activity, with `newer` windows after it in its list.
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
            let enabled = auto_expand && file.row.counts.is_some();
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
            let header = tools::diff::file_header(file);
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
        let is_command = matches!(
            &entry.content,
            EntryContent::Item(ItemContent::CommandExecution { .. })
        );
        let auto_enabled =
            row.auto_expand && is_command && self.workspace_store.read(cx).live_command_panel();
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
        // A call whose work its harness lists as running in the background
        // goes on there, whatever the call itself returned; its window leads
        // to that list.
        let thread = self
            .workspace_store
            .read(cx)
            .session_status_for(self.session_id.as_deref(), cx);
        let listed = thread.is_some_and(|status| {
            status
                .background_tasks
                .iter()
                .any(|task| task.task.item_id.as_deref() == Some(entry.id.as_str()))
        });
        // A running command or agent the harness can move into the
        // background, as its own Ctrl+B does.
        let movable = thread.is_some_and(|status| {
            status
                .background_control
                .is_some_and(|control| control.move_running)
        }) && matches!(
            &entry.content,
            EntryContent::Item(ItemContent::CommandExecution { .. } | ItemContent::Subagent { .. })
        );
        if entry.background && listed {
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
        let child = tools::subagent::child_thread(self, entry, cx);
        let detail = if child.is_some() {
            tools::window::Detail::Elsewhere(crate::tr!("tools.open_thread").into_owned().into())
        } else if !expandable {
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
        // A window that opens an agent is what the agent's card grows out of.
        let origin = child
            .as_ref()
            .map(|_| SharedString::from(format!("activity-window-{}", entry.id)));
        let on_click: tools::window::ClickHandler = match child {
            Some(child) => {
                let store = self.workspace_store.clone();
                let host = self.agent_host(cx);
                let origin = origin.clone();
                Box::new(move |_, window, cx| {
                    crate::agents_panel::open_agent(
                        &store,
                        child.clone(),
                        host.clone(),
                        origin.clone(),
                        window,
                        cx,
                    );
                })
            }
            None => {
                let click_key = key;
                let entry_id = entry.id.clone();
                Box::new(cx.listener(move |this, _, _, cx| {
                    this.toggle_activity_expanded((turn, &entry_id), &click_key, expanded, cx);
                }))
            }
        };
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
        // Pointed out from a row of the background work, the window is ringed
        // a while.
        let located = self
            .motion
            .shown(
                format!("located-{}", entry.id),
                (self.located.as_deref() == Some(entry.id.as_str())).then_some(()),
                CHANGE,
                window,
                cx,
            )
            .map(|shown| shown.progress());
        let tool = match located {
            Some(progress) => div()
                .relative()
                .w_full()
                .child(tool)
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(crate::material::radius_chip(cx))
                        .border_2()
                        .border_color(cx.theme().primary.opacity(progress)),
                )
                .into_any_element(),
            None => tool,
        };
        match origin {
            Some(origin) => self
                .side_origins
                .mark(origin, crate::material::radius_card(cx), tool)
                .into_any_element(),
            None => tool,
        }
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
            .child(crate::tr!("background.in_list"))
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
                output,
                status,
                ..
            } => tools::shell::shell_body(
                tools::shell::ShellRun {
                    entry,
                    command,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::draw_settled;
    use agent::ItemStatus;
    use gpui::TestAppContext;
    use tcode_core::session::TurnMeta;

    #[gpui::test]
    fn a_pending_steer_shows_as_its_bubble(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.extend([
            entry("user", user_item("go")),
            entry("assistant", assistant("On it.")),
            entry(
                "steer",
                EntryContent::Steer {
                    text: "also do this".into(),
                    status: SteeringStatus::Pending,
                    attachments: Vec::new(),
                },
            ),
        ]);

        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        assert!(cx.debug_bounds("steering-steer").is_some());
    }

    /// The host steers its notices into a running turn: steered or still
    /// pending, a notice is a disclosure row per notice, never the user's bubble.
    #[gpui::test]
    fn steered_notices_render_as_disclosures_not_bubbles(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};
        use tcode_core::session::SteeringStatus;

        let steer = |text: String, status| EntryContent::Steer {
            text,
            status,
            attachments: Vec::new(),
        };
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries.extend([
            entry("user", user_item("go")),
            entry(
                "wake",
                steer(
                    tcode_core::pull_request_watch::merged_message(5, "https://x/pull/5"),
                    SteeringStatus::Accepted,
                ),
            ),
            entry("assistant", assistant("On it.")),
            entry(
                "results",
                steer(
                    "[orchestrate] thread a (\"First\") reported.\nResult (reported via report_result):\none\n\n[orchestrate] thread b (\"Second\") failed.\nthe provider closed".into(),
                    SteeringStatus::Pending,
                ),
            ),
        ]);

        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        cx.update(|cx| cx.set_reduce_motion(true));
        let (_view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        for row in [
            "disclosure-pull-request-update-wake",
            "disclosure-orchestrate-callback-results-0",
            "disclosure-orchestrate-callback-results-1",
        ] {
            assert!(cx.debug_bounds(row).is_some(), "{row} is not a disclosure");
        }
        for bubble in ["steering-wake", "steering-results"] {
            assert!(cx.debug_bounds(bubble).is_none(), "{bubble} is a bubble");
        }
    }

    /// A running command's panel watches the host's screen at the panel's
    /// width, over one subscription per width that growth keeps and that
    /// collapsing the panel or leaving the thread releases.
    #[gpui::test]
    fn a_running_command_subscribes_to_the_screen_at_its_width_until_closed(
        cx: &mut TestAppContext,
    ) {
        use gpui::{VisualTestContext, px, size};
        use tcode_protocol::Topic;

        let mut threads = seed_two_threads(cx, Timeline::default(), Timeline::default());
        cx.update(|cx| cx.set_reduce_motion(true));
        // A provider runs the command, and its output grows.
        let turn = scripted_turn(
            &threads.host,
            &mut threads.driver,
            &threads.first.clone(),
            agent::ProviderKind::ClaudeCode,
            cx,
        );
        for event in [
            agent::AgentEvent::TurnStarted {
                turn_id: "turn".into(),
            },
            agent::AgentEvent::ItemStarted(agent::ThreadItem {
                id: "run".into(),
                parent_item_id: None,
                content: ItemContent::CommandExecution {
                    command: "make".into(),
                    output: String::new(),
                    exit_code: None,
                    status: ItemStatus::InProgress,
                },
            }),
        ] {
            turn.1.try_send(event).unwrap();
        }
        let TwoThreads {
            store,
            mut driver,
            host,
            first: session_id,
            second: other_id,
        } = threads;
        driver.wait_until(cx, "the running command", |cx| {
            store.read_with(cx, |store, cx| {
                store
                    .with_timeline_for(
                        store.sole_thread().as_deref(),
                        |timeline| timeline.entries.iter().any(|entry| entry.id == "run"),
                        cx,
                    )
                    .unwrap_or(false)
            })
        });
        let window_state = cx.new(|_| WindowState::default());
        let (_, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(900.)));

        let link = driver.link.clone();
        let output_topics = || {
            link.subscribed_topics()
                .into_iter()
                .filter_map(|topic| match topic {
                    Topic::CommandOutput {
                        session_id,
                        item_id,
                        cols,
                    } => Some((session_id, item_id, cols)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let watched = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, cx| {
                store
                    .command_output_for(store.sole_thread().as_deref(), "run", cx)
                    .and_then(|output| output.watched())
            })
        };
        let frame = |cx: &mut VisualTestContext| {
            store.read_with(cx, |store, cx| {
                store
                    .command_output_for(store.sole_thread().as_deref(), "run", cx)
                    .and_then(|output| output.frame().cloned())
            })
        };
        let mut settle = |cx: &mut VisualTestContext,
                          what: &str,
                          ready: &dyn Fn(&mut VisualTestContext) -> bool| {
            driver.settle_until(cx, what, ready);
        };
        // Open the command, as a reader would.
        settle(cx, "the command row", &|cx| {
            cx.debug_bounds("activity-row-run").is_some()
        });
        if cx.debug_bounds("activity-detail").is_none() {
            let row = cx.debug_bounds("activity-row-run").unwrap();
            cx.simulate_click(row.center(), gpui::Modifiers::default());
        }
        settle(cx, "the panel to measure its width", &|cx| {
            watched(cx).is_some()
        });
        let cols = watched(cx).unwrap();
        let subscribed = vec![(session_id.clone(), "run".to_string(), cols)];
        assert_eq!(output_topics(), subscribed);

        for text in ["10%\n", "100%\n"] {
            turn.1
                .try_send(agent::AgentEvent::Delta {
                    item_id: "run".into(),
                    kind: agent::DeltaKind::CommandOutput,
                    text: text.into(),
                })
                .unwrap();
            settle(cx, "one host turn", &|_| true);
            assert_eq!(output_topics(), subscribed, "growth re-subscribed");
        }
        settle(cx, "the screen after the last growth", &|cx| {
            frame(cx).is_some_and(|frame| frame.cols == cols)
        });
        assert_eq!(output_topics(), subscribed);

        cx.simulate_resize(size(px(600.), px(900.)));
        settle(cx, "the narrower screen", &|cx| {
            watched(cx).is_some_and(|watched| watched < cols)
                && frame(cx).is_some_and(|frame| Some(frame.cols) == watched(cx))
        });
        let narrower = watched(cx).unwrap();
        assert_eq!(
            output_topics(),
            vec![(session_id.clone(), "run".to_string(), narrower)]
        );

        let header = cx.debug_bounds("activity-row-run").expect("command row");
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the collapsed panel to release its screen", &|_| {
            output_topics().is_empty()
        });
        let header = cx.debug_bounds("activity-row-run").expect("command row");
        cx.simulate_click(header.center(), gpui::Modifiers::default());
        settle(cx, "the reopened panel", &|_| output_topics().len() == 1);

        store.update(cx, |store, cx| store.show_alone(other_id, cx));
        settle(cx, "leaving the thread", &|_| output_topics().is_empty());
        driver.shutdown(cx, &host);
    }

    /// The rewind panel a message opens offers exactly the scopes its
    /// Harness restores that message in: Claude Code all three of any turn
    /// (while its preview is still out); OpenCode the conversation, with or
    /// without files, of any turn, never files alone.
    #[gpui::test]
    fn the_rewind_panel_offers_exactly_the_harness_rewind_modes(cx: &mut TestAppContext) {
        use agent::{ProviderKind, RewindMode};
        use gpui::{Modifiers, VisualTestContext, px, size};

        const ALL: [RewindMode; 3] = [
            RewindMode::FilesAndConversation,
            RewindMode::Conversation,
            RewindMode::Files,
        ];
        for (provider, turn, offered) in [
            (ProviderKind::ClaudeCode, 0, &ALL[..]),
            (ProviderKind::ClaudeCode, 1, &ALL[..]),
            (
                ProviderKind::OpenCode,
                0,
                &[RewindMode::FilesAndConversation, RewindMode::Conversation][..],
            ),
            (
                ProviderKind::OpenCode,
                1,
                &[RewindMode::FilesAndConversation, RewindMode::Conversation][..],
            ),
        ] {
            let mut timeline = Timeline::default();
            timeline.turns = (0..2)
                .map(|index| TurnMeta {
                    provider_checkpoint_id: Some(format!("msg_checkpoint_{index}")),
                    ..TurnMeta::default()
                })
                .collect();
            timeline.entries.extend([
                at_turn(entry("user-0", user_item("first")), 0),
                at_turn(entry("assistant-0", assistant("One.")), 0),
                at_turn(entry("user-1", user_item("second")), 1),
                at_turn(entry("assistant-1", assistant("Two.")), 1),
            ]);
            // A preview the panel asks for starts a scripted provider.
            let (workspace_store, window_state, session_id, _driver, _scripted) =
                seed_chat_scripted(cx, log_of(&timeline), false, Some(provider), true);
            let (_view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
            });
            let window: &mut VisualTestContext = window;
            // The preview's spinner turns while the Harness is asked.
            window.update(|_, cx| cx.set_reduce_motion(true));
            window.simulate_resize(size(px(1_024.), px(700.)));
            draw_settled(window);
            // The bubble's actions show while the pointer is over it.
            let bubble = window
                .debug_bounds(["user-bubble-user-0", "user-bubble-user-1"][turn])
                .expect("the user bubble is drawn");
            window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
            draw_settled(window);
            let trigger = window
                .debug_bounds(["rewind-user-0", "rewind-user-1"][turn])
                .unwrap_or_else(|| panic!("{provider:?} turn {turn} has a rewind action"));
            window.simulate_click(trigger.center(), Modifiers::default());
            draw_settled(window);
            assert!(window.debug_bounds("rewind-panel").is_some());
            let shown: Vec<RewindMode> = ALL
                .into_iter()
                .filter(|mode| {
                    window
                        .debug_bounds(match mode {
                            RewindMode::FilesAndConversation => {
                                "rewind-option-FilesAndConversation"
                            }
                            RewindMode::Conversation => "rewind-option-Conversation",
                            RewindMode::Files => "rewind-option-Files",
                        })
                        .is_some()
                })
                .collect();
            assert_eq!(shown, offered, "{provider:?} turn {turn}");
        }
    }

    /// The panel asks the Harness what each file scope would restore, and
    /// offers those scopes only when it names files: a message after which
    /// nothing changed can only have its conversation rewound, as Claude
    /// Code's own menu hides "Restore code" there.
    #[gpui::test]
    fn the_rewind_panel_drops_file_scopes_the_preview_finds_nothing_for(cx: &mut TestAppContext) {
        use agent::{
            AgentEvent, ProviderKind, RewindFile, RewindFiles, RewindMode, RewindPreview,
            RewindPreviewResult, SessionCommand,
        };
        use gpui::{Modifiers, VisualTestContext, px, size};

        for (files, offered) in [
            (Vec::new(), vec![RewindMode::Conversation]),
            (
                vec![RewindFile::path("src/main.rs")],
                vec![
                    RewindMode::FilesAndConversation,
                    RewindMode::Conversation,
                    RewindMode::Files,
                ],
            ),
        ] {
            let mut timeline = Timeline::default();
            timeline.turns = vec![TurnMeta {
                provider_checkpoint_id: Some("checkpoint-0".into()),
                ..TurnMeta::default()
            }];
            timeline.entries.extend([
                at_turn(entry("user-0", user_item("first")), 0),
                at_turn(entry("assistant-0", assistant("One.")), 0),
            ]);
            let (store, window_state, session_id, mut driver, scripted) = seed_chat_scripted(
                cx,
                log_of(&timeline),
                false,
                Some(ProviderKind::ClaudeCode),
                true,
            );
            let (commands, events) = scripted.unwrap();
            let (_view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(store.clone(), window_state, Some(session_id), window, cx)
            });
            let window: &mut VisualTestContext = window;
            // The preview's spinner turns while the Harness is asked.
            window.update(|_, cx| cx.set_reduce_motion(true));
            window.simulate_resize(size(px(1_024.), px(700.)));
            draw_settled(window);
            let bubble = window.debug_bounds("user-bubble-user-0").unwrap();
            window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
            draw_settled(window);
            let trigger = window.debug_bounds("rewind-user-0").unwrap();
            window.simulate_click(trigger.center(), Modifiers::default());
            draw_settled(window);
            assert!(window.debug_bounds("rewind-preview-checking").is_some());

            let mut asked = Vec::new();
            while asked.len() < 2 {
                if let SessionCommand::PreviewRewind {
                    checkpoint_id,
                    mode,
                } = driver.recv(window, &commands)
                {
                    asked.push((checkpoint_id, mode));
                }
            }
            for (checkpoint_id, mode) in asked {
                assert_eq!(checkpoint_id, "checkpoint-0");
                events
                    .try_send(AgentEvent::RewindPreviewed {
                        checkpoint_id,
                        mode,
                        result: RewindPreviewResult::Ready(RewindPreview {
                            files: RewindFiles {
                                files: files.clone(),
                                insertions: Some(3),
                                deletions: Some(1),
                                skipped_links: 0,
                            },
                            conflicts: Vec::new(),
                        }),
                    })
                    .unwrap();
            }
            let shown = |window: &mut VisualTestContext| -> Vec<RewindMode> {
                [
                    RewindMode::FilesAndConversation,
                    RewindMode::Conversation,
                    RewindMode::Files,
                ]
                .into_iter()
                .filter(|mode| {
                    window
                        .debug_bounds(format!("rewind-option-{mode:?}").leak())
                        .is_some()
                })
                .collect()
            };
            // The spinner is the selected scope's preview alone; the scopes
            // offered settle once the other scope's answer lands as well.
            driver.settle_until(window, "the previews' answers", |window| {
                window.debug_bounds("rewind-preview-checking").is_none() && shown(window) == offered
            });
            assert_eq!(
                window.debug_bounds("rewind-preview-files").is_some(),
                !files.is_empty(),
                "the files the default scope restores"
            );
        }
    }

    /// A finished turn's fork is beside its answer's Copy where the Harness
    /// has one, whatever it is called, and never on a user message or a turn
    /// still running. Used, it opens a thread that names where it came from,
    /// with nothing in the composer: no message was dropped.
    #[gpui::test]
    fn a_finished_turn_offers_its_fork_beside_the_answer(cx: &mut TestAppContext) {
        use agent::{ProviderKind, TurnStatus};
        use gpui::{Modifiers, VisualTestContext, px, size};

        // The running turn's indicator rests only with motion reduced.
        cx.update(|cx| cx.set_reduce_motion(true));
        for (provider, running, offered) in [
            (ProviderKind::ClaudeCode, false, true),
            (ProviderKind::Pi, false, true),
            (ProviderKind::Cursor, false, false),
            (ProviderKind::ClaudeCode, true, false),
        ] {
            let mut timeline = Timeline::default();
            timeline.turns = (0..2)
                .map(|index| TurnMeta {
                    provider_checkpoint_id: Some(format!("checkpoint-{index}")),
                    status: Some(TurnStatus::Completed),
                    ..TurnMeta::default()
                })
                .collect();
            timeline.entries.extend([
                at_turn(entry("user-0", user_item("first")), 0),
                at_turn(entry("assistant-0", assistant("One.")), 0),
            ]);
            if running {
                timeline.turns.truncate(1);
            } else {
                timeline.entries.extend([
                    at_turn(entry("user-1", user_item("second")), 1),
                    at_turn(entry("assistant-1", assistant("Two.")), 1),
                ]);
            }
            let (store, window_state, session_id, mut driver, scripted) =
                seed_chat_scripted(cx, log_of(&timeline), false, Some(provider), running);
            // The second turn runs: the user sent it and the provider answers.
            let mut user = "user-1".to_owned();
            if let Some((commands, events)) = &scripted {
                driver.command(
                    cx,
                    tcode_protocol::Command::SendTurn {
                        session_id: session_id.clone(),
                        text: "second".into(),
                        attachment_paths: Vec::new(),
                    },
                );
                let delivery_id = loop {
                    if let agent::SessionCommand::SendTurn { delivery_id, .. } =
                        driver.recv(cx, commands)
                    {
                        break delivery_id;
                    }
                };
                for event in [
                    agent::AgentEvent::TurnAccepted { delivery_id },
                    agent::AgentEvent::TurnStarted {
                        turn_id: "turn-1".into(),
                    },
                    agent::AgentEvent::ItemStarted(agent::ThreadItem {
                        id: "assistant-1".into(),
                        parent_item_id: None,
                        content: ItemContent::AssistantMessage {
                            text: "Two.".into(),
                            summarized: false,
                        },
                    }),
                ] {
                    events.try_send(event).unwrap();
                }
                let sent = |cx: &TestAppContext| {
                    store.read_with(cx, |store, cx| {
                        store
                            .with_timeline_for(
                                Some(session_id.as_str()),
                                |timeline| {
                                    let last = timeline.entries.last()?;
                                    (last.id == "assistant-1").then(|| {
                                        timeline.entries.iter().rev().find_map(|entry| {
                                            crate::chat::model::user_content(&entry.content)
                                                .map(|_| entry.id.clone())
                                        })
                                    })?
                                },
                                cx,
                            )
                            .flatten()
                    })
                };
                driver.wait_until(cx, "the running turn", |cx| sent(cx).is_some());
                user = sent(cx).unwrap();
            }
            let (_view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(
                    store.clone(),
                    window_state.clone(),
                    Some(session_id.clone()),
                    window,
                    cx,
                )
            });
            let window: &mut VisualTestContext = window;
            window.simulate_resize(size(px(1_024.), px(700.)));
            draw_settled(window);
            let bubble = window
                .debug_bounds(format!("user-bubble-{user}").leak())
                .expect("the user bubble is drawn");
            window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
            draw_settled(window);
            assert!(window.debug_bounds(format!("fork-{user}").leak()).is_none());
            if let Some(copy) = window.debug_bounds("copy-assistant:assistant-1") {
                window.simulate_mouse_move(copy.center(), None, Modifiers::default());
                draw_settled(window);
            }
            assert_eq!(
                window.debug_bounds("fork-assistant-1").is_some(),
                offered,
                "{provider:?} running {running}"
            );
            if !offered {
                continue;
            }
            let copy = window
                .debug_bounds("copy-assistant:assistant-0")
                .expect("the finished turn's answer has its actions");
            window.simulate_mouse_move(copy.center(), None, Modifiers::default());
            draw_settled(window);
            let action = window
                .debug_bounds("fork-assistant-0")
                .expect("the fork is beside Copy");

            window.simulate_click(action.center(), Modifiers::default());
            let fork = |cx: &App| {
                store
                    .read(cx)
                    .sidebar_sessions()
                    .iter()
                    .find(|meta| meta.fork_origin.is_some())
                    .map(|meta| meta.id.clone())
            };
            driver.wait_until(window, "the fork", |cx| cx.update(|cx| fork(cx).is_some()));
            let fork = window.update(|_, cx| fork(cx)).unwrap();
            let fork_id = fork.clone();
            window.update(|_, cx| store.update(cx, |store, cx| store.show_alone(fork_id, cx)));
            let (view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(store.clone(), window_state, Some(fork.clone()), window, cx)
            });
            let window: &mut VisualTestContext = window;
            window.simulate_resize(size(px(1_024.), px(700.)));
            // The origin comes with the thread's metadata, once its status
            // reaches this window, which redraws it.
            driver.settle_until(window, "the fork's source", |window| {
                window.debug_bounds("fork-origin").is_some()
            });
            assert_eq!(
                view.read_with(window, |view, cx| view.composer.read(cx).draft(cx)),
                ""
            );
        }
    }

    /// A rewind's divider says what the rewind did; a truncating one only
    /// lets the removed turns be read back, an undoable one adds Undo, and a
    /// branch adds the way back to the branch it left.
    #[gpui::test]
    fn a_rewind_divider_offers_what_its_harness_keeps(cx: &mut TestAppContext) {
        use agent::{LaterHistory, ProviderKind, RewindMode};
        use gpui::{Modifiers, VisualTestContext, px, size};

        for (later_history, undo_token, sent_after, undo) in [
            (LaterHistory::Truncated, None, false, None),
            (
                LaterHistory::HiddenUntilNextSend,
                Some("revert"),
                false,
                Some("rewound-undo"),
            ),
            (
                LaterHistory::HiddenUntilNextSend,
                Some("revert"),
                true,
                None,
            ),
            (
                LaterHistory::Branch,
                Some("/sessions/original.jsonl"),
                false,
                Some("rewound-return"),
            ),
            (
                LaterHistory::Branch,
                Some("/sessions/original.jsonl"),
                true,
                Some("rewound-open-branch"),
            ),
        ] {
            let mut events = Vec::new();
            for index in 0..2 {
                events.extend([
                    agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                        id: format!("user-{index}"),
                        parent_item_id: None,
                        content: agent::ItemContent::UserMessage {
                            text: format!("prompt {index}"),
                            attachments: Vec::new(),
                        },
                    }),
                    agent::AgentEvent::TurnCheckpoint {
                        turn_id: format!("turn-{index}"),
                        checkpoint_id: format!("checkpoint-{index}"),
                    },
                    agent::AgentEvent::TurnCompleted {
                        turn_id: format!("turn-{index}"),
                        status: agent::TurnStatus::Completed,
                        usage: None,
                    },
                ]);
            }
            events.push(agent::AgentEvent::RewindCompleted {
                checkpoint_id: "checkpoint-1".into(),
                mode: RewindMode::Conversation,
                prefill: None,
                prefill_attachments: Vec::new(),
                later_history,
                files: None,
                incomplete: None,
                undo_token: undo_token.map(str::to_owned),
            });
            if sent_after {
                events.push(agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "user-after".into(),
                    parent_item_id: None,
                    content: agent::ItemContent::UserMessage {
                        text: "sent after".into(),
                        attachments: Vec::new(),
                    },
                }));
            }
            let (workspace_store, window_state, session_id, _driver) =
                seed_chat_log_on(cx, events, Some(ProviderKind::OpenCode));
            cx.update(|cx| cx.set_reduce_motion(true));
            let (_view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(
                    workspace_store.clone(),
                    window_state,
                    Some(session_id),
                    window,
                    cx,
                )
            });
            let window: &mut VisualTestContext = window;
            window.simulate_resize(size(px(1_024.), px(700.)));
            draw_settled(window);
            // The divider the client folded from the host's records.
            let divider = workspace_store
                .read_with(window, |store, cx| {
                    store.with_timeline_for(
                        store.sole_thread().as_deref(),
                        |timeline| {
                            timeline
                                .entries
                                .iter()
                                .rev()
                                .find(|entry| matches!(entry.content, EntryContent::Rewound(_)))
                                .map(|entry| entry.id.clone())
                        },
                        cx,
                    )
                })
                .flatten()
                .expect("the rewind's divider");
            let toggle = window
                .debug_bounds(format!("rewound-toggle-{divider}").leak())
                .unwrap_or_else(|| panic!("{later_history:?}: the removed turn can be read"));
            for action in ["rewound-undo", "rewound-return", "rewound-open-branch"] {
                assert_eq!(
                    window
                        .debug_bounds(format!("{action}-{divider}").leak())
                        .is_some(),
                    undo == Some(action),
                    "{later_history:?}: {action}"
                );
            }
            assert!(
                window
                    .debug_bounds(format!("rewound-removed-{divider}").leak())
                    .is_none(),
                "the removed turn is folded"
            );
            window.simulate_click(toggle.center(), Modifiers::default());
            draw_settled(window);
            assert!(
                window
                    .debug_bounds(format!("rewound-removed-{divider}").leak())
                    .is_some()
            );
        }
    }
}
