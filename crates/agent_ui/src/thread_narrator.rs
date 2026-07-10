//! Narration bridge from agent-panel (ACP) threads to the DontSpeak daemon.
//!
//! A [`ThreadNarrator`] subscribes to one [`AcpThread`]'s event stream and
//! forwards each assistant message's *cumulative* text to the daemon's
//! blockquote-narration pipeline (`narrate_batch`) over the DontSpeak socket.
//! The daemon owns all narration logic — blockquote extraction, dedup
//! (`DisplayState`), mic gating, the TTS queue — so re-sending the same
//! cumulative text is harmless by design; this side only debounces and keys
//! the batches.
//!
//! Batch keys are `"{session}#{generation}#{entry_ix}"`. Truncation
//! (`EntriesRemoved`) reuses entry indices, so it bumps `generation` to keep
//! keys unique. A turn is finalized (`is_final: true`, which runs the
//! daemon's end-of-turn shorts fallback) on `Stopped`, `Error`, *and*
//! `Refusal` — the error path never emits `Stopped`.
//!
//! Gating: nothing is sent unless the `dontspeak.narrate_panel_agents`
//! setting allows this thread's agent (`auto` skips agents with DontSpeak
//! hook wiring of their own — their replies already narrate through the
//! daemon's hooks, and narrating both sides would speak everything twice)
//! and the daemon connection is live.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use acp_thread::{
    AcpThread, AcpThreadEvent, AgentThreadEntry, AssistantMessageChunk, ThreadStatus,
};
use agent_client_protocol::schema::v1 as acp;
use agent_servers::{CLAUDE_AGENT_ID, CODEX_ID};
use collections::HashMap;
use dontspeak::{DontSpeak, DontSpeakSettings, NarratePanelAgents, Request};
use gpui::{App, AppContext as _, Context, Entity, SharedString, Subscription, Task};
use settings::Settings as _;

/// How long an assistant entry's updates are batched before its cumulative
/// text is (re)sent to the daemon. Throttle, not a trailing debounce: the
/// first update in a window schedules the send, later updates ride along —
/// so a continuously streaming reply still narrates mid-turn.
const SEND_INTERVAL: Duration = Duration::from_millis(300);

/// Embedded copy of DontSpeak's `DEFAULT_NARRATION_SPEC`
/// (`rust/crates/ds-config/src/narration.rs`), used when the user has no
/// `narration-spec.md` override in DontSpeak's config directory. Keep in
/// sync manually — it changes rarely and drift only shades the spoken style.
const EMBEDDED_NARRATION_SPEC: &str = r#"# Narrate
Only `>` lines are spoken (rest silent). Lead every reply with a `>` digest: one line per point, plain speech, no markdown/code/URLs/paths, say IDs as words. Any pick-one options → speak each as the LAST `>` lines, for voice reply.
"#;

/// Where narration requests go. Production forwards to the global
/// [`DontSpeak`] daemon connection; tests record.
pub trait NarrationSink {
    /// Whether the daemon can currently receive narration requests.
    fn is_available(&self, cx: &App) -> bool;
    /// Fire-and-forget a request to the daemon.
    fn send(&self, request: Request, cx: &mut App);
}

/// Production sink: one-shot requests through the global [`DontSpeak`]
/// entity, available only while Zed is subscribed to the daemon.
struct DaemonNarrationSink;

impl NarrationSink for DaemonNarrationSink {
    fn is_available(&self, cx: &App) -> bool {
        DontSpeak::global(cx).is_some_and(|dontspeak| dontspeak.read(cx).status().is_connected())
    }

    fn send(&self, request: Request, cx: &mut App) {
        let Some(dontspeak) = DontSpeak::global(cx) else {
            return;
        };
        let response = dontspeak.read(cx).request(request, cx);
        cx.background_spawn(async move {
            match response.await {
                Ok(dontspeak::Response::Error { message }) => {
                    log::warn!("dontspeak: narration request rejected: {message}");
                }
                Ok(_) => {}
                Err(error) => {
                    log::debug!("dontspeak: narration request failed: {error:#}");
                }
            }
        })
        .detach();
    }
}

/// Narrates one [`AcpThread`]'s assistant replies through the DontSpeak
/// daemon. Owns nothing but its subscription; dropping it ends the session
/// daemon-side (`SessionEnd`).
pub struct ThreadNarrator {
    /// The daemon-facing session key — the ACP session id verbatim, so the
    /// daemon's per-session voice pool gives each thread its own voice.
    session: String,
    agent_id: SharedString,
    /// Bumped on `EntriesRemoved`: truncation reuses entry indices, and a
    /// fresh generation keeps batch keys unique across it.
    generation: u64,
    /// Assistant entries updated in the current turn (current generation).
    /// Cleared on finalize and on truncation — after a refusal-truncation,
    /// finalizing must not re-narrate surviving entries from older turns.
    touched: BTreeSet<usize>,
    /// Per-entry scheduled sends. Dropping a task cancels it.
    pending: HashMap<usize, Task<()>>,
    was_generating: bool,
    sink: Rc<dyn NarrationSink>,
    _thread_subscription: Subscription,
}

impl ThreadNarrator {
    /// Attaches a narrator to `thread`, narrating through the global
    /// DontSpeak daemon connection.
    pub fn attach(
        thread: &Entity<AcpThread>,
        agent_id: SharedString,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| Self::new(thread, agent_id, Rc::new(DaemonNarrationSink), cx))
    }

    pub fn new(
        thread: &Entity<AcpThread>,
        agent_id: SharedString,
        sink: Rc<dyn NarrationSink>,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = thread.read(cx).session_id().to_string();
        let this = Self {
            session,
            agent_id,
            generation: 0,
            touched: BTreeSet::new(),
            pending: HashMap::default(),
            was_generating: thread.read(cx).status() == ThreadStatus::Generating,
            sink,
            _thread_subscription: cx.subscribe(thread, Self::handle_thread_event),
        };

        // Panel-narrated agents don't run DontSpeak's UserPromptSubmit hook,
        // so they get the narration spec here instead: queued as a
        // request-only content block on the session's first prompt — sent to
        // the agent (whose server-side session history retains it for the
        // whole session) but never rendered in the user's message bubble.
        if this.narration_active(cx) {
            thread.update(cx, |thread, _cx| {
                thread.append_request_context_for_next_prompt([acp::ContentBlock::Text(
                    acp::TextContent::new(narration_spec()),
                )]);
            });
        }

        cx.on_release(|this: &mut Self, cx: &mut App| {
            // The thread is closing for good: reclaim its daemon-side queue
            // and session-scoped voice state.
            if this.narration_active(cx) {
                this.sink.send(
                    Request::SessionEnd {
                        session: Some(this.session.clone()),
                    },
                    cx,
                );
            }
        })
        .detach();

        this
    }

    fn handle_thread_event(
        &mut self,
        thread: Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AcpThreadEvent::NewEntry => {
                let Some(ix) = thread.read(cx).entries().len().checked_sub(1) else {
                    return;
                };
                self.schedule_send(thread, ix, cx);
            }
            AcpThreadEvent::EntryUpdated(ix) => {
                self.schedule_send(thread, *ix, cx);
            }
            AcpThreadEvent::EntriesRemoved(_) => {
                // Entry indices are reused after truncation; retire every
                // outstanding key instead of guessing which ones shifted.
                self.generation += 1;
                self.pending.clear();
                self.touched.clear();
            }
            AcpThreadEvent::Stopped(_) | AcpThreadEvent::Error | AcpThreadEvent::Refusal => {
                self.finalize_turn(&thread, cx);
            }
            AcpThreadEvent::StatusChanged => {
                let generating = thread.read(cx).status() == ThreadStatus::Generating;
                if generating && !self.was_generating && self.narration_active(cx) {
                    // Mirror of the hooks' UserPromptSubmit → MarkActive: a
                    // prompt was just submitted to this session.
                    self.sink.send(
                        Request::MarkActive {
                            session: Some(self.session.clone()),
                            synthetic: false,
                        },
                        cx,
                    );
                }
                self.was_generating = generating;
            }
            _ => {}
        }
    }

    /// Schedule a (re)send of entry `ix`'s cumulative text, unless one is
    /// already scheduled.
    fn schedule_send(&mut self, thread: Entity<AcpThread>, ix: usize, cx: &mut Context<Self>) {
        if !self.narration_active(cx)
            || !matches!(
                thread.read(cx).entries().get(ix),
                Some(AgentThreadEntry::AssistantMessage(_))
            )
        {
            return;
        }
        self.touched.insert(ix);
        if self.pending.contains_key(&ix) {
            return;
        }
        let generation = self.generation;
        self.pending.insert(
            ix,
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SEND_INTERVAL).await;
                this.update(cx, |this, cx| {
                    this.pending.remove(&ix);
                    // Belt-and-braces: a truncation between scheduling and
                    // firing drops this task, but never trust a stale index.
                    if this.generation == generation {
                        this.send_batch(&thread, ix, false, cx);
                    }
                })
                .ok();
            }),
        );
    }

    /// End of turn: flush still-scheduled entries, then send the last
    /// assistant entry as `is_final` so the daemon runs its shorts fallback.
    fn finalize_turn(&mut self, thread: &Entity<AcpThread>, cx: &mut Context<Self>) {
        self.pending.clear();
        let touched = std::mem::take(&mut self.touched);
        let Some(&last) = touched.last() else {
            return;
        };
        for &ix in touched.iter().filter(|&&ix| ix != last) {
            self.send_batch(thread, ix, false, cx);
        }
        self.send_batch(thread, last, true, cx);
    }

    fn send_batch(
        &mut self,
        thread: &Entity<AcpThread>,
        ix: usize,
        is_final: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.narration_active(cx) {
            return;
        }
        let Some(text) = assistant_message_text(thread.read(cx), ix, cx) else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        self.sink.send(
            Request::NarrateBatch {
                session: self.session.clone(),
                key: format!("{}#{}#{}", self.session, self.generation, ix),
                text,
                is_final,
            },
            cx,
        );
    }

    fn narration_active(&self, cx: &App) -> bool {
        self.settings_allow(cx) && self.sink.is_available(cx)
    }

    fn settings_allow(&self, cx: &App) -> bool {
        let settings = DontSpeakSettings::get_global(cx);
        settings.enabled
            && match settings.narrate_panel_agents {
                NarratePanelAgents::None => false,
                NarratePanelAgents::All => true,
                NarratePanelAgents::Auto => !agent_has_hook_wiring(&self.agent_id),
            }
    }
}

/// Agents that carry DontSpeak's own hook wiring (`MessageDisplay`/`Stop`
/// hooks in their settings) narrate through the daemon already; `auto`
/// leaves them alone so replies aren't spoken twice.
fn agent_has_hook_wiring(agent_id: &str) -> bool {
    agent_id == CLAUDE_AGENT_ID
        || agent_id == CODEX_ID
        || agent_id.to_ascii_lowercase().contains("qwen")
}

/// The cumulative narratable text of assistant entry `ix`: its `Message`
/// chunks' markdown, in order, skipping `Thought` chunks. `None` when `ix`
/// is not an assistant message.
fn assistant_message_text(thread: &AcpThread, ix: usize, cx: &App) -> Option<String> {
    let AgentThreadEntry::AssistantMessage(message) = thread.entries().get(ix)? else {
        return None;
    };
    let mut text = String::new();
    for chunk in &message.chunks {
        if let AssistantMessageChunk::Message { block, .. } = chunk {
            let chunk_text = block.to_markdown(cx);
            if chunk_text.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(chunk_text);
        }
    }
    Some(text)
}

/// The narration spec injected into panel-narrated agents: the user's
/// `narration-spec.md` override from DontSpeak's config directory when
/// present and non-empty, otherwise the embedded default — the same
/// precedence DontSpeak's own UserPromptSubmit hook applies.
fn narration_spec() -> String {
    std::fs::read_to_string(dontspeak::client::config_dir().join("narration-spec.md"))
        .ok()
        .filter(|spec| !spec.trim().is_empty())
        .unwrap_or_else(|| EMBEDDED_NARRATION_SPEC.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{AgentConnection as _, StubAgentConnection};
    use gpui::{TestAppContext, UpdateGlobal as _};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::cell::RefCell;
    use std::path::Path;
    use util::{path, path_list::PathList};

    struct RecordingSink {
        requests: Rc<RefCell<Vec<Request>>>,
        available: bool,
    }

    impl NarrationSink for RecordingSink {
        fn is_available(&self, _cx: &App) -> bool {
            self.available
        }

        fn send(&self, request: Request, _cx: &mut App) {
            self.requests.borrow_mut().push(request);
        }
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings_store = SettingsStore::test(cx);
            settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
            cx.set_global(settings_store);
        });
    }

    fn set_narrate_setting(cx: &mut TestAppContext, narrate: settings::NarratePanelAgents) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |content| {
                    let dontspeak = content.dontspeak.get_or_insert_default();
                    dontspeak.enabled = Some(true);
                    dontspeak.narrate_panel_agents = Some(narrate);
                });
            });
        });
    }

    struct NarratedThread {
        connection: Rc<StubAgentConnection>,
        thread: Entity<AcpThread>,
        session_id: acp::SessionId,
        narrator: Entity<ThreadNarrator>,
        requests: Rc<RefCell<Vec<Request>>>,
    }

    async fn build(cx: &mut TestAppContext, agent_id: &str) -> NarratedThread {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|cx| {
                connection.clone().new_session(
                    project,
                    PathList::new(&[Path::new(path!("/test"))]),
                    cx,
                )
            })
            .await
            .unwrap();
        let session_id = thread.read_with(cx, |thread, _| thread.session_id().clone());
        let requests = Rc::new(RefCell::new(Vec::new()));
        let narrator = cx.new(|cx| {
            ThreadNarrator::new(
                &thread,
                agent_id.to_string().into(),
                Rc::new(RecordingSink {
                    requests: requests.clone(),
                    available: true,
                }),
                cx,
            )
        });
        NarratedThread {
            connection,
            thread,
            session_id,
            narrator,
            requests,
        }
    }

    fn narrate_batches(requests: &[Request]) -> Vec<(String, String, bool)> {
        requests
            .iter()
            .filter_map(|request| match request {
                Request::NarrateBatch {
                    key,
                    text,
                    is_final,
                    ..
                } => Some((key.clone(), text.clone(), *is_final)),
                _ => None,
            })
            .collect()
    }

    fn agent_says(fixture: &NarratedThread, text: &str, cx: &mut TestAppContext) {
        let session_id = fixture.session_id.clone();
        let connection = fixture.connection.clone();
        let text = text.to_string();
        cx.update(|cx| {
            connection.send_update(
                session_id,
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(text.as_str().into())),
                cx,
            );
        });
    }

    #[gpui::test]
    async fn test_streamed_blockquote_narrates_nonfinal_then_final(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::All);
        let fixture = build(cx, "stub").await;
        let session = fixture.session_id.to_string();

        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("hi", cx));
        cx.run_until_parked();

        // Submitting the prompt marks this session ACTIVE, like the hooks do.
        assert_eq!(
            fixture.requests.borrow().first(),
            Some(&Request::MarkActive {
                session: Some(session.clone()),
                synthetic: false,
            })
        );

        agent_says(&fixture, "> Hello world.", cx);
        cx.executor().advance_clock(SEND_INTERVAL * 2);
        cx.run_until_parked();

        let expected_key = format!("{session}#0#1");
        assert_eq!(
            narrate_batches(&fixture.requests.borrow()),
            [(expected_key.clone(), "> Hello world.".into(), false)]
        );

        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::EndTurn);
        send.await.unwrap();
        cx.run_until_parked();

        assert_eq!(
            narrate_batches(&fixture.requests.borrow()),
            [
                (expected_key.clone(), "> Hello world.".into(), false),
                (expected_key, "> Hello world.".into(), true),
            ]
        );
    }

    #[gpui::test]
    async fn test_truncation_bumps_the_key_generation(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::All);
        let fixture = build(cx, "stub").await;
        let session = fixture.session_id.to_string();

        // Turn 1 streams, then the agent REFUSES: the thread truncates back
        // past the user message (EntriesRemoved) and must not re-narrate.
        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("one", cx));
        cx.run_until_parked();
        agent_says(&fixture, "> One.", cx);
        cx.executor().advance_clock(SEND_INTERVAL * 2);
        cx.run_until_parked();
        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::Refusal);
        send.await.unwrap();
        cx.run_until_parked();

        // Turn 2 reuses entry index 1 — the bumped generation keeps its key
        // distinct from turn 1's.
        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("two", cx));
        cx.run_until_parked();
        agent_says(&fixture, "> Two.", cx);
        cx.executor().advance_clock(SEND_INTERVAL * 2);
        cx.run_until_parked();
        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::EndTurn);
        send.await.unwrap();
        cx.run_until_parked();

        assert_eq!(
            narrate_batches(&fixture.requests.borrow()),
            [
                (format!("{session}#0#1"), "> One.".into(), false),
                (format!("{session}#1#1"), "> Two.".into(), false),
                (format!("{session}#1#1"), "> Two.".into(), true),
            ]
        );
    }

    #[gpui::test]
    async fn test_error_terminated_turn_still_finalizes(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::All);
        let fixture = build(cx, "stub").await;
        let session = fixture.session_id.to_string();

        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("hi", cx));
        cx.run_until_parked();
        agent_says(&fixture, "> Partial thought.", cx);
        // No clock advance: the scheduled non-final send is still pending
        // when the turn dies — finalize must still deliver the text.
        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::MaxTokens);
        send.await.unwrap_err();
        cx.run_until_parked();

        assert_eq!(
            narrate_batches(&fixture.requests.borrow()),
            [(format!("{session}#0#1"), "> Partial thought.".into(), true)]
        );
    }

    #[gpui::test]
    async fn test_thought_only_turn_is_silent(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::All);
        let fixture = build(cx, "stub").await;

        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("hi", cx));
        cx.run_until_parked();
        cx.update(|cx| {
            fixture.connection.send_update(
                fixture.session_id.clone(),
                acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                    "> not spoken".into(),
                )),
                cx,
            );
        });
        cx.executor().advance_clock(SEND_INTERVAL * 2);
        cx.run_until_parked();
        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::EndTurn);
        send.await.unwrap();
        cx.run_until_parked();

        assert_eq!(narrate_batches(&fixture.requests.borrow()), []);
    }

    #[gpui::test]
    async fn test_auto_skips_hook_wired_agents_entirely(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::Auto);
        let fixture = build(cx, CLAUDE_AGENT_ID).await;

        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("hi", cx));
        cx.run_until_parked();
        agent_says(&fixture, "> Hook-narrated already.", cx);
        cx.executor().advance_clock(SEND_INTERVAL * 2);
        cx.run_until_parked();
        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::EndTurn);
        send.await.unwrap();
        cx.run_until_parked();

        // Not even MarkActive/SessionEnd: the hooks own this session's
        // lifecycle, and a competing session key would misroute narration.
        drop(fixture.narrator);
        cx.run_until_parked();
        assert!(
            fixture.requests.borrow().is_empty(),
            "expected no requests, got {:?}",
            fixture.requests.borrow()
        );

        // The same auto setting narrates an agent with no hook wiring.
        let fixture = build(cx, "gemini").await;
        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("hi", cx));
        cx.run_until_parked();
        agent_says(&fixture, "> Spoken.", cx);
        cx.executor().advance_clock(SEND_INTERVAL * 2);
        cx.run_until_parked();
        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::EndTurn);
        send.await.unwrap();
        cx.run_until_parked();
        assert_eq!(narrate_batches(&fixture.requests.borrow()).len(), 2);
    }

    #[gpui::test]
    async fn test_none_setting_disables_narration(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::None);
        let fixture = build(cx, "stub").await;

        let send = fixture
            .thread
            .update(cx, |thread, cx| thread.send_raw("hi", cx));
        cx.run_until_parked();
        agent_says(&fixture, "> Silent.", cx);
        cx.executor().advance_clock(SEND_INTERVAL * 2);
        cx.run_until_parked();
        fixture
            .connection
            .end_turn(fixture.session_id.clone(), acp::StopReason::EndTurn);
        send.await.unwrap();
        cx.run_until_parked();

        assert!(
            fixture.requests.borrow().is_empty(),
            "expected no requests, got {:?}",
            fixture.requests.borrow()
        );
    }

    #[gpui::test]
    async fn test_dropping_the_narrator_ends_the_session(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::All);
        let fixture = build(cx, "stub").await;
        let session = fixture.session_id.to_string();

        drop(fixture.narrator);
        cx.run_until_parked();

        assert_eq!(
            fixture.requests.borrow().last(),
            Some(&Request::SessionEnd {
                session: Some(session),
            })
        );
    }

    #[gpui::test]
    async fn test_narration_spec_is_queued_as_request_only_context(cx: &mut TestAppContext) {
        init_test(cx);
        set_narrate_setting(cx, settings::NarratePanelAgents::All);
        let fixture = build(cx, "stub").await;

        // The narrator queued exactly one request-only spec block on the
        // thread; `acp_thread` owns delivering it with the first prompt.
        fixture.thread.read_with(cx, |thread, _| {
            assert_eq!(thread.request_context_for_next_prompt().len(), 1);
        });

        // A hook-wired agent under `auto` gets no spec (its hook injects one).
        set_narrate_setting(cx, settings::NarratePanelAgents::Auto);
        let fixture = build(cx, CODEX_ID).await;
        fixture.thread.read_with(cx, |thread, _| {
            assert_eq!(thread.request_context_for_next_prompt().len(), 0);
        });
    }
}
