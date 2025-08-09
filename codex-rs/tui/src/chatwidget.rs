use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use codex_core::codex_wrapper::CodexConversation;
use codex_core::codex_wrapper::init_codex;
use codex_core::config::Config;
use codex_core::protocol::AgentMessageDeltaEvent;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningDeltaEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::AgentReasoningRawContentDeltaEvent;
use codex_core::protocol::AgentReasoningRawContentEvent;
use codex_core::protocol::ApplyPatchApprovalRequestEvent;
use codex_core::protocol::BackgroundEventEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecApprovalRequestEvent;
use codex_core::protocol::ExecCommandBeginEvent;
use codex_core::protocol::ExecCommandEndEvent;
use codex_core::protocol::InputItem;
use codex_core::protocol::McpToolCallBeginEvent;
use codex_core::protocol::McpToolCallEndEvent;
use codex_core::protocol::Op;
use codex_core::protocol::PatchApplyBeginEvent;
use codex_core::protocol::TaskCompleteEvent;
use codex_core::protocol::TokenUsage;
use codex_core::protocol::TurnDiffEvent;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tracing::debug;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::InputResult;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::history_cell::CommandOutput;
use crate::history_cell::HistoryCell;
use crate::history_cell::PatchEventType;
// streaming internals are provided by crate::streaming and crate::markdown_stream
use crate::user_approval_widget::ApprovalRequest;
use codex_file_search::FileMatch;

struct RunningCommand {
    command: Vec<String>,
    #[allow(dead_code)]
    cwd: PathBuf,
}

pub(crate) struct ChatWidget<'a> {
    app_event_tx: AppEventSender,
    codex_op_tx: UnboundedSender<Op>,
    bottom_pane: BottomPane<'a>,
    active_history_cell: Option<HistoryCell>,
    config: Config,
    initial_user_message: Option<UserMessage>,
    total_token_usage: TokenUsage,
    last_token_usage: TokenUsage,
    // Newline-gated markdown streaming state, grouped per stream kind
    reasoning: StreamState,
    answer: StreamState,
    running_commands: HashMap<String, RunningCommand>,
    current_stream: Option<StreamKind>,
    // Currently active stream kind, or None if idle
    task_complete_pending: bool,
    finishing_after_drain: bool,
    // Queue of interruptive UI events deferred during an active write cycle
    interrupt_queue: VecDeque<QueuedInterrupt>,
    // Header emission state and logic
    header: crate::streaming::HeaderEmitter,
}

struct UserMessage {
    text: String,
    image_paths: Vec<PathBuf>,
}

use crate::streaming::StreamKind;

use crate::streaming::StreamState;

use crate::streaming::HeaderEmitter;

#[derive(Debug)]
enum QueuedInterrupt {
    ExecApproval(String, ExecApprovalRequestEvent),
    ApplyPatchApproval(String, ApplyPatchApprovalRequestEvent),
    ExecBegin(ExecCommandBeginEvent),
    McpBegin(McpToolCallBeginEvent),
    McpEnd(McpToolCallEndEvent),
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            image_paths: Vec::new(),
        }
    }
}

fn create_initial_user_message(text: String, image_paths: Vec<PathBuf>) -> Option<UserMessage> {
    if text.is_empty() && image_paths.is_empty() {
        None
    } else {
        Some(UserMessage { text, image_paths })
    }
}

impl ChatWidget<'_> {
    /// Compute the subset of `rendered_final` that still needs to be inserted into history.
    ///
    /// Invariant between collector and streamer:
    /// - `MarkdownStreamCollector::committed_count()` tracks how many logical lines have been
    ///   committed so far during streaming.
    /// - `AnimatedLineStreamer::len()` returns how many of those committed lines are still queued
    ///   for insertion into history (i.e., have not yet been inserted by commit ticks).
    ///
    /// Therefore the number of lines that have already been inserted into history is
    /// `committed_count - queued_len`. To avoid duplication on finalization, we skip that many
    /// lines from the fully rendered final output and return only the remaining tail.
    fn compute_remaining_final_lines_for_state(
        &self,
        state: &StreamState,
        rendered_final: &[ratatui::text::Line<'static>],
    ) -> Vec<ratatui::text::Line<'static>> {
        let committed = state.collector.committed_count();
        let queued = state.streamer.len();
        let already_inserted = committed.saturating_sub(queued);
        if already_inserted >= rendered_final.len() {
            Vec::new()
        } else {
            rendered_final[already_inserted..].to_vec()
        }
    }
    fn state_mut(&mut self, kind: StreamKind) -> &mut StreamState {
        match kind {
            StreamKind::Reasoning => &mut self.reasoning,
            StreamKind::Answer => &mut self.answer,
        }
    }

    fn active_kind_and_state_mut(&mut self) -> Option<(StreamKind, &mut StreamState)> {
        match self.current_stream {
            Some(StreamKind::Reasoning) => Some((StreamKind::Reasoning, &mut self.reasoning)),
            Some(StreamKind::Answer) => Some((StreamKind::Answer, &mut self.answer)),
            None => None,
        }
    }

    fn emit_header_if_needed(
        &mut self,
        kind: StreamKind,
        out_lines: &mut Vec<ratatui::text::Line<'static>>,
    ) {
        if self.header.maybe_emit(kind, out_lines) {
            debug!(target: "chatwidget", "emit_header: kind={:?}", kind);
        }
    }
    fn header_line(kind: StreamKind) -> ratatui::text::Line<'static> {
        use ratatui::style::Stylize;
        match kind {
            StreamKind::Reasoning => ratatui::text::Line::from("thinking".magenta().italic()),
            StreamKind::Answer => ratatui::text::Line::from("codex".magenta().bold()),
        }
    }
    /// Periodic tick to commit at most one queued line to history with a small delay,
    /// animating the output.
    pub(crate) fn on_commit_tick(&mut self) {
        // Choose the active streamer
        let Some(kind) = self.current_stream else {
            // No active stream. Nothing to animate.
            return;
        };

        // Prepare header if needed
        let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
        self.emit_header_if_needed(kind, &mut lines);

        let step = {
            let state = self.state_mut(kind);
            state.step()
        };
        if !step.history.is_empty() || !lines.is_empty() {
            debug!(target: "chatwidget", "commit_tick: kind={:?} lines_emitted={} (including header={})", kind, step.history.len(), !lines.is_empty());
            lines.extend(step.history);
            self.app_event_tx.send(AppEvent::InsertHistory(lines));
        }

        // If streamer is now idle and there is no more active stream data, finalize state.
        let is_idle = self.state_mut(kind).is_idle();
        if is_idle {
            // Stop animation ticks between bursts.
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
            if self.finishing_after_drain {
                // Final cleanup once fully drained at end-of-stream.
                // Reset stream state so the next request emits headers again.
                if let Some(kind_finished) = self.current_stream {
                    self.state_mut(kind_finished).clear();
                }
                self.current_stream = None;
                self.finishing_after_drain = false;
                if self.task_complete_pending {
                    self.bottom_pane.set_task_running(false);
                    self.task_complete_pending = false;
                }
                // After the write cycle completes, release any queued interrupts.
                self.flush_interrupt_queue();
            }
        }
    }
    fn is_write_cycle_active(&self) -> bool {
        self.current_stream.is_some()
    }

    fn flush_interrupt_queue(&mut self) {
        while let Some(q) = self.interrupt_queue.pop_front() {
            match q {
                QueuedInterrupt::ExecApproval(id, ev) => self.handle_exec_approval_now(id, ev),
                QueuedInterrupt::ApplyPatchApproval(id, ev) => {
                    self.handle_apply_patch_approval_now(id, ev)
                }
                QueuedInterrupt::ExecBegin(ev) => self.handle_exec_begin_now(ev),
                QueuedInterrupt::McpBegin(ev) => self.handle_mcp_begin_now(ev),
                QueuedInterrupt::McpEnd(ev) => self.handle_mcp_end_now(ev),
            }
        }
    }

    fn handle_exec_approval_now(&mut self, id: String, ev: ExecApprovalRequestEvent) {
        // Log a background summary immediately so the history is chronological.
        let cmdline = strip_bash_lc_and_escape(&ev.command);
        let text = format!(
            "command requires approval:\n$ {cmdline}{reason}",
            reason = ev
                .reason
                .as_ref()
                .map(|r| format!("\n{r}"))
                .unwrap_or_default()
        );
        self.add_to_history(HistoryCell::new_background_event(text));

        let request = ApprovalRequest::Exec {
            id,
            command: ev.command,
            cwd: ev.cwd,
            reason: ev.reason,
        };
        self.bottom_pane.push_approval_request(request);
        self.request_redraw();
    }

    fn handle_apply_patch_approval_now(&mut self, id: String, ev: ApplyPatchApprovalRequestEvent) {
        self.add_to_history(HistoryCell::new_patch_event(
            PatchEventType::ApprovalRequest,
            ev.changes.clone(),
        ));

        let request = ApprovalRequest::ApplyPatch {
            id,
            reason: ev.reason,
            grant_root: ev.grant_root,
        };
        self.bottom_pane.push_approval_request(request);
        self.request_redraw();
    }

    fn handle_exec_begin_now(&mut self, ev: ExecCommandBeginEvent) {
        // Ensure the status indicator is visible while the command runs.
        self.bottom_pane
            .update_status_text("running command".to_string());
        self.running_commands.insert(
            ev.call_id.clone(),
            RunningCommand {
                command: ev.command.clone(),
                cwd: ev.cwd.clone(),
            },
        );
        self.active_history_cell = Some(HistoryCell::new_active_exec_command(ev.command));
    }

    fn handle_mcp_begin_now(&mut self, ev: McpToolCallBeginEvent) {
        self.add_to_history(HistoryCell::new_active_mcp_tool_call(ev.invocation));
    }

    fn handle_mcp_end_now(&mut self, ev: McpToolCallEndEvent) {
        self.add_to_history(HistoryCell::new_completed_mcp_tool_call(
            80,
            ev.invocation,
            ev.duration,
            ev.result
                .as_ref()
                .map(|r| !r.is_error.unwrap_or(false))
                .unwrap_or(false),
            ev.result,
        ));
    }
    fn interrupt_running_task(&mut self) {
        if self.bottom_pane.is_task_running() {
            self.active_history_cell = None;
            self.bottom_pane.clear_ctrl_c_quit_hint();
            self.submit_op(Op::Interrupt);
            self.bottom_pane.set_task_running(false);
            self.reasoning.clear();
            self.answer.clear();
            self.current_stream = None;
            self.request_redraw();
        }
    }
    fn layout_areas(&self, area: Rect) -> [Rect; 2] {
        Layout::vertical([
            Constraint::Max(
                self.active_history_cell
                    .as_ref()
                    .map_or(0, |c| c.desired_height(area.width)),
            ),
            Constraint::Min(self.bottom_pane.desired_height(area.width)),
        ])
        .areas(area)
    }

    pub(crate) fn new(
        config: Config,
        app_event_tx: AppEventSender,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        enhanced_keys_supported: bool,
    ) -> Self {
        let (codex_op_tx, mut codex_op_rx) = unbounded_channel::<Op>();

        let app_event_tx_clone = app_event_tx.clone();
        // Create the Codex asynchronously so the UI loads as quickly as possible.
        let config_for_agent_loop = config.clone();
        tokio::spawn(async move {
            let CodexConversation {
                codex,
                session_configured,
                ..
            } = match init_codex(config_for_agent_loop).await {
                Ok(vals) => vals,
                Err(e) => {
                    // TODO: surface this error to the user.
                    tracing::error!("failed to initialize codex: {e}");
                    return;
                }
            };

            // Forward the captured `SessionInitialized` event that was consumed
            // inside `init_codex()` so it can be rendered in the UI.
            app_event_tx_clone.send(AppEvent::CodexEvent(session_configured.clone()));
            let codex = Arc::new(codex);
            let codex_clone = codex.clone();
            tokio::spawn(async move {
                while let Some(op) = codex_op_rx.recv().await {
                    let id = codex_clone.submit(op).await;
                    if let Err(e) = id {
                        tracing::error!("failed to submit op: {e}");
                    }
                }
            });

            while let Ok(event) = codex.next_event().await {
                app_event_tx_clone.send(AppEvent::CodexEvent(event));
            }
        });

        Self {
            app_event_tx: app_event_tx.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
            }),
            active_history_cell: None,
            config,
            initial_user_message: create_initial_user_message(
                initial_prompt.unwrap_or_default(),
                initial_images,
            ),
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage::default(),
            reasoning: StreamState::new(),
            answer: StreamState::new(),
            running_commands: HashMap::new(),
            current_stream: None,
            task_complete_pending: false,
            finishing_after_drain: false,
            interrupt_queue: VecDeque::new(),
            header: HeaderEmitter::new(),
        }
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.bottom_pane.desired_height(width)
            + self
                .active_history_cell
                .as_ref()
                .map_or(0, |c| c.desired_height(width))
    }

    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Press {
            self.bottom_pane.clear_ctrl_c_quit_hint();
        }

        match self.bottom_pane.handle_key_event(key_event) {
            InputResult::Submitted(text) => {
                self.submit_user_message(text.into());
            }
            InputResult::None => {}
        }
    }

    pub(crate) fn handle_paste(&mut self, text: String) {
        self.bottom_pane.handle_paste(text);
    }

    fn add_to_history(&mut self, cell: HistoryCell) {
        self.app_event_tx
            .send(AppEvent::InsertHistory(cell.plain_lines()));
    }

    fn submit_user_message(&mut self, user_message: UserMessage) {
        let UserMessage { text, image_paths } = user_message;
        let mut items: Vec<InputItem> = Vec::new();

        if !text.is_empty() {
            items.push(InputItem::Text { text: text.clone() });
        }

        for path in image_paths {
            items.push(InputItem::LocalImage { path });
        }

        if items.is_empty() {
            return;
        }

        self.codex_op_tx
            .send(Op::UserInput { items })
            .unwrap_or_else(|e| {
                tracing::error!("failed to send message: {e}");
            });

        // Persist the text to cross-session message history.
        if !text.is_empty() {
            self.codex_op_tx
                .send(Op::AddToHistory { text: text.clone() })
                .unwrap_or_else(|e| {
                    tracing::error!("failed to send AddHistory op: {e}");
                });
        }

        // Only show text portion in conversation history for now.
        if !text.is_empty() {
            self.add_to_history(HistoryCell::new_user_prompt(text.clone()));
        }
    }

    pub(crate) fn handle_codex_event(&mut self, event: Event) {
        let Event { id, msg } = event;
        match msg {
            EventMsg::SessionConfigured(event) => {
                self.bottom_pane
                    .set_history_metadata(event.history_log_id, event.history_entry_count);
                // Record session information at the top of the conversation.
                self.add_to_history(HistoryCell::new_session_info(&self.config, event, true));

                if let Some(user_message) = self.initial_user_message.take() {
                    // If the user provided an initial message, add it to the
                    // conversation history.
                    self.submit_user_message(user_message);
                }

                self.request_redraw();
            }
            EventMsg::AgentMessage(AgentMessageEvent { message }) => {
                // Final assistant answer. Ensure the final text is present in the
                // collector even if the active stream was torn down (e.g. due to
                // earlier finalize of another stream) before this event arrived.
                let answer_started =
                    self.header.has_emitted_for_stream(StreamKind::Answer)
                        || self.current_stream == Some(StreamKind::Answer);

                if !message.is_empty() {
                    // Ensure we're in Answer stream.
                    if self.current_stream != Some(StreamKind::Answer) || !answer_started {
                        self.begin_stream(StreamKind::Answer);
                    }

                    // Compute fully rendered final lines.
                    let mut rendered_final: Vec<ratatui::text::Line<'static>> = Vec::new();
                    let mut msg_with_nl = message.clone();
                    if !msg_with_nl.ends_with('\n') {
                        msg_with_nl.push('\n');
                    }
                    crate::markdown::append_markdown(
                        &msg_with_nl,
                        &mut rendered_final,
                        &self.config,
                    );

                    // Replace streamer contents with only the remaining lines beyond what
                    // has already been inserted into history.
                    let remaining = self
                        .compute_remaining_final_lines_for_state(&self.answer, &rendered_final);
                    self.answer.streamer.clear();
                    if !remaining.is_empty() {
                        self.answer.streamer.enqueue(remaining);
                    }
                    // Update collector to reflect that the entire final message is now the buffer
                    // and all of it is considered committed from the collector's perspective, so
                    // finalize will not try to enqueue more.
                    self.answer
                        .collector
                        .replace_with_and_mark_committed(&msg_with_nl, rendered_final.len());
                }

                // Commit remaining rows and close with a trailing blank line.
                // Flush immediately so the final answer is visible even if no further
                // animation ticks are driven by the UI.
                self.finalize_stream(StreamKind::Answer, true);
                self.request_redraw();
            }
            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
                self.begin_stream(StreamKind::Answer);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta }) => {
                // Stream CoT into the live pane; keep input visible and commit
                // overflow rows incrementally to scrollback.
                self.begin_stream(StreamKind::Reasoning);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text: _ }) => {
                // Final reasoning: commit remaining rows and close with a blank.
                // Keep animation for reasoning; no need to flush immediately.
                self.finalize_stream(StreamKind::Reasoning, false);
                self.request_redraw();
            }
            EventMsg::AgentReasoningSectionBreak(_) => {
                // Ensure the next reasoning section starts on its own, and avoid duplicating separators.
                self.begin_stream(StreamKind::Reasoning);
                let cfg = self.config.clone();
                self.reasoning.collector.insert_section_break();
                let newly_completed = self.reasoning.collector.commit_complete_lines(&cfg);
                if !newly_completed.is_empty() {
                    self.reasoning.streamer.enqueue(newly_completed);
                    self.app_event_tx.send(AppEvent::StartCommitAnimation);
                }
            }
            EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent {
                delta,
            }) => {
                // Treat raw reasoning content the same as summarized reasoning for UI flow.
                self.begin_stream(StreamKind::Reasoning);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text: _ }) => {
                // Finalize the raw reasoning stream just like the summarized reasoning event.
                self.finalize_stream(StreamKind::Reasoning, false);
                self.request_redraw();
            }
            EventMsg::TaskStarted => {
                self.bottom_pane.clear_ctrl_c_quit_hint();
                self.bottom_pane.set_task_running(true);
                // Replace composer with single-line spinner while waiting.
                self.bottom_pane
                    .update_status_text("waiting for model".to_string());
                // New turn: reset header emission state so each header can be emitted once.
                self.header.reset_for_new_turn();
                self.request_redraw();
            }
            EventMsg::TaskComplete(TaskCompleteEvent {
                last_agent_message: _,
            }) => {
                // Defer clearing status/live ring until streaming fully completes.
                let streaming_active = match self.current_stream {
                    Some(StreamKind::Reasoning) => !self.reasoning.streamer.is_idle(),
                    Some(StreamKind::Answer) => !self.answer.streamer.is_idle(),
                    None => false,
                };
                if streaming_active {
                    self.task_complete_pending = true;
                } else {
                    self.bottom_pane.set_task_running(false);
                    self.request_redraw();
                }
            }
            EventMsg::TokenCount(token_usage) => {
                self.total_token_usage = add_token_usage(&self.total_token_usage, &token_usage);
                self.last_token_usage = token_usage;
                self.bottom_pane.set_token_usage(
                    self.total_token_usage.clone(),
                    self.last_token_usage.clone(),
                    self.config.model_context_window,
                );
            }
            EventMsg::Error(ErrorEvent { message }) => {
                self.add_to_history(HistoryCell::new_error_event(message.clone()));
                self.bottom_pane.set_task_running(false);
                self.reasoning.clear();
                self.answer.clear();
                self.current_stream = None;
                self.request_redraw();
            }
            EventMsg::PlanUpdate(update) => {
                // Commit plan updates directly to history (no status-line preview).
                self.add_to_history(HistoryCell::new_plan_update(update));
            }
            EventMsg::ExecApprovalRequest(ev) => {
                if self.is_write_cycle_active() {
                    self.interrupt_queue
                        .push_back(QueuedInterrupt::ExecApproval(id, ev));
                } else {
                    self.handle_exec_approval_now(id, ev);
                }
            }
            EventMsg::ApplyPatchApprovalRequest(ev) => {
                if self.is_write_cycle_active() {
                    self.interrupt_queue
                        .push_back(QueuedInterrupt::ApplyPatchApproval(id, ev));
                } else {
                    self.handle_apply_patch_approval_now(id, ev);
                }
            }
            EventMsg::ExecCommandBegin(ev) => {
                if self.is_write_cycle_active() {
                    self.interrupt_queue
                        .push_back(QueuedInterrupt::ExecBegin(ev));
                } else {
                    self.handle_exec_begin_now(ev);
                }
            }
            EventMsg::ExecCommandOutputDelta(_) => {
                // TODO
            }
            EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: _,
                auto_approved,
                changes,
            }) => {
                self.add_to_history(HistoryCell::new_patch_event(
                    PatchEventType::ApplyBegin { auto_approved },
                    changes,
                ));
            }
            EventMsg::PatchApplyEnd(event) => {
                if event.success {
                    self.add_to_history(HistoryCell::new_patch_apply_success(event.stdout));
                } else {
                    self.add_to_history(HistoryCell::new_patch_apply_failure(event.stderr));
                }
            }
            EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id,
                exit_code,
                duration: _,
                stdout,
                stderr,
            }) => {
                // Compute summary before moving stdout into the history cell.
                let cmd = self.running_commands.remove(&call_id);
                self.active_history_cell = None;
                self.add_to_history(HistoryCell::new_completed_exec_command(
                    cmd.map(|cmd| cmd.command).unwrap_or_else(|| vec![call_id]),
                    CommandOutput {
                        exit_code,
                        stdout,
                        stderr,
                    },
                ));
            }
            EventMsg::McpToolCallBegin(ev) => {
                if self.is_write_cycle_active() {
                    self.interrupt_queue
                        .push_back(QueuedInterrupt::McpBegin(ev));
                } else {
                    self.handle_mcp_begin_now(ev);
                }
            }
            EventMsg::McpToolCallEnd(ev) => {
                if self.is_write_cycle_active() {
                    self.interrupt_queue.push_back(QueuedInterrupt::McpEnd(ev));
                } else {
                    self.handle_mcp_end_now(ev);
                }
            }
            EventMsg::GetHistoryEntryResponse(event) => {
                let codex_core::protocol::GetHistoryEntryResponseEvent {
                    offset,
                    log_id,
                    entry,
                } = event;

                // Inform bottom pane / composer.
                self.bottom_pane
                    .on_history_entry_response(log_id, offset, entry.map(|e| e.text));
            }
            EventMsg::ShutdownComplete => {
                self.app_event_tx.send(AppEvent::ExitRequest);
            }
            EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) => {
                debug!("TurnDiffEvent: {unified_diff}");
            }
            EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
                debug!("BackgroundEvent: {message}");
            }
        }
    }

    /// Update the live log preview while a task is running.
    pub(crate) fn update_latest_log(&mut self, line: String) {
        if self.bottom_pane.is_task_running() {
            self.bottom_pane.update_status_text(line);
        }
    }

    fn request_redraw(&mut self) {
        self.app_event_tx.send(AppEvent::RequestRedraw);
    }

    pub(crate) fn add_diff_output(&mut self, diff_output: String) {
        self.add_to_history(HistoryCell::new_diff_output(diff_output.clone()));
    }

    pub(crate) fn add_status_output(&mut self) {
        self.add_to_history(HistoryCell::new_status_output(
            &self.config,
            &self.total_token_usage,
        ));
    }

    pub(crate) fn add_prompts_output(&mut self) {
        self.add_to_history(HistoryCell::new_prompts_output());
    }

    /// Forward file-search results to the bottom pane.
    pub(crate) fn apply_file_search_result(&mut self, query: String, matches: Vec<FileMatch>) {
        self.bottom_pane.on_file_search_result(query, matches);
    }

    /// Handle Ctrl-C key press.
    /// Returns CancellationEvent::Handled if the event was consumed by the UI, or
    /// CancellationEvent::Ignored if the caller should handle it (e.g. exit).
    pub(crate) fn on_ctrl_c(&mut self) -> CancellationEvent {
        match self.bottom_pane.on_ctrl_c() {
            CancellationEvent::Handled => return CancellationEvent::Handled,
            CancellationEvent::Ignored => {}
        }
        if self.bottom_pane.is_task_running() {
            self.interrupt_running_task();
            CancellationEvent::Ignored
        } else if self.bottom_pane.ctrl_c_quit_hint_visible() {
            self.submit_op(Op::Shutdown);
            CancellationEvent::Handled
        } else {
            self.bottom_pane.show_ctrl_c_quit_hint();
            CancellationEvent::Ignored
        }
    }

    pub(crate) fn on_ctrl_z(&mut self) {
        self.interrupt_running_task();
    }

    pub(crate) fn composer_is_empty(&self) -> bool {
        self.bottom_pane.composer_is_empty()
    }

    /// Forward an `Op` directly to codex.
    pub(crate) fn submit_op(&self, op: Op) {
        // Record outbound operation for session replay fidelity.
        crate::session_log::log_outbound_op(&op);
        if let Err(e) = self.codex_op_tx.send(op) {
            tracing::error!("failed to submit op: {e}");
        }
    }

    /// Programmatically submit a user text message as if typed in the
    /// composer. The text will be added to conversation history and sent to
    /// the agent.
    pub(crate) fn submit_text_message(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.submit_user_message(text.into());
    }

    pub(crate) fn token_usage(&self) -> &TokenUsage {
        &self.total_token_usage
    }

    pub(crate) fn clear_token_usage(&mut self) {
        self.total_token_usage = TokenUsage::default();
        self.bottom_pane.set_token_usage(
            self.total_token_usage.clone(),
            self.last_token_usage.clone(),
            self.config.model_context_window,
        );
    }

    pub fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let [_, bottom_pane_area] = self.layout_areas(area);
        self.bottom_pane.cursor_pos(bottom_pane_area)
    }
}

impl ChatWidget<'_> {
    fn begin_stream(&mut self, kind: StreamKind) {
        if let Some(current) = self.current_stream {
            if current != kind {
                // Synchronously flush only fully completed lines from the previous
                // stream to keep ordering sane, but avoid forcing partial lines
                // (no trailing newline) into history. This prevents the first
                // answer line from appearing before its header when streams
                // alternate.
                let cfg = self.config.clone();
                let prev_state = self.state_mut(current);
                let newly_completed = prev_state.collector.commit_complete_lines(&cfg);
                if !newly_completed.is_empty() {
                    prev_state.enqueue(newly_completed);
                }
                let step = prev_state.drain_all();
                if !step.history.is_empty() || !self.header.has_emitted_for_stream(current) {
                    let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
                    self.emit_header_if_needed(current, &mut lines);
                    lines.extend(step.history);
                    // Ensure at most one blank separator after the flushed block.
                    if let Some(last) = lines.last() {
                        if !crate::line_utils::is_blank_line_trim(last) {
                            lines.push(ratatui::text::Line::from(""));
                        }
                    } else {
                        lines.push(ratatui::text::Line::from(""));
                    }
                    self.app_event_tx.send(AppEvent::InsertHistory(lines));
                }
                // Reset for new stream
                self.current_stream = None;
            }
        }

        if self.current_stream != Some(kind) {
            // Only reset the header flag when switching FROM a different stream kind.
            // If current_stream is None (e.g., transient idle), preserve header flags
            // to avoid duplicate headers on re-entry into the same stream.
            let prev = self.current_stream;
            self.current_stream = Some(kind);
            debug!(target: "chatwidget", "begin_stream: {:?} (prev={:?})", kind, prev);
            if prev.is_some() {
                self.header.reset_for_stream(kind);
            }
            // Ensure the waiting status is visible (composer replaced).
            self.bottom_pane
                .update_status_text("waiting for model".to_string());
            // Emit immediately only for reasoning; for answers, defer header
            // to the first committed line to keep content contiguous.
            if matches!(kind, StreamKind::Reasoning) {
                let mut header_lines = Vec::new();
                self.emit_header_if_needed(kind, &mut header_lines);
                if !header_lines.is_empty() {
                    self.app_event_tx
                        .send(AppEvent::InsertHistory(header_lines));
                }
            }
        }
    }

    fn stream_push_and_maybe_commit(&mut self, delta: &str) {
        // Newline-gated: only consider committing when a newline is present.
        let cfg = self.config.clone();
        let Some((_, state)) = self.active_kind_and_state_mut() else {
            return;
        };
        debug!(target: "chatwidget", "push_delta: len={} contains_nl={}", delta.len(), delta.contains('\n'));
        state.collector.push_delta(delta);
        if delta.contains('\n') {
            let newly_completed = state.collector.commit_complete_lines(&cfg);
            if !newly_completed.is_empty() {
                debug!(target: "chatwidget", "enqueue_completed: count={}", newly_completed.len());
                state.enqueue(newly_completed);
                // Start or continue commit animation.
                self.app_event_tx.send(AppEvent::StartCommitAnimation);
            }
        }
    }

    fn finalize_stream(&mut self, kind: StreamKind, flush_immediately: bool) {
        if self.current_stream != Some(kind) {
            // Nothing to do; either already finalized or not the active stream.
            return;
        }
        let cfg = self.config.clone();
        debug!(target: "chatwidget", "finalize_stream: {:?} (flush_immediately={})", kind, flush_immediately);
        // First, finalize the collector in its own borrow scope.
        let remaining = {
            let state = self.state_mut(kind);
            state.collector.finalize_and_drain(&cfg)
        };
        if flush_immediately {
            // Emit header if it hasn't been shown yet for this stream.
            let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
            self.emit_header_if_needed(kind, &mut lines);
            {
                let state = self.state_mut(kind);
                if !remaining.is_empty() {
                    debug!(target: "chatwidget", "finalize_immediate_emit: count={}", remaining.len());
                    state.enqueue(remaining);
                }
                // Drain everything queued and append a single trailing blank spacer.
                let step = state.drain_all();
                lines.extend(step.history);
            }
            if lines
                .last()
                .map(|l| !crate::line_utils::is_blank_line_trim(l))
                .unwrap_or(true)
            {
                lines.push(ratatui::text::Line::from(""));
            }
            // Send directly to history without waiting for commit ticks.
            #[allow(clippy::redundant_clone)]
            {
                let dbg_lines: Vec<String> = lines
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.clone())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .collect();
                debug!(target: "chatwidget", "finalize_immediate_insert: {:?}", dbg_lines);
            }
            self.app_event_tx.send(AppEvent::InsertHistory(lines));

            // Final cleanup mirrors the post-drain behavior from on_commit_tick.
            // Reset stream state so the next request emits headers again.
            self.state_mut(kind).clear();
            self.current_stream = None;
            self.finishing_after_drain = false;
            if self.task_complete_pending {
                self.bottom_pane.set_task_running(false);
                self.task_complete_pending = false;
            }
            self.flush_interrupt_queue();
        } else {
            if !remaining.is_empty() {
                debug!(target: "chatwidget", "finalize_enqueue_remaining: count={}", remaining.len());
                let state = self.state_mut(kind);
                state.enqueue(remaining);
            }
            // Trailing blank spacer will be animated out via commit ticks.
            {
                let state = self.state_mut(kind);
                state.enqueue(vec![ratatui::text::Line::from("")]);
            }
            // Mark that we should clear state after draining.
            self.finishing_after_drain = true;
            // Start animation to drain remaining lines. Final cleanup will occur when drained.
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
        }
    }
}

impl WidgetRef for &ChatWidget<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let [active_cell_area, bottom_pane_area] = self.layout_areas(area);
        (&self.bottom_pane).render(bottom_pane_area, buf);
        if let Some(cell) = &self.active_history_cell {
            cell.render_ref(active_cell_area, buf);
        }
    }
}

fn add_token_usage(current_usage: &TokenUsage, new_usage: &TokenUsage) -> TokenUsage {
    let cached_input_tokens = match (
        current_usage.cached_input_tokens,
        new_usage.cached_input_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    let reasoning_output_tokens = match (
        current_usage.reasoning_output_tokens,
        new_usage.reasoning_output_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    TokenUsage {
        input_tokens: current_usage.input_tokens + new_usage.input_tokens,
        cached_input_tokens,
        output_tokens: current_usage.output_tokens + new_usage.output_tokens,
        reasoning_output_tokens,
        total_tokens: current_usage.total_tokens + new_usage.total_tokens,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unnameable_test_items)]
mod chatwidget_helper_tests {
    use super::*;
    use crate::app_event::AppEvent;
    use crate::app_event_sender::AppEventSender;
    use codex_core::config::ConfigOverrides;
    use codex_core::config::ConfigToml;
    use codex_core::plan_tool::PlanItemArg;
    use codex_core::plan_tool::StepStatus;
    use codex_core::plan_tool::UpdatePlanArgs;
    use codex_core::protocol::AgentMessageDeltaEvent;
    use codex_core::protocol::AgentReasoningDeltaEvent;
    use codex_core::protocol::ApplyPatchApprovalRequestEvent;
    use codex_core::protocol::FileChange;
    use codex_core::protocol::PatchApplyBeginEvent;
    use codex_core::protocol::PatchApplyEndEvent;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use std::sync::mpsc::channel;

    fn test_config() -> Config {
        // Use base defaults to avoid depending on host state.
        codex_core::config::Config::load_from_base_config_with_overrides(
            ConfigToml::default(),
            ConfigOverrides::default(),
            std::env::temp_dir(),
        )
        .expect("config")
    }

    #[test]
    fn final_answer_without_newline_is_flushed_immediately() {
        let (mut chat, rx, _op_rx) = make_chatwidget_manual();

        // Simulate a streaming answer without any newline characters.
        chat.handle_codex_event(Event {
            id: "sub-a".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "Hi! How can I help with codex-rs or anything else today?".into(),
            }),
        });

        // Now simulate the final AgentMessage which should flush the pending line immediately.
        chat.handle_codex_event(Event {
            id: "sub-a".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: "Hi! How can I help with codex-rs or anything else today?".into(),
            }),
        });

        // Drain history insertions and verify the final line is present.
        let cells = drain_insert_history(&rx);
        assert!(
            cells.iter().any(|lines| {
                let s = lines
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .map(|sp| sp.content.clone())
                    .collect::<String>();
                s.contains("codex")
            }),
            "expected 'codex' header to be emitted",
        );
        let found_final = cells.iter().any(|lines| {
            let s = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|sp| sp.content.clone())
                .collect::<String>();
            s.contains("Hi! How can I help with codex-rs or anything else today?")
        });
        assert!(
            found_final,
            "expected final answer text to be flushed to history"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn helpers_are_available_and_do_not_panic() {
        let (tx_raw, _rx) = channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let cfg = test_config();
        let mut w = ChatWidget::new(cfg, tx, None, Vec::new(), false);
        // Basic construction sanity.
        let _ = &mut w;
    }

    // --- Helpers for tests that need direct construction and event draining ---
    fn make_chatwidget_manual() -> (
        ChatWidget<'static>,
        std::sync::mpsc::Receiver<AppEvent>,
        tokio::sync::mpsc::UnboundedReceiver<Op>,
    ) {
        let (tx_raw, rx) = channel::<AppEvent>();
        let app_event_tx = AppEventSender::new(tx_raw);
        let (op_tx, op_rx) = unbounded_channel::<Op>();
        let cfg = test_config();
        let bottom = BottomPane::new(BottomPaneParams {
            app_event_tx: app_event_tx.clone(),
            has_input_focus: true,
            enhanced_keys_supported: false,
        });
        let widget = ChatWidget {
            app_event_tx,
            codex_op_tx: op_tx,
            bottom_pane: bottom,
            active_history_cell: None,
            config: cfg,
            initial_user_message: None,
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage::default(),
            reasoning: StreamState::new(),
            answer: StreamState::new(),
            running_commands: HashMap::new(),
            current_stream: None,
            task_complete_pending: false,
            finishing_after_drain: false,
            interrupt_queue: VecDeque::new(),
            header: HeaderEmitter::new(),
        };
        (widget, rx, op_rx)
    }

    fn drain_insert_history(
        rx: &std::sync::mpsc::Receiver<AppEvent>,
    ) -> Vec<Vec<ratatui::text::Line<'static>>> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::InsertHistory(lines) = ev {
                out.push(lines);
            }
        }
        out
    }

    fn lines_to_single_string(lines: &[ratatui::text::Line<'static>]) -> String {
        let mut s = String::new();
        for line in lines {
            for span in &line.spans {
                s.push_str(&span.content);
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn final_longer_answer_after_single_char_delta_is_complete() {
        let (mut chat, rx, _op_rx) = make_chatwidget_manual();

        // Simulate a stray delta without newline (e.g., punctuation).
        chat.handle_codex_event(Event {
            id: "sub-x".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta: "?".into() }),
        });

        // Now send the full final answer with no newline.
        let full = "Hi! How can I help with codex-rs today? Want me to explore the repo, run tests, or work on a specific change?";
        chat.handle_codex_event(Event {
            id: "sub-x".into(),
            msg: EventMsg::AgentMessage(AgentMessageEvent {
                message: full.into(),
            }),
        });

        // Drain and assert the full message appears in history.
        let cells = drain_insert_history(&rx);
        let mut found = false;
        for lines in &cells {
            let s = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|sp| sp.content.clone())
                .collect::<String>();
            if s.contains(full) {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "expected full final message to be flushed to history, cells={:?}",
            cells.len()
        );
    }

    #[test]
    fn apply_patch_events_emit_history_cells() {
        let (mut chat, rx, _op_rx) = make_chatwidget_manual();

        // 1) Approval request -> proposed patch summary cell
        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("foo.txt"),
            FileChange::Add {
                content: "hello\n".to_string(),
            },
        );
        let ev = ApplyPatchApprovalRequestEvent {
            call_id: "c1".into(),
            changes,
            reason: None,
            grant_root: None,
        };
        chat.handle_codex_event(Event {
            id: "s1".into(),
            msg: EventMsg::ApplyPatchApprovalRequest(ev),
        });
        let cells = drain_insert_history(&rx);
        assert!(!cells.is_empty(), "expected pending patch cell to be sent");
        let blob = lines_to_single_string(cells.last().unwrap());
        assert!(
            blob.contains("proposed patch"),
            "missing proposed patch header: {blob:?}"
        );

        // 2) Begin apply -> applying patch cell
        let mut changes2 = HashMap::new();
        changes2.insert(
            PathBuf::from("foo.txt"),
            FileChange::Add {
                content: "hello\n".to_string(),
            },
        );
        let begin = PatchApplyBeginEvent {
            call_id: "c1".into(),
            auto_approved: true,
            changes: changes2,
        };
        chat.handle_codex_event(Event {
            id: "s1".into(),
            msg: EventMsg::PatchApplyBegin(begin),
        });
        let cells = drain_insert_history(&rx);
        assert!(!cells.is_empty(), "expected applying patch cell to be sent");
        let blob = lines_to_single_string(cells.last().unwrap());
        assert!(
            blob.contains("Applying patch"),
            "missing applying patch header: {blob:?}"
        );

        // 3) End apply success -> success cell
        let end = PatchApplyEndEvent {
            call_id: "c1".into(),
            stdout: "ok\n".into(),
            stderr: String::new(),
            success: true,
        };
        chat.handle_codex_event(Event {
            id: "s1".into(),
            msg: EventMsg::PatchApplyEnd(end),
        });
        let cells = drain_insert_history(&rx);
        assert!(!cells.is_empty(), "expected applied patch cell to be sent");
        let blob = lines_to_single_string(cells.last().unwrap());
        assert!(
            blob.contains("Applied patch"),
            "missing applied patch header: {blob:?}"
        );
    }

    #[test]
    fn apply_patch_approval_sends_op_with_submission_id() {
        let (mut chat, rx, _op_rx) = make_chatwidget_manual();
        // Simulate receiving an approval request with a distinct submission id and call id
        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("file.rs"),
            FileChange::Add {
                content: "fn main(){}\n".into(),
            },
        );
        let ev = ApplyPatchApprovalRequestEvent {
            call_id: "call-999".into(),
            changes,
            reason: None,
            grant_root: None,
        };
        chat.handle_codex_event(Event {
            id: "sub-123".into(),
            msg: EventMsg::ApplyPatchApprovalRequest(ev),
        });

        // Approve via key press 'y'
        chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        // Expect a CodexOp with PatchApproval carrying the submission id, not call id
        let mut found = false;
        while let Ok(app_ev) = rx.try_recv() {
            if let AppEvent::CodexOp(Op::PatchApproval { id, decision }) = app_ev {
                assert_eq!(id, "sub-123");
                assert!(matches!(
                    decision,
                    codex_core::protocol::ReviewDecision::Approved
                ));
                found = true;
                break;
            }
        }
        assert!(found, "expected PatchApproval op to be sent");
    }

    #[test]
    fn apply_patch_full_flow_integration_like() {
        let (mut chat, rx, mut op_rx) = make_chatwidget_manual();

        // 1) Backend requests approval
        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("pkg.rs"),
            FileChange::Add { content: "".into() },
        );
        chat.handle_codex_event(Event {
            id: "sub-xyz".into(),
            msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                call_id: "call-1".into(),
                changes,
                reason: None,
                grant_root: None,
            }),
        });

        // 2) User approves via 'y' and App receives a CodexOp
        chat.handle_key_event(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        let mut maybe_op: Option<Op> = None;
        while let Ok(app_ev) = rx.try_recv() {
            if let AppEvent::CodexOp(op) = app_ev {
                maybe_op = Some(op);
                break;
            }
        }
        let op = maybe_op.expect("expected CodexOp after key press");

        // 3) App forwards to widget.submit_op, which pushes onto codex_op_tx
        chat.submit_op(op);
        let forwarded = op_rx
            .try_recv()
            .expect("expected op forwarded to codex channel");
        match forwarded {
            Op::PatchApproval { id, decision } => {
                assert_eq!(id, "sub-xyz");
                assert!(matches!(
                    decision,
                    codex_core::protocol::ReviewDecision::Approved
                ));
            }
            other => panic!("unexpected op forwarded: {other:?}"),
        }

        // 4) Simulate patch begin/end events from backend; ensure history cells are emitted
        let mut changes2 = HashMap::new();
        changes2.insert(
            PathBuf::from("pkg.rs"),
            FileChange::Add { content: "".into() },
        );
        chat.handle_codex_event(Event {
            id: "sub-xyz".into(),
            msg: EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: "call-1".into(),
                auto_approved: false,
                changes: changes2,
            }),
        });
        chat.handle_codex_event(Event {
            id: "sub-xyz".into(),
            msg: EventMsg::PatchApplyEnd(PatchApplyEndEvent {
                call_id: "call-1".into(),
                stdout: String::from("ok"),
                stderr: String::new(),
                success: true,
            }),
        });
    }

    #[test]
    fn apply_patch_untrusted_shows_approval_modal() {
        let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
        // Ensure approval policy is untrusted (OnRequest)
        chat.config.approval_policy = codex_core::protocol::AskForApproval::OnRequest;

        // Simulate a patch approval request from backend
        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("a.rs"),
            FileChange::Add { content: "".into() },
        );
        chat.handle_codex_event(Event {
            id: "sub-1".into(),
            msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                call_id: "call-1".into(),
                changes,
                reason: None,
                grant_root: None,
            }),
        });

        // Render and ensure the approval modal title is present
        let area = ratatui::layout::Rect::new(0, 0, 80, 12);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        (&chat).render_ref(area, &mut buf);

        let mut contains_title = false;
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            if row.contains("Apply changes?") {
                contains_title = true;
                break;
            }
        }
        assert!(
            contains_title,
            "expected approval modal to be visible with title 'Apply changes?'"
        );
    }

    #[test]
    fn apply_patch_request_shows_diff_summary() {
        let (mut chat, rx, _op_rx) = make_chatwidget_manual();

        // Ensure we are in OnRequest so an approval is surfaced
        chat.config.approval_policy = codex_core::protocol::AskForApproval::OnRequest;

        // Simulate backend asking to apply a patch adding two lines to README.md
        let mut changes = HashMap::new();
        changes.insert(
            PathBuf::from("README.md"),
            FileChange::Add {
                // Two lines (no trailing empty line counted)
                content: "line one\nline two\n".into(),
            },
        );
        chat.handle_codex_event(Event {
            id: "sub-apply".into(),
            msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                call_id: "call-apply".into(),
                changes,
                reason: None,
                grant_root: None,
            }),
        });

        // Drain history insertions and verify the diff summary is present
        let cells = drain_insert_history(&rx);
        assert!(
            !cells.is_empty(),
            "expected a history cell with the proposed patch summary"
        );
        let blob = lines_to_single_string(cells.last().unwrap());

        // Header should summarize totals
        assert!(
            blob.contains("proposed patch to 1 file (+2 -0)"),
            "missing or incorrect diff header: {blob:?}"
        );

        // Per-file summary line should include the file path and counts
        assert!(
            blob.contains("README.md (+2 -0)"),
            "missing per-file diff summary: {blob:?}"
        );
    }

    #[test]
    fn plan_update_renders_history_cell() {
        let (mut chat, rx, _op_rx) = make_chatwidget_manual();
        let update = UpdatePlanArgs {
            explanation: Some("Adapting plan".to_string()),
            plan: vec![
                PlanItemArg {
                    step: "Explore codebase".into(),
                    status: StepStatus::Completed,
                },
                PlanItemArg {
                    step: "Implement feature".into(),
                    status: StepStatus::InProgress,
                },
                PlanItemArg {
                    step: "Write tests".into(),
                    status: StepStatus::Pending,
                },
            ],
        };
        chat.handle_codex_event(Event {
            id: "sub-1".into(),
            msg: EventMsg::PlanUpdate(update),
        });
        let cells = drain_insert_history(&rx);
        assert!(!cells.is_empty(), "expected plan update cell to be sent");
        let blob = lines_to_single_string(cells.last().unwrap());
        assert!(blob.contains("Updated"), "missing plan header: {blob:?}");
        assert!(blob.contains("Explore codebase"));
        assert!(blob.contains("Implement feature"));
        assert!(blob.contains("Write tests"));
    }

    #[test]
    fn headers_emitted_on_stream_begin_for_answer_and_reasoning() {
        let (mut chat, rx, _op_rx) = make_chatwidget_manual();

        // Answer: no header until a newline commit
        chat.handle_codex_event(Event {
            id: "sub-a".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "Hello".into(),
            }),
        });
        let mut saw_codex_pre = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::InsertHistory(lines) = ev {
                let s = lines
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                if s.contains("codex") {
                    saw_codex_pre = true;
                    break;
                }
            }
        }
        assert!(
            !saw_codex_pre,
            "answer header should not be emitted before first newline commit"
        );

        // Newline arrives, then header is emitted
        chat.handle_codex_event(Event {
            id: "sub-a".into(),
            msg: EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                delta: "!\n".into(),
            }),
        });
        chat.on_commit_tick();
        let mut saw_codex_post = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::InsertHistory(lines) = ev {
                let s = lines
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                if s.contains("codex") {
                    saw_codex_post = true;
                    break;
                }
            }
        }
        assert!(
            saw_codex_post,
            "expected 'codex' header to be emitted after first newline commit"
        );

        // Reasoning: header immediately
        let (mut chat2, rx2, _op_rx2) = make_chatwidget_manual();
        chat2.handle_codex_event(Event {
            id: "sub-b".into(),
            msg: EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                delta: "Thinking".into(),
            }),
        });
        let mut saw_thinking = false;
        while let Ok(ev) = rx2.try_recv() {
            if let AppEvent::InsertHistory(lines) = ev {
                let s = lines
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                if s.contains("thinking") {
                    saw_thinking = true;
                    break;
                }
            }
        }
        assert!(
            saw_thinking,
            "expected 'thinking' header to be emitted at stream start"
        );
    }
}
