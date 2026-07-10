//! The dictation state machine on the Zed side of a DontSpeak frontend
//! subscription.
//!
//! [`DictationController`] consumes the frontend events emitted by the
//! [`DontSpeak`] global (see `dontspeak::Event::FrontendEvent`) and renders
//! them as IME-style marked text in whatever Zed input is focused, using the
//! GPUI focused-input text APIs. It tracks which window it marked so a mark
//! is never orphaned when focus moves between windows mid-dictation, and it
//! answers every `deliver` event with an `ack_deliver` — `ok: false` when
//! the text could not be inserted, which makes the daemon fall back to its
//! classic clipboard-paste path so no utterance is ever lost.
//!
//! All window operations go through [`DictationWindowOps`] so the state
//! machine is testable against a scripted fake.

use std::rc::Rc;
use std::time::Duration;

use dontspeak::{DontSpeak, Event as DontSpeakEvent, FrontendEvent, ReceivedEvent};
use gpui::{AnyWindowHandle, App, Context, Entity, Keystroke, Subscription, Task, WeakEntity};

/// How often a buffered `deliver` re-checks for an active window.
const DELIVER_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long a `deliver` that arrived with no active window is buffered
/// before it is nacked. Kept well under the daemon's own ~300 ms ack
/// deadline so the daemon hears the nack (and falls back to paste) instead
/// of timing out.
const DELIVER_DEADLINE: Duration = Duration::from_millis(150);

/// The window operations the controller needs, abstracted for testing. The
/// production implementation ([`ZedWindowOps`]) forwards to the GPUI
/// focused-input text APIs on [`gpui::Window`] and acks through the
/// [`DontSpeak`] global's subscription connection.
pub trait DictationWindowOps {
    /// The window that is currently active at the platform level, if any.
    fn active_window(&self, cx: &App) -> Option<AnyWindowHandle>;
    /// Set IME-style marked text in `window`'s focused input. Returns false
    /// when the window has no focused text input (or it rejects text).
    fn set_marked_text(&self, window: AnyWindowHandle, text: &str, cx: &mut App) -> bool;
    /// Remove (not commit) any marked text in `window`'s focused input.
    fn clear_marked_text(&self, window: AnyWindowHandle, cx: &mut App) -> bool;
    /// Insert `text` into `window`'s focused input as though typed,
    /// replacing any marked text. Returns false when it cannot insert.
    fn insert_text(&self, window: AnyWindowHandle, text: &str, cx: &mut App) -> bool;
    /// Dispatch a synthetic Enter keystroke to `window` (submits the agent
    /// panel message editor, sends Enter to a terminal, inserts a newline in
    /// a buffer — identical to a physical Enter).
    fn dispatch_enter(&self, window: AnyWindowHandle, cx: &mut App) -> bool;
    /// Acknowledge a `deliver` frontend event on the daemon subscription.
    fn ack(&self, seq: u64, ok: bool, cx: &mut App);
}

/// Production [`DictationWindowOps`]: GPUI window text APIs + the
/// [`DontSpeak`] global for acks.
pub(crate) struct ZedWindowOps {
    dontspeak: WeakEntity<DontSpeak>,
}

impl ZedWindowOps {
    pub(crate) fn new(dontspeak: WeakEntity<DontSpeak>) -> Self {
        Self { dontspeak }
    }
}

impl DictationWindowOps for ZedWindowOps {
    fn active_window(&self, cx: &App) -> Option<AnyWindowHandle> {
        cx.active_window()
    }

    fn set_marked_text(&self, window: AnyWindowHandle, text: &str, cx: &mut App) -> bool {
        window
            .update(cx, |_, window, cx| {
                window.set_marked_text_in_focused_input(text, cx)
            })
            .unwrap_or(false)
    }

    fn clear_marked_text(&self, window: AnyWindowHandle, cx: &mut App) -> bool {
        window
            .update(cx, |_, window, cx| {
                window.clear_marked_text_in_focused_input(false, cx)
            })
            .unwrap_or(false)
    }

    fn insert_text(&self, window: AnyWindowHandle, text: &str, cx: &mut App) -> bool {
        window
            .update(cx, |_, window, cx| {
                window.insert_text_into_focused_input(text, cx)
            })
            .unwrap_or(false)
    }

    fn dispatch_enter(&self, window: AnyWindowHandle, cx: &mut App) -> bool {
        window
            .update(cx, |_, window, cx| {
                window.dispatch_keystroke(Keystroke::parse("enter").expect("valid keystroke"), cx)
            })
            .unwrap_or(false)
    }

    fn ack(&self, seq: u64, ok: bool, cx: &mut App) {
        if let Some(dontspeak) = self.dontspeak.upgrade() {
            dontspeak.read(cx).ack_deliver(seq, ok);
        } else {
            log::warn!("dontspeak: dropping ack {seq}: the DontSpeak global is gone");
        }
    }
}

/// Where the in-flight dictation is in its lifecycle, for the status-bar
/// button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictationPhase {
    /// No dictation in flight.
    Idle,
    /// The mic is live; partials are streaming in.
    Recording,
    /// Recording ended; the daemon is waiting for the confirm gesture.
    AwaitingConfirm,
}

/// A `deliver` that arrived while no Zed window was active, waiting a short
/// grace period for one before it is nacked.
struct PendingDeliver {
    seq: u64,
    generation: usize,
    _task: Task<()>,
}

/// See the module docs.
pub struct DictationController {
    ops: Rc<dyn DictationWindowOps>,
    phase: DictationPhase,
    /// The window whose focused input currently holds our marked text. Any
    /// retarget (focus moved to another window between events) clears this
    /// window's mark first, so marked text is never orphaned.
    marked_window: Option<AnyWindowHandle>,
    pending_deliver: Option<PendingDeliver>,
    /// Bumped whenever a pending deliver is superseded; the stale poll task
    /// observes the mismatch and bows out.
    generation: usize,
    _subscription: Option<Subscription>,
}

impl DictationController {
    pub fn new(
        ops: Rc<dyn DictationWindowOps>,
        dontspeak: Option<&Entity<DontSpeak>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = dontspeak.map(|dontspeak| {
            cx.subscribe(dontspeak, |this, dontspeak, event, cx| match event {
                DontSpeakEvent::FrontendEvent(received) => {
                    this.handle_frontend_event(received.clone(), cx);
                }
                DontSpeakEvent::StatusChanged => {
                    if !dontspeak.read(cx).status().is_connected() {
                        this.reset(cx);
                    }
                }
            })
        });
        Self {
            ops,
            phase: DictationPhase::Idle,
            marked_window: None,
            pending_deliver: None,
            generation: 0,
            _subscription: subscription,
        }
    }

    pub fn phase(&self) -> DictationPhase {
        self.phase
    }

    /// Apply one dictation-lifecycle event from the daemon.
    pub fn handle_frontend_event(&mut self, received: ReceivedEvent, cx: &mut Context<Self>) {
        match received.event {
            FrontendEvent::RecordingStarted => {
                self.nack_pending_deliver(cx);
                self.set_phase(DictationPhase::Recording, cx);
                self.mark_text("", cx);
            }
            FrontendEvent::Partial { text } => {
                self.set_phase(DictationPhase::Recording, cx);
                self.mark_text(&text, cx);
            }
            FrontendEvent::AwaitingConfirm { text } => {
                self.set_phase(DictationPhase::AwaitingConfirm, cx);
                self.mark_text(&text, cx);
            }
            FrontendEvent::Deliver { text, submit } => {
                self.nack_pending_deliver(cx);
                self.set_phase(DictationPhase::Idle, cx);
                self.clear_mark(cx);
                self.deliver(received.seq, text, submit, cx);
            }
            FrontendEvent::Cancelled | FrontendEvent::Refused => {
                self.nack_pending_deliver(cx);
                self.set_phase(DictationPhase::Idle, cx);
                self.clear_mark(cx);
            }
            // A lifecycle event kind this build doesn't understand: leave the
            // in-flight dictation state untouched rather than guessing.
            FrontendEvent::Unknown => {}
        }
    }

    /// The daemon connection is gone: drop any pending deliver (an ack has
    /// nowhere to go), clear our marked text, and go idle.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.pending_deliver = None;
        self.clear_mark(cx);
        self.set_phase(DictationPhase::Idle, cx);
    }

    fn set_phase(&mut self, phase: DictationPhase, cx: &mut Context<Self>) {
        if self.phase != phase {
            self.phase = phase;
            cx.notify();
        }
    }

    /// Set marked text in the active window's focused input, clearing the
    /// previous window's mark first when focus moved between windows.
    fn mark_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let active = self.ops.active_window(cx);
        if let Some(old) = self.marked_window
            && active != Some(old)
        {
            self.ops.clear_marked_text(old, cx);
            self.marked_window = None;
        }
        let Some(window) = active else {
            return;
        };
        if self.ops.set_marked_text(window, text, cx) {
            self.marked_window = Some(window);
        } else if self.marked_window.take() == Some(window) {
            // The focused input stopped accepting text; drop the stale mark.
            self.ops.clear_marked_text(window, cx);
        }
    }

    fn clear_mark(&mut self, cx: &mut Context<Self>) {
        if let Some(window) = self.marked_window.take() {
            self.ops.clear_marked_text(window, cx);
        }
    }

    fn deliver(&mut self, seq: u64, text: String, submit: bool, cx: &mut Context<Self>) {
        if let Some(window) = self.ops.active_window(cx) {
            self.perform_deliver(window, seq, &text, submit, cx);
        } else {
            self.buffer_deliver(seq, text, submit, cx);
        }
    }

    /// Insert the final transcript, submit if asked, and ack — in exactly
    /// that order. `ok: false` when the insert failed (no focused text
    /// input); the daemon then falls back to its paste path.
    fn perform_deliver(
        &mut self,
        window: AnyWindowHandle,
        seq: u64,
        text: &str,
        submit: bool,
        cx: &mut Context<Self>,
    ) {
        let inserted = self.ops.insert_text(window, text, cx);
        if inserted && submit {
            // The ack reflects insertion only: once the text has landed the
            // utterance is delivered, so a (near-impossible) Enter-dispatch
            // failure is best-effort and does not trigger the paste fallback.
            self.ops.dispatch_enter(window, cx);
        }
        self.ops.ack(seq, inserted, cx);
    }

    /// No window is active right now (e.g. the OS is mid focus-transfer):
    /// hold the deliver for a short grace period, polling for a window, and
    /// nack at the deadline.
    fn buffer_deliver(&mut self, seq: u64, text: String, submit: bool, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let task = cx.spawn(async move |this, cx| {
            let mut waited = Duration::ZERO;
            loop {
                cx.background_executor().timer(DELIVER_POLL_INTERVAL).await;
                waited += DELIVER_POLL_INTERVAL;
                let deadline_reached = waited >= DELIVER_DEADLINE;
                let done = this
                    .update(cx, |this, cx| {
                        if this
                            .pending_deliver
                            .as_ref()
                            .is_none_or(|pending| pending.generation != generation)
                        {
                            return true;
                        }
                        if let Some(window) = this.ops.active_window(cx) {
                            this.pending_deliver = None;
                            this.perform_deliver(window, seq, &text, submit, cx);
                            true
                        } else if deadline_reached {
                            this.pending_deliver = None;
                            this.ops.ack(seq, false, cx);
                            true
                        } else {
                            false
                        }
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        });
        self.pending_deliver = Some(PendingDeliver {
            seq,
            generation,
            _task: task,
        });
    }

    /// A newer event superseded a buffered deliver: nack it promptly so the
    /// daemon falls back to paste without waiting out its own timeout.
    fn nack_pending_deliver(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        if let Some(pending) = self.pending_deliver.take() {
            self.ops.ack(pending.seq, false, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Render, TestAppContext, UpdateGlobal as _, Window, div};
    use std::cell::RefCell;

    struct EmptyView;

    impl Render for EmptyView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            div()
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        SetMarked(AnyWindowHandle, String),
        ClearMarked(AnyWindowHandle),
        Insert(AnyWindowHandle, String),
        Enter(AnyWindowHandle),
        Ack(u64, bool),
    }

    struct FakeState {
        active_window: Option<AnyWindowHandle>,
        mark_result: bool,
        insert_result: bool,
        calls: Vec<Call>,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                active_window: None,
                mark_result: true,
                insert_result: true,
                calls: Vec::new(),
            }
        }
    }

    struct FakeOps(Rc<RefCell<FakeState>>);

    impl DictationWindowOps for FakeOps {
        fn active_window(&self, _cx: &App) -> Option<AnyWindowHandle> {
            self.0.borrow().active_window
        }

        fn set_marked_text(&self, window: AnyWindowHandle, text: &str, _cx: &mut App) -> bool {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::SetMarked(window, text.to_string()));
            state.mark_result
        }

        fn clear_marked_text(&self, window: AnyWindowHandle, _cx: &mut App) -> bool {
            self.0.borrow_mut().calls.push(Call::ClearMarked(window));
            true
        }

        fn insert_text(&self, window: AnyWindowHandle, text: &str, _cx: &mut App) -> bool {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::Insert(window, text.to_string()));
            state.insert_result
        }

        fn dispatch_enter(&self, window: AnyWindowHandle, _cx: &mut App) -> bool {
            self.0.borrow_mut().calls.push(Call::Enter(window));
            true
        }

        fn ack(&self, seq: u64, ok: bool, _cx: &mut App) {
            self.0.borrow_mut().calls.push(Call::Ack(seq, ok));
        }
    }

    struct Fixture {
        state: Rc<RefCell<FakeState>>,
        controller: Entity<DictationController>,
    }

    impl Fixture {
        fn new(cx: &mut TestAppContext) -> Self {
            let state = Rc::new(RefCell::new(FakeState::default()));
            let controller = {
                let state = state.clone();
                cx.new(|cx| DictationController::new(Rc::new(FakeOps(state)), None, cx))
            };
            Self { state, controller }
        }

        fn window(&self, cx: &mut TestAppContext) -> AnyWindowHandle {
            cx.add_window(|_, _| EmptyView).into()
        }

        fn activate(&self, window: Option<AnyWindowHandle>) {
            self.state.borrow_mut().active_window = window;
        }

        fn send(&self, cx: &mut TestAppContext, seq: u64, event: FrontendEvent) {
            self.controller.update(cx, |controller, cx| {
                controller.handle_frontend_event(ReceivedEvent { seq, event }, cx)
            });
        }

        fn calls(&self) -> Vec<Call> {
            self.state.borrow().calls.clone()
        }

        fn clear_calls(&self) {
            self.state.borrow_mut().calls.clear();
        }

        fn phase(&self, cx: &mut TestAppContext) -> DictationPhase {
            self.controller
                .read_with(cx, |controller, _| controller.phase())
        }
    }

    #[gpui::test]
    async fn test_full_cycle_marks_partials_and_delivers_with_submit(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        let window = fixture.window(cx);
        fixture.activate(Some(window));

        fixture.send(cx, 1, FrontendEvent::RecordingStarted);
        assert_eq!(fixture.phase(cx), DictationPhase::Recording);

        fixture.send(
            cx,
            2,
            FrontendEvent::Partial {
                text: "hello wor".into(),
            },
        );
        fixture.send(
            cx,
            3,
            FrontendEvent::AwaitingConfirm {
                text: "hello world".into(),
            },
        );
        assert_eq!(fixture.phase(cx), DictationPhase::AwaitingConfirm);

        fixture.send(
            cx,
            4,
            FrontendEvent::Deliver {
                text: "hello world".into(),
                submit: true,
            },
        );
        assert_eq!(fixture.phase(cx), DictationPhase::Idle);

        // Submit bookkeeping order: clear mark, insert, Enter, then ack.
        assert_eq!(
            fixture.calls(),
            vec![
                Call::SetMarked(window, "".into()),
                Call::SetMarked(window, "hello wor".into()),
                Call::SetMarked(window, "hello world".into()),
                Call::ClearMarked(window),
                Call::Insert(window, "hello world".into()),
                Call::Enter(window),
                Call::Ack(4, true),
            ]
        );
    }

    #[gpui::test]
    async fn test_deliver_without_submit_skips_enter(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        let window = fixture.window(cx);
        fixture.activate(Some(window));

        fixture.send(
            cx,
            1,
            FrontendEvent::Deliver {
                text: "hello".into(),
                submit: false,
            },
        );
        assert_eq!(
            fixture.calls(),
            vec![Call::Insert(window, "hello".into()), Call::Ack(1, true)]
        );
    }

    #[gpui::test]
    async fn test_focus_moved_mid_dictation_retargets_the_mark(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        let first = fixture.window(cx);
        let second = fixture.window(cx);

        fixture.activate(Some(first));
        fixture.send(cx, 1, FrontendEvent::RecordingStarted);
        fixture.send(cx, 2, FrontendEvent::Partial { text: "hel".into() });

        // Focus moves to another window between events: the old window's
        // mark is cleared before the new window is marked.
        fixture.activate(Some(second));
        fixture.send(
            cx,
            3,
            FrontendEvent::Partial {
                text: "hello".into(),
            },
        );

        assert_eq!(
            fixture.calls(),
            vec![
                Call::SetMarked(first, "".into()),
                Call::SetMarked(first, "hel".into()),
                Call::ClearMarked(first),
                Call::SetMarked(second, "hello".into()),
            ]
        );

        // The deliver lands in the newly focused window.
        fixture.clear_calls();
        fixture.send(
            cx,
            4,
            FrontendEvent::Deliver {
                text: "hello".into(),
                submit: false,
            },
        );
        assert_eq!(
            fixture.calls(),
            vec![
                Call::ClearMarked(second),
                Call::Insert(second, "hello".into()),
                Call::Ack(4, true),
            ]
        );
    }

    #[gpui::test]
    async fn test_cancelled_clears_the_mark(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        let window = fixture.window(cx);
        fixture.activate(Some(window));

        fixture.send(cx, 1, FrontendEvent::RecordingStarted);
        fixture.send(cx, 2, FrontendEvent::Partial { text: "hel".into() });
        fixture.send(cx, 3, FrontendEvent::Cancelled);

        assert_eq!(fixture.phase(cx), DictationPhase::Idle);
        assert_eq!(
            fixture.calls(),
            vec![
                Call::SetMarked(window, "".into()),
                Call::SetMarked(window, "hel".into()),
                Call::ClearMarked(window),
            ]
        );
    }

    #[gpui::test]
    async fn test_failed_mark_is_not_tracked(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        let window = fixture.window(cx);
        fixture.activate(Some(window));
        fixture.state.borrow_mut().mark_result = false;

        fixture.send(cx, 1, FrontendEvent::Partial { text: "hel".into() });
        fixture.send(cx, 2, FrontendEvent::Cancelled);

        // The mark attempt failed, so there is nothing to clear on cancel.
        assert_eq!(fixture.calls(), vec![Call::SetMarked(window, "hel".into())]);
    }

    #[gpui::test]
    async fn test_deliver_with_no_window_nacks_after_the_deadline(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        fixture.activate(None);

        fixture.send(
            cx,
            7,
            FrontendEvent::Deliver {
                text: "hello".into(),
                submit: true,
            },
        );
        cx.run_until_parked();
        assert_eq!(fixture.calls(), vec![], "the deliver must be buffered");

        cx.executor()
            .advance_clock(DELIVER_DEADLINE + DELIVER_POLL_INTERVAL);
        cx.run_until_parked();
        assert_eq!(fixture.calls(), vec![Call::Ack(7, false)]);
    }

    #[gpui::test]
    async fn test_buffered_deliver_lands_when_a_window_activates(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        let window = fixture.window(cx);
        fixture.activate(None);

        fixture.send(
            cx,
            8,
            FrontendEvent::Deliver {
                text: "hi".into(),
                submit: false,
            },
        );
        fixture.activate(Some(window));
        cx.executor().advance_clock(DELIVER_POLL_INTERVAL * 2);
        cx.run_until_parked();

        assert_eq!(
            fixture.calls(),
            vec![Call::Insert(window, "hi".into()), Call::Ack(8, true)]
        );

        // The poll task is done: nothing further happens at the deadline.
        fixture.clear_calls();
        cx.executor().advance_clock(DELIVER_DEADLINE * 2);
        cx.run_until_parked();
        assert_eq!(fixture.calls(), vec![]);
    }

    #[gpui::test]
    async fn test_failed_insert_nacks_and_skips_enter(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        let window = fixture.window(cx);
        fixture.activate(Some(window));
        fixture.state.borrow_mut().insert_result = false;

        fixture.send(
            cx,
            4,
            FrontendEvent::Deliver {
                text: "hello".into(),
                submit: true,
            },
        );
        assert_eq!(
            fixture.calls(),
            vec![Call::Insert(window, "hello".into()), Call::Ack(4, false)]
        );
    }

    #[gpui::test]
    async fn test_new_recording_nacks_a_buffered_deliver(cx: &mut TestAppContext) {
        let fixture = Fixture::new(cx);
        fixture.activate(None);

        fixture.send(
            cx,
            5,
            FrontendEvent::Deliver {
                text: "hello".into(),
                submit: false,
            },
        );
        fixture.send(cx, 6, FrontendEvent::RecordingStarted);
        assert_eq!(fixture.calls(), vec![Call::Ack(5, false)]);

        // The superseded poll task is inert even past the deadline.
        fixture.clear_calls();
        cx.executor().advance_clock(DELIVER_DEADLINE * 2);
        cx.run_until_parked();
        assert_eq!(fixture.calls(), vec![]);
    }

    /// End-to-end through the real [`DontSpeak`] subscription: losing the
    /// daemon connection mid-dictation clears the marked text and resets the
    /// controller to idle (a pending ack has nowhere to go, so no nack is
    /// attempted either).
    #[gpui::test]
    async fn test_losing_the_daemon_connection_resets_the_controller(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            command_palette_hooks::init(cx);
        });
        let temp = tempfile::tempdir().unwrap();
        let dontspeak = cx.new(|cx| DontSpeak::new(temp.path().join("no-daemon.sock"), cx));
        cx.run_until_parked();

        let state = Rc::new(RefCell::new(FakeState::default()));
        let controller = {
            let state = state.clone();
            cx.new(|cx| DictationController::new(Rc::new(FakeOps(state)), Some(&dontspeak), cx))
        };
        let fixture = Fixture { state, controller };
        let window = fixture.window(cx);
        fixture.activate(Some(window));

        fixture.send(cx, 1, FrontendEvent::RecordingStarted);
        fixture.send(cx, 2, FrontendEvent::Partial { text: "hel".into() });
        assert_eq!(fixture.phase(cx), DictationPhase::Recording);
        fixture.clear_calls();

        // Disabling the integration tears the connection down; the
        // controller observes the status change through its subscription.
        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.dontspeak.get_or_insert_default().enabled = Some(false);
                });
            });
        });
        cx.run_until_parked();

        assert_eq!(fixture.phase(cx), DictationPhase::Idle);
        assert_eq!(fixture.calls(), vec![Call::ClearMarked(window)]);
    }
}
