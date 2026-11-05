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
                    column = column.child(components::dividers::context_compacted_divider(
                        &entry.id,
                        match &entry.content {
                            EntryContent::ContextCompacted(c) => Some(c),
                            _ => None,
                        },
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
                Segment::ControlAction(entry) => {
                    let EntryContent::ControlAction(action) = &entry.content else {
                        unreachable!();
                    };
                    column = column.child(components::dividers::control_action_divider(
                        &entry.id, action, cx,
                    ));
                }
                Segment::ActivityRun(activities) => {
                    column = column.child(self.compose_work_log(
                        (index, &row.part, turn, cwd, activities, row.live_activity),
                        window,
                        cx,
                    ));
                }
                Segment::Images(images) => {
                    column = column.child(components::images::image_tiles(
                        images,
                        cwd,
                        self.session_key.as_deref().unwrap_or_default(),
                        &self.motion.clone(),
                        window,
                        cx,
                    ));
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
                    if actions == components::assistant::MessageActions::InFooter {
                        ending = Some((
                            components::assistant::EndingActions {
                                id: entry.id.clone(),
                                copied,
                                compact,
                            },
                            copy_text.clone(),
                        ));
                    }
                    column = column.child(components::assistant::assistant(
                        components::assistant::AssistantData {
                            id: &entry.id,
                            text,
                            copy_text: copy_text.clone(),
                            summarized: *summarized,
                            cwd,
                            markdown,
                            compact,
                            pinned,
                            actions,
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
                Segment::Error(entry) => {
                    let message = displayed_error_text(&entry.content);
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
                                now_secs(),
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
        let changes_fold = self
            .motion
            .shown(
                format!("changed-files-fold-{index}"),
                (!turn.running && !changes.is_empty()).then_some(()),
                SHOW,
                window,
                cx,
            )
            .filter(|_| !changes.is_empty());
        let changes = changes_fold.map(|fold| {
            let more_key = format!("changed-files-more-{index}");
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
                        &self.motion,
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
            let status_key = SharedString::from(format!("turn-status-{index}"));
            self.shared.arrive(
                &status_key,
                &SharedString::from(row.identity.to_string()),
                cx,
            );
            let motion = self.motion.clone();
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
                    format!("turn-status-{index}"),
                    turn.running,
                    CHANGE,
                    window,
                    cx,
                    move |running, window, cx| {
                        if *running {
                            return div()
                                .id(("working-status", index))
                                .debug_selector(move || format!("working-status-{index}"))
                                .child(components::indicator::turn_working_indicator(
                                    index,
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
            &self.motion.clone(),
            format!("pending-steers-{index}"),
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

    pub(in crate::chat) fn on_rewind_turn(
        &mut self,
        action: &RewindTurn,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let RewindTurn { turn, mode } = *action;
        let session_id = self.session_id.clone().unwrap_or_default();
        self.workspace_store.update(cx, |store, _cx| {
            store.rewind_turn_for(session_id, turn, mode)
        });
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
        let (turn, entry_id, text, cwd, context_len, attachments, steering, pinned) = args;
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

        let context = context_len
            .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
            .map(|len| &text[..len]);
        let visible = user_visible_text(text, context_len);
        let rewind_menu = steering
            .is_none()
            .then(|| {
                self.workspace_store.read(cx).chat_native_rewind_state_for(
                    self.session_id.as_deref(),
                    turn,
                    cx,
                )
            })
            .flatten()
            .map(|state| components::bubble::RewindMenu {
                turn,
                disabled: state.blocked,
                modes: state.modes,
            });
        let rewind = rewind_menu.clone().map(|menu| {
            let handlers = menu
                .modes
                .iter()
                .map(|&mode| {
                    Arc::new(cx.listener(move |this, _, _, cx| {
                        let session_id = this.session_id.clone().unwrap_or_default();
                        this.workspace_store.update(cx, |store, _cx| {
                            store.rewind_turn_for(session_id, turn, mode)
                        });
                    })) as components::bubble::SharedClickHandler
                })
                .collect();
            components::bubble::native_rewind_button(
                menu,
                self.window_state.read(cx).compact,
                handlers,
                &self.motion,
                cx,
            )
        });
        let markdown = self.md_states.get(entry_id).map(|md| md.state.clone());
        let copy_key = format!("user:{entry_id}");
        let copied = self.copied.is(&copy_key);
        let mark = copy_key;
        let copy_text: Arc<str> = Arc::from(visible);
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
                            tcode_protocol::OriginalTarget::File { path: path.clone() },
                            crate::store::image_preview(path.clone(), 120., true),
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

        let bubble = if let Some(author) = self.workspace_store.read(cx).message_byline_for(
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
        };

        let Some(context) = context else {
            return bubble;
        };
        v_flex()
            .w_full()
            .gap_2()
            .child(self.compose_disclosure(
                turn,
                format!("orchestrate-context-{entry_id}"),
                injected_context_label(context).into(),
                context,
                cx,
            ))
            .child(bubble)
            .into_any_element()
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
        let (text, steering, context_len, attachments) =
            user_content(&entry.content).expect("a user entry");
        let Some(notice) = user_notice(&entry.content) else {
            return self.compose_user(
                (
                    turn,
                    &entry.id,
                    text,
                    cwd,
                    context_len,
                    attachments,
                    steering,
                    pinned,
                ),
                window,
                cx,
            );
        };
        // A notice is the host's, never the user's words: centered disclosure
        // rows without an action row, below any context the turn injected.
        let context = context_len
            .filter(|len| *len <= text.len() && text.is_char_boundary(*len))
            .map(|len| &text[..len]);
        let pending = steering == Some(SteeringStatus::Pending);
        let label = |label: String| -> SharedString {
            if pending {
                format!("{label} · {}", crate::tr!("chat.steering")).into()
            } else {
                label.into()
            }
        };
        let mut column = v_flex()
            .w_full()
            .gap_2()
            .when_some(context, |column, context| {
                column.child(self.compose_disclosure(
                    turn,
                    format!("orchestrate-context-{}", entry.id),
                    injected_context_label(context).into(),
                    context,
                    cx,
                ))
            });
        let mut keys = notice_keys(&notice, &entry.id).into_iter();
        let mut key = || keys.next().expect("a key per notice row");
        let visible = user_visible_text(text, context_len);
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
                    visible,
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
        let section_key = work_log_key(index, segment_id);
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
            let outcome = work_log_outcome(turn, folded, is_last);
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
            let enabled = auto_expand && edit.live && file.row.counts.is_some();
            let expanded =
                self.auto_activity_expanded((turn, &entry.id), &key, enabled, recency, cx);
            let window_key = format!("{}-file-{file_index}", entry.id);
            let detail = if file.row.counts.is_none() {
                tools::window::Detail::None
            } else {
                match tools::window::fold(&self.motion, &window_key, expanded, window, cx) {
                    Some(shown) => tools::window::Detail::Fold(
                        shown,
                        self.inline_diffs.render(&key, file, cx),
                        0,
                    ),
                    None => tools::window::Detail::Shut,
                }
            };
            let header = tools::diff::file_header(file);
            // The call's failure and its time belong to it, told once.
            let first = file_index == 0;
            let elapsed_ms = first
                .then(|| tools::window::elapsed(&header, entry))
                .flatten();
            let toggle_key = key.clone();
            let entry_id = entry.id.clone();
            rows.push(tools::window::tool_window(
                tools::window::WindowSpec {
                    key: window_key,
                    header,
                    status: edit.status,
                    error: first.then(|| edit.failure.clone()).flatten(),
                    elapsed_ms,
                    compact: false,
                    copy: tools::diff::file_copy_items(file),
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
            header,
            status,
            mut copy,
            expandable,
            path,
            ..
        } = activity;
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
        match origin {
            Some(origin) => self
                .side_origins
                .mark(origin, crate::material::radius_card(cx), tool)
                .into_any_element(),
            None => tool,
        }
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
                    workdir: None,
                    output,
                    hosted: true,
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

/// A recorded turn's injected context is named by what it holds.
fn injected_context_label(context: &str) -> String {
    let orchestrate = || crate::tr!("chat.orchestrate_skill").into_owned();
    match tcode_core::pull_request::strip_linking_instructions(context) {
        None => orchestrate(),
        Some(rest) => {
            let instructions = crate::tr!("pull_requests.instructions_disclosure").into_owned();
            if rest.trim().is_empty() {
                instructions
            } else {
                format!("{instructions} · {}", orchestrate())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::{draw, draw_settled};
    use agent::ItemStatus;
    use gpui::TestAppContext;
    use tcode_core::session::TurnMeta;

    #[gpui::test]
    fn pending_steer_after_the_last_segment_renders_in_the_trailer(cx: &mut TestAppContext) {
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
                    context_len: None,
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
            context_len: None,
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

    /// A chat under a probe that reads, on each frame it draws, how far in
    /// the hover fill of each of `keys` is.
    struct HoverProbe {
        chat: gpui::Entity<ChatView>,
        keys: &'static [&'static str],
        drawn: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<&'static str, f32>>>,
    }

    impl gpui::Render for HoverProbe {
        fn render(
            &mut self,
            window: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{ParentElement as _, Styled as _};
            let hover = self.chat.read(cx).timeline.read(cx).hover.clone();
            for &key in self.keys {
                let fill = hover.reveal(key, window, cx);
                self.drawn.borrow_mut().insert(key, fill);
            }
            gpui::div().size_full().child(self.chat.read(cx).frame())
        }
    }

    /// A Work Log's header and a tool window's header fill in over several
    /// frames as the pointer comes onto them and empty as it leaves, and the
    /// window comes to rest; with motion reduced the fill comes and goes at
    /// once.
    #[gpui::test]
    fn a_header_under_the_pointer_fades_its_fill_in_and_out(cx: &mut TestAppContext) {
        use gpui::{AppContext as _, Modifiers, VisualTestContext, point, px, size};
        use std::time::Duration;

        let keys: &[&str] = &["worklog-header-0-command-0", "activity-row-command-0"];
        for reduce in [false, true] {
            let mut timeline = Timeline::default();
            timeline.turns = vec![TurnMeta::default()];
            timeline.entries.push(entry("user", user_item("go")));
            for index in 0..3 {
                timeline.entries.push(command(&format!("command-{index}")));
            }
            timeline
                .entries
                .push(entry("assistant", assistant("Done.")));
            let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
            cx.update(|cx| cx.set_reduce_motion(reduce));
            let drawn = std::rc::Rc::default();
            let (_, cx) = cx.add_window_view(|window, cx| HoverProbe {
                chat: cx.new(|cx| {
                    ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
                }),
                keys,
                drawn: std::rc::Rc::clone(&drawn),
            });
            let cx: &mut VisualTestContext = cx;
            cx.simulate_resize(size(px(1_024.), px(700.)));
            draw_settled(cx);
            let header = cx.debug_bounds(keys[0]).expect("collapsed work log");
            cx.simulate_click(header.center(), Modifiers::default());
            draw_settled(cx);

            let away = point(px(1_000.), px(4.));
            for key in keys {
                // The fill drawn on each frame after the pointer moves.
                let follow = |to, cx: &mut VisualTestContext| {
                    cx.simulate_mouse_move(to, None, Modifiers::default());
                    let mut seen = Vec::new();
                    for _ in 0..20 {
                        cx.executor().advance_clock(Duration::from_millis(16));
                        cx.update(|window, cx| window.simulate_next_frame(cx));
                        draw(cx);
                        seen.push(drawn.borrow()[key]);
                    }
                    draw_settled(cx);
                    seen
                };
                let fades = |seen: &[f32], up: bool| {
                    seen.windows(2)
                        .all(|pair| pair[1] == pair[0] || (pair[1] > pair[0]) == up)
                        && seen
                            .iter()
                            .filter(|fill| **fill > 0. && **fill < 1.)
                            .count()
                            >= 3
                };
                follow(away, cx);
                let bounds = cx.debug_bounds(key).unwrap_or_else(|| panic!("{key}"));
                let entering = follow(bounds.center(), cx);
                let leaving = follow(away, cx);
                assert_eq!(*entering.last().unwrap(), 1., "{key}: {entering:?}");
                assert_eq!(*leaving.last().unwrap(), 0., "{key}: {leaving:?}");
                if reduce {
                    assert_eq!((entering[0], leaving[0]), (1., 0.), "{key} faded");
                } else {
                    assert!(fades(&entering, true), "{key} popped in: {entering:?}");
                    assert!(fades(&leaving, false), "{key} popped out: {leaving:?}");
                }
            }
            cx.update(|_, cx| cx.set_reduce_motion(false));
        }
    }

    /// The Copy of the message a finished turn ends on, drawn in the turn's
    /// footer, stays in as the pointer goes down from the message's text,
    /// across the space between them, onto Copy: the message and its footer
    /// are one hover.
    #[gpui::test]
    fn the_pointer_going_from_a_message_to_its_footer_keeps_copy_in(cx: &mut TestAppContext) {
        use gpui::{AppContext as _, Modifiers, VisualTestContext, point, px, size};
        use std::time::Duration;

        const KEYS: &[&str] = &["assistant-answer"];
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            end_ts: Some(1_700_000_000),
            ..TurnMeta::default()
        }];
        timeline.entries.push(entry("question", user_item("go")));
        timeline.entries.push(entry(
            "answer",
            assistant("The first line.\n\nThe second line."),
        ));
        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let drawn = std::rc::Rc::default();
        let (_, cx) = cx.add_window_view(|window, cx| HoverProbe {
            chat: cx.new(|cx| {
                ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
            }),
            keys: KEYS,
            drawn: std::rc::Rc::clone(&drawn),
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        let message = cx
            .debug_bounds("timeline-row-1")
            .expect("the message's row");
        let copy = cx
            .debug_bounds("copy-assistant:answer")
            .expect("the footer's Copy");
        let x = copy.center().x;
        cx.simulate_mouse_move(point(x, message.top() + px(4.)), None, Modifiers::default());
        draw_settled(cx);
        assert_eq!(drawn.borrow()[KEYS[0]], 1., "the message's Copy is in");

        // A pixel at a time, a frame apart, as a slow pointer moves.
        let mut y = message.top() + px(4.);
        while y < copy.center().y {
            y += px(1.);
            cx.simulate_mouse_move(point(x, y), None, Modifiers::default());
            cx.executor().advance_clock(Duration::from_millis(16));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            draw(cx);
            let reveal = drawn.borrow()[KEYS[0]];
            assert_eq!(reveal, 1., "Copy faded with the pointer at {y:?}: {reveal}");
        }
    }

    #[gpui::test]
    fn an_expanded_work_log_lays_out_only_the_activities_on_screen(cx: &mut TestAppContext) {
        use gpui::{FollowMode, ListOffset, Modifiers, VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta::default()];
        timeline.entries.push(entry("user", user_item("go")));
        for index in 0..400 {
            timeline.entries.push(command(&format!("command-{index}")));
        }
        timeline
            .entries
            .push(entry("assistant", assistant("Done.")));

        let (workspace_store, window_state, session_id) = seed_chat(cx, timeline);
        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        draw_settled(cx);

        let header = cx
            .debug_bounds("worklog-header-0-command-0")
            .expect("collapsed work log");
        cx.simulate_click(header.center(), Modifiers::default());
        draw_settled(cx);
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("activity-row-command-0").is_some());
        assert!(
            cx.debug_bounds("activity-row-command-399").is_none(),
            "an expanded work log must not lay out all 400 activities"
        );

        view.update(cx, |chat, cx| {
            chat.list_state.set_follow_mode(FollowMode::Tail);
            cx.notify();
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("activity-row-command-399").is_some());
        assert!(cx.debug_bounds("activity-row-command-0").is_none());

        // Opening one activity keeps the measured heights of the turn's
        // other rows, however far away.
        view.update(cx, |chat, cx| {
            chat.list_state.scroll_to(ListOffset::default());
            cx.notify();
        });
        draw_settled(cx);
        let last = view
            .read_with(cx, |chat, _| chat.timeline())
            .read_with(cx, |chat, _| chat.rows.len() - 1);
        let measured = |cx: &mut VisualTestContext| {
            view.read_with(cx, |chat, _| {
                chat.list_state.bounds_for_item(last).is_some()
            })
        };
        assert!(measured(cx));
        let row = cx
            .debug_bounds("activity-row-command-0")
            .expect("the first activity");
        cx.simulate_click(row.center(), Modifiers::default());
        draw_settled(cx);
        assert!(cx.debug_bounds("activity-detail").is_some());
        assert!(
            measured(cx),
            "opening one activity remeasured the whole turn"
        );
    }

    #[gpui::test]
    fn a_live_file_edit_lays_out_only_the_diff_lines_on_screen(cx: &mut TestAppContext) {
        use gpui::{Modifiers, ScrollDelta, ScrollEvent, TouchPhase, VisualTestContext};
        use gpui::{point, px, size};

        let mut diff = format!("@@ -1,400 +1,401 @@\n+// {}\n", "wide ".repeat(80));
        for line in 0..400 {
            diff.push_str(&format!("-fn old_{line}() {{}}\n+fn new_{line}() {{}}\n"));
        }
        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries = vec![entry("user", user_item("go"))];
        for note in 0..20 {
            timeline.entries.push(entry(
                &format!("note-{note}"),
                assistant("A note above the edit."),
            ));
        }
        timeline.entries.push(entry(
            "edit",
            EntryContent::Item(ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: "src/lib.rs".into(),
                    kind: agent::FileChangeKind::Modify,
                    diff: Some(diff),
                }],
                status: ItemStatus::InProgress,
            }),
        ));
        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        cx.update(|cx| cx.set_reduce_motion(true));
        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(600.), px(900.)));
        draw_settled(cx);

        assert!(cx.debug_bounds("file-edit-diff-line-0").is_some());
        assert!(
            cx.debug_bounds("file-edit-diff-line-300").is_none(),
            "an inline diff must not lay out all 801 lines"
        );

        // Both axes scroll inside the diff; the timeline stays put.
        let timeline_top = |cx: &mut VisualTestContext| {
            let top = view.read_with(cx, |chat, _| chat.list_state.logical_scroll_top());
            (top.item_ix, top.offset_in_item)
        };
        let before = timeline_top(cx);
        assert!(
            view.read_with(cx, |chat, _| chat.list_state.max_offset_for_scrollbar().y) > px(0.)
        );
        let diff_bounds = cx.debug_bounds("file-edit-diff").expect("inline diff");
        let wheel = |delta, cx: &mut VisualTestContext| {
            for (touch_phase, delta) in [
                (TouchPhase::Started, point(px(0.), px(0.))),
                (TouchPhase::Moved, delta),
                (TouchPhase::Ended, point(px(0.), px(0.))),
            ] {
                cx.simulate_event(ScrollEvent {
                    position: diff_bounds.center(),
                    delta: ScrollDelta::Pixels(delta),
                    modifiers: Modifiers::default(),
                    touch_phase,
                    ..Default::default()
                });
            }
            draw_settled(cx);
        };
        wheel(point(px(0.), px(-2_000.)), cx);
        assert!(cx.debug_bounds("file-edit-diff-line-0").is_none());
        // `debug_bounds` takes a `'static` selector.
        let shown: &'static str = (1..801)
            .map(|ix| &*format!("file-edit-diff-line-{ix}").leak())
            .find(|selector| cx.debug_bounds(selector).is_some())
            .expect("a line scrolled into the diff");
        let line = cx.debug_bounds(shown).unwrap();
        wheel(point(px(-200.), px(0.)), cx);
        let scrolled = cx.debug_bounds(shown).unwrap();
        assert!(
            scrolled.left() < line.left(),
            "{scrolled:?} did not scroll left of {line:?}"
        );
        assert_eq!(scrolled.top(), line.top());
        assert_eq!(timeline_top(cx), before);

        // The provider reports a different patch for the same edit.
        timeline.entries[21] = entry(
            "edit",
            EntryContent::Item(ItemContent::FileChange {
                changes: vec![agent::FileChange {
                    path: "src/lib.rs".into(),
                    kind: agent::FileChangeKind::Modify,
                    diff: Some("@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n".into()),
                }],
                status: ItemStatus::InProgress,
            }),
        );
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
        });
        draw_settled(cx);
        assert!(cx.debug_bounds("file-edit-diff-line-1").is_some());
        assert!(cx.debug_bounds("file-edit-diff-line-2").is_none());
    }

    /// A running command's panel shows the screens the host renders at the
    /// panel's width, pushed as the output grows, over one subscription per
    /// width that collapsing the panel or leaving the thread releases.
    #[gpui::test]
    fn a_running_command_shows_the_screens_the_host_pushes_for_its_width(cx: &mut TestAppContext) {
        use crate::terminal_drawer::TERMINAL_CELL_HEIGHT;
        use gpui::{VisualTestContext, px, size};
        use tcode_protocol::{Topic, terminal::TerminalColor};

        let (host, session_id, _) = seeded_chat_host(cx, Timeline::default(), false);
        cx.update(|cx| cx.set_reduce_motion(true));
        let seeded_id = session_id.clone();
        let other_id = smol::block_on(host.update_state_for_test(move |state, cx| {
            state.residents.live.get_mut(&seeded_id).unwrap().timeline = Timeline::default();
            for (ts, event) in [
                agent::AgentEvent::TurnStarted {
                    turn_id: "turn".into(),
                },
                agent::AgentEvent::ItemCompleted(agent::ThreadItem {
                    id: "user".into(),
                    parent_item_id: None,
                    content: ItemContent::UserMessage {
                        text: "build it".into(),
                        context_len: None,
                        attachments: Vec::new(),
                    },
                }),
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
            ]
            .iter()
            .enumerate()
            {
                state.record_event_for_replica_test(&seeded_id, ts as u64, event, cx);
            }
            let other = state.start_draft("other-thread".into(), std::env::temp_dir(), cx);
            let active = state.residents.live.get_mut(&other).expect("second draft");
            active.draft = false;
            let meta = active.meta.clone();
            state.sessions.push(meta);
            other
        }))
        .expect("record the running command");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &store, "chat baseline", |cx| {
            store.read_with(cx, |store, cx| {
                !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        let window_state = cx.new(|_| WindowState::new(false));
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
                          description: &str,
                          ready: &dyn Fn(&mut VisualTestContext) -> bool| {
            let deadline = crate::store::tests::host_deadline();
            loop {
                driver.step(cx, &store);
                draw_settled(cx);
                if ready(cx) {
                    return;
                }
                assert!(Instant::now() < deadline, "waiting for {description}");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        // Nothing drives this turn on the host, so its Work Log reads as
        // finished and folded: open it and the command, as a reader would.
        settle(cx, "the Work Log", &|cx| {
            cx.debug_bounds("worklog-header-0-run").is_some()
        });
        let header = cx.debug_bounds("worklog-header-0-run").unwrap();
        cx.simulate_click(header.center(), gpui::Modifiers::default());
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

        let mut long_line = "x".repeat(usize::from(cols));
        long_line.push_str("yz");
        let growth = [
            "10%\r".to_string(),
            "50%\r".into(),
            "100%\n".into(),
            format!("{long_line}\n"),
            "\u{1b}[32mok\u{1b}[0m\n".into(),
        ];
        for (index, text) in growth.into_iter().enumerate() {
            let seeded_id = session_id.clone();
            smol::block_on(host.update_state_for_test(move |state, cx| {
                state.record_event_for_replica_test(
                    &seeded_id,
                    10 + index as u64,
                    &agent::AgentEvent::Delta {
                        item_id: "run".into(),
                        kind: agent::DeltaKind::CommandOutput,
                        text,
                    },
                    cx,
                );
            }))
            .expect("grow the command output");
            settle(cx, "one host turn", &|_| true);
            assert_eq!(output_topics(), subscribed, "growth re-subscribed");
        }
        settle(cx, "the screen after the last growth", &|cx| {
            frame(cx).is_some_and(|frame| frame.rows == 4)
        });
        let screen = frame(cx).unwrap();
        let text = |row: usize| {
            screen.visible[row]
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        };
        assert_eq!(screen.cols, cols);
        assert_eq!(text(0), "100%", "the progress bar was not redrawn in place");
        assert_eq!(text(1), "x".repeat(usize::from(cols)));
        assert_eq!(text(2), "yz");
        assert_eq!(text(3), "ok");
        assert_eq!(
            screen.style(&screen.visible[3].cells[0]).fg,
            TerminalColor::Indexed(2)
        );
        assert_eq!(output_topics(), subscribed);
        // The panel is as tall as the host's screen, not as the raw output's
        // three lines.
        let output = cx
            .debug_bounds("command-output-run")
            .expect("the output grid");
        assert_eq!(output.size.height, px(4. * TERMINAL_CELL_HEIGHT));

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
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn a_plan_is_decided_on_its_block_and_answered_in_words_from_the_composer(
        cx: &mut TestAppContext,
    ) {
        use gpui::{Modifiers, VisualTestContext, px, size};
        use tcode_core::project::SessionMeta;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let root = crate::store::tests::scratch_root("chat-plan-decision-test");
        let session_store =
            tcode_services::store::SessionStore::open_at(root.clone()).expect("test session store");
        let meta = SessionMeta::new(agent::ProviderKind::ClaudeCode, root.clone(), None);
        let session_id = meta.id.clone();
        session_store.upsert_meta(&meta).expect("persist session");
        let host = tcode_runtime::pipe::spawn_host(
            session_store,
            tcode_runtime::pipe::HostServices::default(),
        )
        .expect("spawn test host");
        let scripted = tcode_runtime::app::scripted_provider(agent::ProviderKind::ClaudeCode);
        let (provider_events, provider_commands) =
            (scripted.events.clone(), scripted.commands.clone());
        smol::block_on(host.update_state_for_test(move |state, _| {
            state.set_provider_launcher_for_test(scripted.launcher)
        }))
        .expect("script the provider");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &store, "workspace baseline", |cx| {
            store.read_with(cx, |store, _| store.baseline_ready())
        });
        store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        store.update(cx, |store, _| {
            store.send_turn_for(session_id.clone(), "Plan it first".into(), Vec::new())
        });
        driver.wait_until(cx, &store, "the turn reaches the provider", |_| {
            matches!(
                provider_commands.try_recv(),
                Ok(agent::SessionCommand::SendTurn { .. })
            )
        });

        let window_state = cx.new(|_| WindowState::new(false));
        let (chat, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(800.)));
        chat.update(cx, |chat, cx| {
            chat.set_session(Some(session_id.clone()), cx)
        });
        let propose = |id: &str, request: &str| {
            let markdown = format!("# Plan {id}\n\n1. Write hello.txt");
            for event in [
                agent::AgentEvent::ItemStarted(agent::ThreadItem {
                    id: id.into(),
                    parent_item_id: None,
                    content: ItemContent::Plan {
                        markdown: markdown.clone(),
                    },
                }),
                agent::AgentEvent::ApprovalRequested(agent::ApprovalRequest {
                    id: request.into(),
                    turn_id: None,
                    parent_item_id: None,
                    kind: agent::ApprovalKind::Plan {
                        item_id: id.into(),
                        markdown,
                    },
                    options: vec![
                        agent::ApprovalOption {
                            id: "allow".into(),
                            label: "Approve".into(),
                            kind: agent::ApprovalOptionKind::AllowOnce,
                        },
                        agent::ApprovalOption {
                            id: "deny".into(),
                            label: "Keep planning".into(),
                            kind: agent::ApprovalOptionKind::RejectOnce,
                        },
                    ],
                }),
            ] {
                provider_events.try_send(event).expect("provider event");
            }
        };
        let decided = |driver: &mut crate::store::tests::LocalHostDriver,
                       cx: &mut VisualTestContext,
                       what: &str| {
            let mut decision = None;
            let deadline = crate::store::tests::host_deadline();
            while decision.is_none() {
                assert!(Instant::now() < deadline, "waiting for {what}");
                driver.step(cx, &store);
                draw_settled(cx);
                if let Ok(agent::SessionCommand::RespondApproval {
                    request_id,
                    decision: answer,
                }) = provider_commands.try_recv()
                {
                    decision = Some((request_id, answer));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            decision.unwrap()
        };
        let shown = |driver: &mut crate::store::tests::LocalHostDriver,
                     cx: &mut VisualTestContext| {
            let deadline = crate::store::tests::host_deadline();
            while cx.debug_bounds("plan-decision").is_none() {
                assert!(Instant::now() < deadline, "waiting for the plan's decision");
                driver.step(cx, &store);
                draw_settled(cx);
                std::thread::sleep(Duration::from_millis(1));
            }
        };

        propose("plan-1", "decide-1");
        shown(&mut driver, cx);
        assert!(
            cx.debug_bounds("approval-header").is_none(),
            "a plan is decided on its block, not in the composer"
        );
        let approve = cx
            .debug_bounds("plan-option-allow")
            .expect("approve button");
        cx.simulate_click(approve.center(), Modifiers::default());
        assert_eq!(
            decided(&mut driver, cx, "the approval"),
            (
                "decide-1".to_string(),
                agent::ApprovalDecision::Option("allow".into())
            )
        );
        provider_events
            .try_send(agent::AgentEvent::ApprovalResolved {
                request_id: "decide-1".into(),
                decision: agent::ApprovalDecision::Option("allow".into()),
            })
            .expect("provider event");

        propose("plan-2", "decide-2");
        shown(&mut driver, cx);
        chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
        cx.simulate_input("Split step one");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            decided(&mut driver, cx, "the feedback"),
            (
                "decide-2".to_string(),
                agent::ApprovalDecision::Feedback("Split step one".into())
            )
        );
        driver.shutdown(cx, &store, &host);
    }

    #[gpui::test]
    fn long_markdown_paints_middle_blocks_when_scrolled_in_chat_outer_list(
        cx: &mut TestAppContext,
    ) {
        use gpui::{VisualTestContext, point, px, size};
        use tcode_runtime::pipe::{HostServices, spawn_host};
        use tcode_services::store::SessionStore;

        const DEMO_MARKDOWN: &str = r#"# H1

## H2

This paragraph has **bold**, *italic*, ~~strikethrough~~, and `inline code`.

```rust
fn main() {
    let language = "Rust";
    let message = format!("Hello from {language}!");
    println!("{message}");
}
```

```typescript
const count: number = 3;
interface Demo {
  title: string;
  enabled: boolean;
}
const demo: Demo = { title: "TypeScript", enabled: true };
```

```python
def greet(name: str) -> str:
    message = f"Hello, {name}!"
    return message

print(greet("Python"))
```

```go
package main
func main() {
    message := "Hello from Go"
    println(message)
}
```

```toml
[demo]
title = "TOML sample"
enabled = true
count = 3
```

```kotlin
fun main() {
    val language: String = "Kotlin"
    println("Hello from $language")
}
```

```text
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

| Left | Center | Right |
| :--- | :----: | ----: |
| alpha | beta | 100 |
| gamma | delta | 200 |

| Name | Status | Owner | Description | Count | Notes |
| :--- | :----: | :--- | :---------- | ----: | :---- |
| Short | Ready | UI | This deliberately long table cell contains about sixty characters total. | 12 | wraps |
| Longer component name | Pending | Visual QA | Brief | 3 | compact |

- [x] Render headings
- [ ] Inspect every pixel

1. Ordered parent
   - Unordered child
     1. Nested ordered child
2. Second ordered item

- Unordered parent
  - Nested bullet
    1. Ordered grandchild

> First line of the blockquote.
> Second line of the blockquote.

Visit [Example](https://example.com) or the bare URL https://tcode.dev for more.

---

This paragraph has a soft
line break, followed by a hard break.\
This begins after the hard break."#;

        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let data_root = crate::store::tests::scratch_root("markdown-outer-list-test");
        let store = SessionStore::open_at(data_root).expect("test session store");
        let host = spawn_host(store, HostServices::default()).expect("spawn test host");
        let (session_id, timeline) =
            smol::block_on(host.update_state_for_test(move |state, cx| {
                let id = state.start_draft("markdown-test".into(), std::env::temp_dir(), cx);
                let active = state.residents.live.get_mut(&id).expect("selected draft");
                active.timeline = Timeline::default();
                active.timeline.turns = vec![TurnMeta::default()];
                active.timeline.entries = vec![
                    entry("user", user_item("render a long document")),
                    entry("assistant", assistant(DEMO_MARKDOWN)),
                ];
                active.draft = false;
                (active.meta.id.clone(), active.timeline.clone())
            }))
            .expect("seed markdown host");
        let mut driver = crate::store::tests::LocalHostDriver::new(&host);
        let workspace_store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                driver.link.clone(),
                crate::store::WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        driver.wait_until(cx, &workspace_store, "workspace baseline", |cx| {
            workspace_store.read_with(cx, |store, _| store.baseline_ready())
        });
        workspace_store.update(cx, |store, cx| store.show_alone(session_id.clone(), cx));
        driver.wait_until(cx, &workspace_store, "selected conversation", |cx| {
            workspace_store.read_with(cx, |store, cx| {
                store.baseline_ready()
                    && !store.chat_loading_for(store.sole_thread().as_deref(), cx)
            })
        });
        workspace_store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id.clone(), timeline, cx);
        });
        let window_state = cx.new(|_| WindowState::new(false));

        let (view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(
                workspace_store.clone(),
                window_state,
                Some(session_id.clone()),
                window,
                cx,
            )
        });
        let cx: &mut VisualTestContext = cx;
        cx.simulate_resize(size(px(1_024.), px(700.)));
        cx.run_until_parked();
        draw_settled(cx);
        cx.run_until_parked();
        draw_settled(cx);

        let outer_list = view.read_with(cx, |chat, _| chat.list_state.clone());
        let max_scroll = outer_list.max_offset_for_scrollbar().y;
        assert!(
            max_scroll > px(800.),
            "long markdown did not contribute its full height to the chat list: {max_scroll:?}"
        );
        let scroll_step = px(40.);
        let steps = (f32::from(max_scroll / 2.) / f32::from(scroll_step)).ceil() as usize;
        for step in 0..steps {
            let distance = (scroll_step * (step + 1) as f32).min(max_scroll / 2.);
            outer_list.set_offset_from_scrollbar(point(px(0.), -(max_scroll - distance)));
            view.update(cx, |_, cx| cx.notify());
            draw_settled(cx);
        }

        let viewport = outer_list.viewport_bounds();
        let middle = cx.debug_bounds("markdown-block-6");
        assert!(
            middle.is_some_and(|bounds| bounds.intersects(&viewport)),
            "middle markdown block did not paint at the chat list's middle offset; bounds={middle:?}, max_scroll={max_scroll:?}, viewport={viewport:?}"
        );
        assert!(
            cx.debug_bounds("markdown-block-18").is_none(),
            "offscreen tail block was painted; block virtualization did not cull it"
        );
        driver.shutdown(cx, &workspace_store, &host);
    }

    /// A user turn's rewind popover offers exactly the scopes its harness
    /// restores that turn in: Claude Code restores only files of the first
    /// turn and all three scopes of a later one; OpenCode restores the
    /// conversation, with or without files, of any turn, and never files alone.
    #[gpui::test]
    fn the_rewind_popover_offers_exactly_the_harness_rewind_modes(cx: &mut TestAppContext) {
        use agent::{ProviderKind, RewindMode};
        use gpui::{Modifiers, VisualTestContext, px, size};

        const ALL: [RewindMode; 3] = [
            RewindMode::FilesAndConversation,
            RewindMode::Conversation,
            RewindMode::Files,
        ];
        for (provider, turn, offered) in [
            (ProviderKind::ClaudeCode, 0, &[RewindMode::Files][..]),
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
            let (workspace_store, window_state, session_id, _driver) =
                seed_chat_with_history_on(cx, timeline, false, Some(provider));
            let (_view, window) = crate::chat::tests::add_chat_window(cx, |window, cx| {
                ChatView::new(workspace_store, window_state, Some(session_id), window, cx)
            });
            let window: &mut VisualTestContext = window;
            window.simulate_resize(size(px(1_024.), px(700.)));
            draw_settled(window);
            // The bubble's actions show while the pointer is over it.
            let bubble = window
                .debug_bounds(["user-bubble-user-0", "user-bubble-user-1"][turn])
                .expect("the user bubble is drawn");
            window.simulate_mouse_move(bubble.center(), None, Modifiers::default());
            draw_settled(window);
            let trigger = window
                .debug_bounds(["rewind-0", "rewind-1"][turn])
                .unwrap_or_else(|| panic!("{provider:?} turn {turn} has a rewind button"));
            window.simulate_click(trigger.center(), Modifiers::default());
            draw_settled(window);
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

    /// A running turn's working indicator keeps its pulse when the turn
    /// grows a new last row and the indicator moves into it.
    #[gpui::test]
    fn the_working_pulse_carries_on_when_its_turn_grows_a_row(cx: &mut TestAppContext) {
        use gpui::{VisualTestContext, px, size};

        let mut timeline = Timeline::default();
        timeline.turns = vec![TurnMeta {
            running: true,
            ..TurnMeta::default()
        }];
        timeline.entries = vec![entry("user", user_item("go")), command_with_output("first")];
        let (store, window_state, session_id) = seed_chat(cx, timeline.clone());
        let (_view, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
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
        // The pulse never rests while the turn runs: frames as a display
        // spaces them, on both clocks.
        let frames = |count: usize, cx: &mut VisualTestContext| {
            for _ in 0..count {
                std::thread::sleep(Duration::from_millis(16));
                cx.executor().advance_clock(Duration::from_millis(16));
                draw(cx);
            }
        };
        // The indicator's nine cells, in reading order.
        let cells = |cx: &mut VisualTestContext| {
            let status = cx
                .debug_bounds("working-status-0")
                .expect("the running turn's status");
            cx.update(|window, _| {
                let scale = window.scale_factor();
                let status = status.scale(scale);
                let cell = px(4.).scale(scale);
                let mut cells = window
                    .painted_quads()
                    .into_iter()
                    .filter(|quad| {
                        quad.bounds.size.width == cell
                            && quad.bounds.size.height == cell
                            && status.contains(&quad.bounds.origin)
                    })
                    .map(|quad| (quad.bounds.origin.y, quad.bounds.origin.x, quad.background))
                    .collect::<Vec<_>>();
                cells.sort_by(|a, b| a.0.0.total_cmp(&b.0.0).then(a.1.0.total_cmp(&b.1.0)));
                cells.into_iter().map(|cell| cell.2).collect::<Vec<_>>()
            })
        };

        frames(20, cx);
        let before = cells(cx);
        assert_eq!(before.len(), 9, "the indicator's pixel grid");
        timeline
            .entries
            .push(entry("reply", assistant("Looked at it.")));
        store.update(cx, |store, cx| {
            store.set_session_replica_for_test(session_id, timeline, cx);
            cx.notify();
        });
        draw(cx);
        assert_eq!(
            cells(cx),
            before,
            "the pulse restarted when the indicator moved into the new row"
        );
    }
}
