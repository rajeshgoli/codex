use super::*;

impl ChatWidget {
    pub(crate) fn submit_external_literal_user_message(&mut self, text: String) {
        let Some((op, history_op, submitted_text)) =
            self.prepare_external_literal_user_message(text)
        else {
            return;
        };

        if !self.submit_op(op) {
            return;
        }
        self.mark_user_turn_pending_start();
        if let Some(history_op) = history_op {
            self.submit_op(history_op);
        }

        self.render_external_literal_user_message(submitted_text);
    }

    pub(super) fn submit_queued_external_literal_user_message(
        &mut self,
        queued_message: QueuedUserMessage,
    ) {
        let text = queued_message.into_user_message().text;
        if text.is_empty() {
            return;
        }
        if !self.is_session_configured() || self.is_user_turn_pending_or_running() {
            tracing::warn!("cannot submit queued external user message immediately; requeueing");
            self.queued_user_messages.push_front(QueuedUserMessage::new(
                UserMessage::from(text),
                QueuedInputAction::LiteralUserTurn,
            ));
            self.refresh_pending_input_preview();
            return;
        }
        let Some((op, history_op, submitted_text)) = self
            .prepare_external_literal_user_message_with_queueing(
                text, /*queue_if_unconfigured*/ false,
            )
        else {
            return;
        };

        if !self.submit_op(op) {
            return;
        }
        self.mark_user_turn_pending_start();
        if let Some(history_op) = history_op {
            self.submit_op(history_op);
        }
        self.render_external_literal_user_message(submitted_text);
    }

    pub(crate) fn queue_external_literal_user_message(&mut self, text: String) {
        self.queue_user_message_with_options(
            UserMessage::from(text),
            QueuedInputAction::LiteralUserTurn,
        );
    }

    pub(crate) fn is_user_turn_pending_start(&self) -> bool {
        self.user_turn_pending_start
    }

    pub(crate) fn prepare_external_literal_user_message(
        &mut self,
        text: String,
    ) -> Option<(AppCommand, Option<AppCommand>, String)> {
        self.prepare_external_literal_user_message_with_queueing(
            text, /*queue_if_unconfigured*/ true,
        )
    }

    pub(crate) fn prepare_targeted_external_literal_user_message(
        &mut self,
        text: String,
    ) -> Option<(AppCommand, Option<AppCommand>, String)> {
        self.prepare_external_literal_user_message_with_queueing(
            text, /*queue_if_unconfigured*/ false,
        )
    }

    pub(crate) fn prepare_targeted_external_literal_user_message_for_thread(
        &mut self,
        text: String,
        session: &ThreadSessionState,
        input_state: Option<&ThreadInputState>,
    ) -> Option<(AppCommand, Option<AppCommand>, String)> {
        if text.is_empty() {
            return None;
        }

        let items = vec![UserInput::Text {
            text: text.clone(),
            text_elements: Vec::new(),
        }];
        let base_mode = input_state
            .map(|state| state.current_collaboration_mode.clone())
            .unwrap_or_else(|| CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: session.model.clone(),
                    reasoning_effort: session.reasoning_effort,
                    developer_instructions: None,
                },
            });
        let effective_mode = input_state
            .and_then(|state| state.active_collaboration_mask.as_ref())
            .map(|mask| base_mode.apply_mask(mask))
            .unwrap_or(base_mode)
            .with_updates(
                Some(session.model.clone()),
                Some(session.reasoning_effort),
                /*developer_instructions*/ None,
            );
        if effective_mode.model().trim().is_empty() {
            self.add_error_message(
                "Target thread model is unavailable. Wait for the thread to finish syncing before sending input.".to_string(),
            );
            return None;
        }
        let collaboration_mode = if self.collaboration_modes_enabled() {
            input_state
                .and_then(|state| state.active_collaboration_mask.as_ref())
                .map(|_| effective_mode.clone())
        } else {
            None
        };
        let permission_profile = if matches!(
            session.sandbox_policy,
            SandboxPolicy::ExternalSandbox { .. }
        ) {
            None
        } else {
            session.permission_profile.clone()
        };
        let op = AppCommand::user_turn(
            items,
            session.cwd.to_path_buf(),
            session.approval_policy,
            session.sandbox_policy.clone(),
            permission_profile,
            effective_mode.model().to_string(),
            effective_mode.reasoning_effort(),
            /*summary*/ None,
            Some(session.service_tier),
            /*final_output_json_schema*/ None,
            collaboration_mode,
            /*personality*/ None,
        )
        .with_approvals_reviewer(session.approvals_reviewer);
        let history_op = Some(AppCommand::from(Op::AddToHistory { text: text.clone() }));

        Some((op, history_op, text))
    }

    pub(crate) fn record_pending_external_literal_steer(&mut self, text: String) {
        self.pending_steers.push_back(PendingSteer {
            user_message: UserMessage::from(text.clone()),
            compare_key: PendingSteerCompareKey {
                message: text,
                image_count: 0,
            },
            rejection_action: QueuedInputAction::LiteralUserTurn,
        });
        self.saw_plan_item_this_turn = false;
        self.refresh_pending_input_preview();
    }

    pub(crate) fn cancel_pending_external_literal_steer(&mut self, text: &str) {
        if self.pending_steers.back().is_some_and(|pending| {
            pending.rejection_action == QueuedInputAction::LiteralUserTurn
                && pending.user_message.text == text
        }) {
            self.pending_steers.pop_back();
            self.refresh_pending_input_preview();
        }
    }

    fn prepare_external_literal_user_message_with_queueing(
        &mut self,
        text: String,
        queue_if_unconfigured: bool,
    ) -> Option<(AppCommand, Option<AppCommand>, String)> {
        if text.is_empty() {
            return None;
        }
        if queue_if_unconfigured
            && (!self.is_session_configured() || self.is_user_turn_pending_or_running())
        {
            tracing::warn!("cannot submit external user message immediately; queueing");
            self.queued_user_messages.push_back(QueuedUserMessage::new(
                UserMessage {
                    text,
                    local_images: Vec::new(),
                    remote_image_urls: Vec::new(),
                    text_elements: Vec::new(),
                    mention_bindings: Vec::new(),
                },
                QueuedInputAction::LiteralUserTurn,
            ));
            self.refresh_pending_input_preview();
            return None;
        }

        let items = vec![UserInput::Text {
            text: text.clone(),
            text_elements: Vec::new(),
        }];
        let effective_mode = self.effective_collaboration_mode();
        if effective_mode.model().trim().is_empty() {
            self.add_error_message(
                "Thread model is unavailable. Wait for the thread to finish syncing or choose a model before sending input.".to_string(),
            );
            return None;
        }
        let collaboration_mode = if self.collaboration_modes_enabled() {
            self.active_collaboration_mask
                .as_ref()
                .map(|_| effective_mode.clone())
        } else {
            None
        };
        let personality = self
            .config
            .personality
            .filter(|_| self.config.features.enabled(Feature::Personality))
            .filter(|_| self.current_model_supports_personality());
        let service_tier = match self.config.service_tier {
            Some(service_tier) => Some(Some(service_tier)),
            None if self.config.notices.fast_default_opt_out == Some(true) => Some(None),
            None => None,
        };
        let permission_profile = if matches!(
            self.config.permissions.sandbox_policy.get(),
            SandboxPolicy::ExternalSandbox { .. }
        ) {
            None
        } else {
            Some(self.config.permissions.permission_profile())
        };
        let op = AppCommand::user_turn(
            items,
            self.config.cwd.to_path_buf(),
            self.config.permissions.approval_policy.value(),
            self.config.permissions.sandbox_policy.get().clone(),
            permission_profile,
            effective_mode.model().to_string(),
            effective_mode.reasoning_effort(),
            /*summary*/ None,
            service_tier,
            /*final_output_json_schema*/ None,
            collaboration_mode,
            personality,
        );
        let history_op = Some(AppCommand::from(Op::AddToHistory { text: text.clone() }));

        Some((op, history_op, text))
    }

    pub(crate) fn render_external_literal_user_message(&mut self, text: String) {
        self.last_rendered_user_message_event = Some(Self::rendered_user_message_event_from_parts(
            text.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        self.record_visible_user_turn_for_copy();
        self.add_to_history(history_cell::new_user_prompt(
            text,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        self.needs_final_message_separator = false;
    }
}

impl ThreadInputState {
    pub(crate) fn record_pending_external_literal_steer(&mut self, text: String) {
        self.pending_steers.push_back(PendingSteer::new(
            UserMessage::from(text),
            QueuedInputAction::LiteralUserTurn,
        ));
    }

    pub(crate) fn cancel_pending_external_literal_steer(&mut self, text: &str) {
        if self.pending_steers.back().is_some_and(|pending| {
            pending.rejection_action == QueuedInputAction::LiteralUserTurn
                && pending.user_message.text == text
        }) {
            self.pending_steers.pop_back();
        }
    }

    pub(crate) fn is_user_turn_pending_or_running(&self) -> bool {
        self.user_turn_pending_start || self.task_running
    }

    pub(crate) fn is_user_turn_pending_start(&self) -> bool {
        self.user_turn_pending_start
    }

    pub(crate) fn mark_user_turn_pending_start(&mut self) {
        self.user_turn_pending_start = true;
    }

    pub(crate) fn mark_user_turn_started(&mut self) {
        self.user_turn_pending_start = false;
        self.task_running = true;
        self.agent_turn_running = true;
    }

    pub(crate) fn mark_user_turn_completed(&mut self) {
        self.user_turn_pending_start = false;
        self.task_running = false;
        self.agent_turn_running = false;
    }

    pub(crate) fn enqueue_rejected_steer(&mut self) -> bool {
        let Some(pending_steer) = self.pending_steers.pop_front() else {
            tracing::warn!(
                "received active-turn-not-steerable error without a matching target pending steer"
            );
            return false;
        };
        match pending_steer.rejection_action {
            QueuedInputAction::Plain => self
                .rejected_steers_queue
                .push_back(pending_steer.user_message),
            QueuedInputAction::LiteralUserTurn
            | QueuedInputAction::ParseSlash
            | QueuedInputAction::RunShell => {
                self.queued_user_messages.push_front(QueuedUserMessage::new(
                    pending_steer.user_message,
                    pending_steer.rejection_action,
                ));
            }
        }
        true
    }
}
