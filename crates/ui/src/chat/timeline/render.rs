use super::*;

/// A whole tool output fetched on request.
pub(in crate::chat) enum FullOutput {
    /// Dropping the task cancels the fetch.
    Loading {
        _request: Task<()>,
    },
    Loaded(SharedString),
    Failed(String),
}

impl ChatView {
    /// Render one timeline row: the segment it spans (a message, an error, a
    /// Work Log run or a part of an expanded one) and, on the turn's last
    /// row, the turn's trailer.
    ///
    /// The trailer reads the whole turn (pending steers, the last
    /// timestamp). `pinned` carries the ids of the last user / last assistant
    /// message in the whole timeline: their action rows stay visible instead
    /// of waiting for a hover, so Copy is never invisible-and-hover-only.
    pub(in crate::chat) fn render_row(
        &mut self,
        args: RowRenderArgs<'_>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (row, turn, cwd, own, entries, pinned) = args;
        let index = row.turn;
        let mut column = v_flex().w_full().gap(px(SEGMENT_GAP));

        // A row's entries re-segment to the one segment it was indexed from.
        let segmented = segment_entries(own, turn.running);
        let segments = &segmented.flow;

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
                        cx,
                    ));
                }
                Segment::Images(images) => {
                    column = column.child(components::images::image_tiles(
                        images,
                        cwd,
                        self.session_key.as_deref().unwrap_or_default(),
                        cx,
                    ));
                    for entry in images {
                        if matches!(
                            &entry.content,
                            EntryContent::Item(ItemContent::ToolCall { .. })
                        ) {
                            column = column.child(self.compose_activity_row(
                                entry,
                                true,
                                false,
                                false,
                                AutoActivityRecency::Older,
                                cx,
                            ));
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
                    column = column.child(components::assistant::assistant(
                        components::assistant::AssistantData {
                            id: &entry.id,
                            text,
                            summarized: *summarized,
                            cwd,
                            markdown,
                            compact: self.window_state.read(cx).compact,
                            pinned: pinned.1 == Some(entry.id.as_str()),
                            show_actions: !turn.running && row.last_assistant,
                            copied,
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
                            let queued: Vec<_> = store
                                .composer_state_for(self.session_id.as_deref())
                                .queue
                                .into_iter()
                                .flat_map(|queue| queue.messages)
                                .collect();
                            let queued_id = queued
                                .iter()
                                .find(|message| message.fire_at_unix_secs == Some(resets_at))
                                .map(|message| message.id);
                            let queue_empty = queued.is_empty();
                            let ends_thread = store
                                .with_timeline_for(self.session_id.as_deref(), |timeline| {
                                    !timeline.turn_running && index + 1 == timeline.turns.len()
                                })
                                .unwrap_or(false);
                            match limit_resume_action(
                                resets_at,
                                queued_id,
                                now_secs(),
                                ends_thread,
                                queue_empty,
                            ) {
                                LimitResumeAction::Scheduled {
                                    queued_id: id,
                                    remaining_secs,
                                } => Some(components::error_card::LimitResume::Scheduled {
                                    remaining_secs,
                                    on_cancel: Box::new(cx.listener(move |this, _, _, cx| {
                                        let session_id =
                                            this.session_id.clone().unwrap_or_default();
                                        this.workspace_store.update(cx, |store, _| {
                                            store.drop_queued_for(session_id, id)
                                        });
                                    })),
                                }),
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
                        .with_timeline_for(self.session_id.as_deref(), |timeline| {
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
                                                matches!(entry.content, EntryContent::Error { .. })
                                            })
                                            .is_some_and(|last| last.id == entry.id)
                                })
                                .cloned()
                        })
                        .flatten();
                    let resume = retry
                        .map(|retry| {
                            let turn_id = retry.turn_id;
                            components::error_card::LimitResume::Retry {
                                remaining_secs: retry
                                    .retry_at
                                    .map(|at| at.saturating_sub(now_secs())),
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
                        cx,
                    ));
                }
            }
        }

        if !row.last_in_turn {
            return column.into_any_element();
        }

        // Changed-file evidence is the turn's settled summary: while the turn
        // still runs, the live per-file rows inside the work log carry this
        // information, so the section appears only once the turn finishes.
        let (changes, completeness) = self
            .workspace_store
            .read(cx)
            .chat_turn_changes_for(self.session_id.as_deref(), index);
        if !turn.running && !changes.is_empty() {
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
            column = column.child(components::changed_files::changed_files(
                index,
                cwd,
                &changes,
                completeness,
                show_all,
                handlers,
                cx,
            ));
        }

        // The turn's liveness, bare after everything it produced: no fold can
        // reach it, so collapsing the last disclosure never hides that we work.
        if turn.running {
            let requested_model = self
                .workspace_store
                .read(cx)
                .chat_requested_model_for(self.session_id.as_deref());
            let served_model =
                divergent_served_model(turn.served_model.as_deref(), requested_model.as_deref())
                    .map(str::to_owned);
            column = column.child(
                div()
                    .id(("working-status", index))
                    .debug_selector(move || format!("working-status-{index}"))
                    .child(components::indicator::turn_working_indicator(
                        index,
                        turn.start_ts,
                        served_model,
                        cx,
                    )),
            );
        } else if let Some(ts) = turn.end_ts.or(entries.last().and_then(|e| e.ts)) {
            let requested_model = self
                .workspace_store
                .read(cx)
                .chat_requested_model_for(self.session_id.as_deref());
            column = column.child(components::indicator::finished_turn_time(
                ts,
                turn,
                requested_model.as_deref(),
                cx,
            ));
        }

        // Pending steers float below every live transcript/work-log element.
        // They are read from the whole turn, not this row's segment, so FIFO
        // order holds without making their request-time position look
        // model-visible.
        for entry in segment_entries(entries, turn.running).pending_steers {
            column = column.child(self.compose_user_entry(
                (index, entry, cwd, pinned.0 == Some(entry.id.as_str())),
                window,
                cx,
            ));
        }

        column.into_any_element()
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

    fn open_diff(&mut self, turn: usize, path: Option<String>, cx: &mut Context<Self>) {
        self.workspace_store.update(cx, |store, cx| match path {
            Some(path) => store.open_diff_for_file(turn, path, cx),
            None => store.open_diff_for_turn(turn, cx),
        });
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
            .delivery_messages()
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
                self.workspace_store
                    .read(cx)
                    .chat_native_rewind_state_for(self.session_id.as_deref(), turn)
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
        let bubble = components::bubble::user_bubble(
            components::bubble::BubbleData {
                entry_id,
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

        let bubble = if let Some(author) = self
            .workspace_store
            .read(cx)
            .message_byline_for(self.session_id.as_deref(), entry_id)
        {
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
            !store.approval_delivery_pending(&request.id)
                && !store.native_subagent_readonly_for(self.session_id.as_deref())
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
            .session_status_for(self.session_id.as_deref())
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
            .session_status_for(self.session_id.as_deref())?
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
            store
                .sidebar_sessions()
                .iter()
                .chain(store.archived_sessions())
                .any(|meta| meta.id == child_id)
        };
        let store = self.workspace_store.clone();
        let lead = self.session_id.clone();
        let on_open = exists.then(|| {
            Box::new(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                crate::agents_panel::open_agent(
                    &store,
                    child_id.clone(),
                    lead.as_deref(),
                    window,
                    cx,
                );
            }) as components::subagent::ClickHandler
        });
        let toggle_key = key.clone();
        components::disclosure::callback_row(
            &key,
            event,
            callback,
            expanded,
            cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(turn, &toggle_key, cx);
            }),
            on_open,
            cx,
        )
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
            cx,
        )
    }

    /// Prepare one stateless Work Log capsule, or the part of an expanded one
    /// that `part` names.
    fn compose_work_log(
        &mut self,
        args: components::work_log::WorkLogArgs<'_>,
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
                let rows = self.compose_work_log_rows(activities, cwd, None, false, cx);
                return components::work_log::work_log_body(rows).into_any_element();
            }
            RowPart::WorkLogLive => {
                return v_flex()
                    .w_full()
                    .gap_1()
                    .children(self.compose_work_log_rows(
                        activities,
                        cwd,
                        live_reasoning_id,
                        running,
                        cx,
                    ))
                    .into_any_element();
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
            let duration =
                format_elapsed_deciseconds(activity_run_duration_ms(folded, turn, is_last));
            let outcome = work_log_outcome(turn, folded, is_last);
            let rows = if expanded && !header_only {
                self.compose_work_log_rows(folded, cwd, live_reasoning_id, false, cx)
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
                },
                cx.listener(move |this, _, _, cx| {
                    this.toggle_expanded(index, &toggle_section_key, cx);
                }),
                cx,
            ));
        }

        if !visible.is_empty() && !header_only {
            flow = flow.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .children(self.compose_work_log_rows(
                        visible,
                        cwd,
                        live_reasoning_id,
                        running,
                        cx,
                    )),
            );
        }

        flow.into_any_element()
    }

    fn compose_work_log_rows(
        &mut self,
        activities: &[&TimelineEntry],
        cwd: &Path,
        live_reasoning_id: Option<&str>,
        auto_expand: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        for (activity_index, entry) in activities.iter().enumerate() {
            let recency = AutoActivityRecency::from_newer_activity_count(
                activities.len() - activity_index - 1,
            );
            if let EntryContent::Item(ItemContent::FileChange { changes, .. }) = &entry.content {
                let turn = entry.turn;
                for (file_index, row) in live_edit_rows(changes, cwd).iter().enumerate() {
                    let key = format!("activity-{}-file-{file_index}", entry.id);
                    let enabled = auto_expand && row.counts.is_some();
                    let expanded =
                        self.auto_activity_expanded((turn, &entry.id), &key, enabled, recency, cx);
                    let inline_diff = (expanded && row.counts.is_some())
                        .then(|| self.inline_diffs.render(&key, row, cx));
                    let toggle_key = key.clone();
                    let entry_id = entry.id.clone();
                    rows.push(components::changed_files::file_edit_row(
                        &key,
                        row,
                        cwd,
                        expanded,
                        inline_diff,
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_activity_expanded(
                                (turn, &entry_id),
                                &toggle_key,
                                expanded,
                                cx,
                            );
                        }),
                        cx,
                    ));
                }
            } else {
                rows.push(self.compose_activity_row(
                    entry,
                    false,
                    live_reasoning_id == Some(entry.id.as_str()),
                    auto_expand,
                    recency,
                    cx,
                ));
            }
        }
        rows
    }

    /// Prepare one stateless Work Log activity component.
    fn compose_activity_row(
        &mut self,
        entry: &TimelineEntry,
        compact: bool,
        live_reasoning: bool,
        auto_expand: bool,
        recency: AutoActivityRecency,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if matches!(
            &entry.content,
            EntryContent::Item(ItemContent::Subagent { .. })
        ) {
            return self.compose_subagent_row(entry, cx);
        }
        let key = format!("activity-{}", entry.id);
        let turn = entry.turn;
        let is_command = matches!(
            &entry.content,
            EntryContent::Item(ItemContent::CommandExecution { .. })
        );
        let auto_enabled =
            auto_expand && is_command && self.workspace_store.read(cx).live_command_panel();
        let expanded =
            self.auto_activity_expanded((turn, &entry.id), &key, auto_enabled, recency, cx);
        let command_detail = if expanded {
            match &entry.content {
                EntryContent::Item(ItemContent::CommandExecution {
                    command, output, ..
                }) => {
                    let panel_id = entry.id.clone();
                    let session_id = self.session_id.clone();
                    let on_cols_change = cx.listener(move |_this, cols: &u16, window, cx| {
                        // Fires from `on_prepaint`, in the middle of laying the
                        // list out. Subscribe once the frame is done.
                        let cols = *cols;
                        let panel_id = panel_id.clone();
                        let session_id = session_id.clone();
                        cx.defer_in(window, move |this, _window, cx| {
                            this.workspace_store.update(cx, |store, _| {
                                store.watch_command_output_for(
                                    session_id.as_deref(),
                                    &panel_id,
                                    cols,
                                );
                            });
                        });
                    });
                    let store = self.workspace_store.read(cx);
                    let replica = store.command_output_for(self.session_id.as_deref(), &entry.id);
                    let screen = components::command_panel::OutputScreen {
                        watched: replica.and_then(|replica| replica.watched()),
                        frame: replica.and_then(|replica| replica.frame()),
                    };
                    Some(self.command_panels.borrow_mut().render(
                        entry,
                        command,
                        output,
                        screen,
                        Some(Box::new(on_cols_change)),
                        cx,
                    ))
                }
                _ => None,
            }
        } else {
            None
        };
        let elided_output = if expanded
            && matches!(
                &entry.content,
                EntryContent::Item(ItemContent::ToolCall { .. })
            ) {
            self.elided_output(&entry.id, turn, cx)
        } else {
            None
        };
        let click_key = key;
        let entry_id = entry.id.clone();
        components::activity::activity_row(
            entry,
            compact,
            live_reasoning,
            expanded,
            command_detail,
            elided_output,
            cx.listener(move |this, _, _, cx| {
                this.toggle_activity_expanded((turn, &entry_id), &click_key, expanded, cx);
            }),
            cx,
        )
    }

    /// How much of a shortened tool output the reader has, or `None` when the
    /// record carried it whole.
    fn elided_output(
        &mut self,
        item_id: &str,
        turn: usize,
        cx: &mut Context<Self>,
    ) -> Option<components::activity::ElidedOutput> {
        use components::activity::ElidedOutput;
        let full_bytes = self
            .workspace_store
            .read(cx)
            .with_timeline_for(self.session_id.as_deref(), |timeline| {
                timeline.elided_outputs.get(item_id).copied()
            })??;
        let load = || -> components::subagent::ClickHandler {
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
                    Ok(output) => FullOutput::Loaded(output.into()),
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
        changes: components::command_panel::PanelChanges,
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

    fn compose_subagent_row(&self, entry: &TimelineEntry, cx: &mut Context<Self>) -> AnyElement {
        let active_id = self.session_id.clone();
        let mirror_id = active_id.as_deref().and_then(|active_id| {
            self.workspace_store
                .read(cx)
                .sidebar_sessions()
                .into_iter()
                .find(|meta| {
                    meta.parent_session_id.as_deref() == Some(active_id)
                        && meta.native_subagent.as_deref() == Some(entry.id.as_str())
                })
                .map(|meta| meta.id)
        });
        let shown = mirror_id
            .as_deref()
            .is_some_and(|mirror_id| crate::shell::shown_in_pane(mirror_id, cx));
        let on_open = mirror_id.map(|mirror_id| {
            let store = self.workspace_store.clone();
            Box::new(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                crate::agents_panel::open_agent(
                    &store,
                    mirror_id.clone(),
                    active_id.as_deref(),
                    window,
                    cx,
                );
            }) as components::subagent::ClickHandler
        });
        components::subagent::subagent_row(entry, on_open, shown, cx)
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
