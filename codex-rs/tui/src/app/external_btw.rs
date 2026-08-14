//! Structured, externally controlled `/btw` side conversations.

use super::*;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;

#[derive(Debug)]
pub(super) struct ExternalBtwState {
    request_id: String,
    parent_thread_id: ThreadId,
    answer: Option<String>,
}

impl App {
    pub(super) async fn start_external_btw(
        &mut self,
        app_server: &mut AppServerSession,
        request_id: String,
        prompt: String,
    ) {
        let Some(parent_thread_id) = self.primary_thread_id else {
            crate::session_log::log_btw_failed(&request_id, None, None, "main_thread_unavailable");
            return;
        };
        if !self.external_btw_requests.is_empty() {
            crate::session_log::log_btw_failed(
                &request_id,
                Some(parent_thread_id),
                None,
                "btw_request_already_active",
            );
            return;
        }

        let fork_config = self.side_fork_config();
        let started = match app_server
            .fork_thread(fork_config.clone(), parent_thread_id)
            .await
        {
            Ok(started) => started,
            Err(err) => {
                crate::session_log::log_btw_failed(
                    &request_id,
                    Some(parent_thread_id),
                    None,
                    &format!("thread_fork_failed: {err}"),
                );
                return;
            }
        };
        let child_thread_id = started.session.thread_id;
        {
            let channel = self.ensure_thread_channel(child_thread_id);
            let mut store = channel.store.lock().await;
            Self::install_side_thread_snapshot(&mut store, started.session, started.turns);
        }

        if let Err(err) = app_server
            .thread_inject_items(child_thread_id, vec![Self::side_boundary_prompt_item()])
            .await
        {
            crate::session_log::log_btw_failed(
                &request_id,
                Some(parent_thread_id),
                Some(child_thread_id),
                &format!("thread_prepare_failed: {err}"),
            );
            self.app_event_tx.send(AppEvent::CleanupExternalBtw {
                thread_id: child_thread_id,
            });
            return;
        }

        self.external_btw_requests.insert(
            child_thread_id,
            ExternalBtwState {
                request_id: request_id.clone(),
                parent_thread_id,
                answer: None,
            },
        );

        let model = fork_config
            .model
            .clone()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| self.chat_widget.current_model().to_string());
        let op = AppCommand::user_turn(
            vec![UserInput::Text {
                text: prompt,
                text_elements: Vec::new(),
            }],
            fork_config.cwd.to_path_buf(),
            AskForApproval::from(fork_config.permissions.approval_policy.value()),
            fork_config.permissions.active_permission_profile(),
            model,
            fork_config.model_reasoning_effort,
            /*summary*/ None,
            Some(fork_config.service_tier.clone()),
            /*final_output_json_schema*/ None,
            /*collaboration_mode*/ None,
            fork_config.personality,
        );
        if let Err(err) = self.submit_thread_op(app_server, child_thread_id, op).await {
            self.external_btw_requests.remove(&child_thread_id);
            crate::session_log::log_btw_failed(
                &request_id,
                Some(parent_thread_id),
                Some(child_thread_id),
                &format!("turn_start_failed: {err}"),
            );
            self.app_event_tx.send(AppEvent::CleanupExternalBtw {
                thread_id: child_thread_id,
            });
            return;
        }

        crate::session_log::log_btw_started(&request_id, parent_thread_id, child_thread_id);
    }

    pub(super) fn handle_external_btw_notification(
        &mut self,
        thread_id: ThreadId,
        notification: &ServerNotification,
    ) -> bool {
        let Some(state) = self.external_btw_requests.get_mut(&thread_id) else {
            return false;
        };

        match notification {
            ServerNotification::ItemCompleted(event) => {
                if let ThreadItem::AgentMessage { text, .. } = &event.item
                    && !text.trim().is_empty()
                {
                    state.answer = Some(text.clone());
                }
            }
            ServerNotification::TurnCompleted(event) => {
                let state = self
                    .external_btw_requests
                    .remove(&thread_id)
                    .expect("external btw state must exist");
                match event.turn.status {
                    TurnStatus::Completed => {
                        if let Some(answer) =
                            state.answer.filter(|answer| !answer.trim().is_empty())
                        {
                            crate::session_log::log_btw_completed(
                                &state.request_id,
                                state.parent_thread_id,
                                thread_id,
                                &answer,
                            );
                        } else {
                            crate::session_log::log_btw_failed(
                                &state.request_id,
                                Some(state.parent_thread_id),
                                Some(thread_id),
                                "completed_without_agent_message",
                            );
                        }
                    }
                    TurnStatus::Failed | TurnStatus::Interrupted | TurnStatus::InProgress => {
                        crate::session_log::log_btw_failed(
                            &state.request_id,
                            Some(state.parent_thread_id),
                            Some(thread_id),
                            &format!("turn_{}", format!("{:?}", event.turn.status).to_lowercase()),
                        );
                    }
                }
                self.app_event_tx
                    .send(AppEvent::CleanupExternalBtw { thread_id });
            }
            ServerNotification::ThreadClosed(_) => {
                let state = self
                    .external_btw_requests
                    .remove(&thread_id)
                    .expect("external btw state must exist");
                crate::session_log::log_btw_failed(
                    &state.request_id,
                    Some(state.parent_thread_id),
                    Some(thread_id),
                    "thread_closed",
                );
                self.app_event_tx
                    .send(AppEvent::CleanupExternalBtw { thread_id });
            }
            _ => {}
        }

        true
    }

    pub(super) fn fail_external_btw(&mut self, thread_id: ThreadId, error: &str) -> bool {
        let Some(state) = self.external_btw_requests.remove(&thread_id) else {
            return false;
        };

        crate::session_log::log_btw_failed(
            &state.request_id,
            Some(state.parent_thread_id),
            Some(thread_id),
            error,
        );
        self.app_event_tx
            .send(AppEvent::CleanupExternalBtw { thread_id });
        true
    }

    pub(super) fn fail_all_external_btw(&mut self, error: &str) {
        for (thread_id, state) in self.external_btw_requests.drain() {
            crate::session_log::log_btw_failed(
                &state.request_id,
                Some(state.parent_thread_id),
                Some(thread_id),
                error,
            );
            self.app_event_tx
                .send(AppEvent::CleanupExternalBtw { thread_id });
        }
    }

    pub(super) async fn cleanup_external_btw(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        if let Err(err) = app_server.thread_unsubscribe(thread_id).await {
            tracing::warn!(%thread_id, %err, "failed to unsubscribe external btw thread");
        }
        self.discard_thread_local_state(thread_id).await;
    }
}

#[cfg(test)]
#[path = "external_btw_tests.rs"]
mod tests;
