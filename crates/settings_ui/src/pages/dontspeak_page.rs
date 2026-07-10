//! The "DontSpeak" sub-page of the Voice settings page: Zed's in-app
//! replacement for DontSpeak's tray/status window.
//!
//! Shows the live daemon connection (via the `dontspeak` crate's global),
//! the TTS-queue snapshot (daemon `status` verb), model readiness (daemon
//! `model_status` verb), and a test-recognition harness streaming live
//! partials from the daemon's `test_recognition_start` verb. When no daemon
//! is listening on the socket, it renders a "not installed" card with an
//! install link instead.
//!
//! Deliberately absent: a voice picker. The daemon's NDJSON protocol has no
//! voice-enumeration or config-write verb (`list_voices`/`set_config` are
//! MCP-server tools layered over `settings.json`, not daemon verbs), so
//! voice selection stays in DontSpeak's own UI until the daemon grows one —
//! at which point the `dontspeak` crate's protocol mirror must be extended
//! in lockstep (see its header comment).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use dontspeak::{
    DontSpeak, Status, client,
    protocol::{Request, Response},
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
use gpui::{AnyElement, AppContext as _, Entity, EntityId, Global, ScrollHandle, Task, WeakEntity};
use net::async_net::UnixStream;
use serde_json::Value;
use ui::{Divider, Tooltip, prelude::*};

use crate::SettingsWindow;

pub(crate) fn render_dontspeak_page(
    _settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(dontspeak) = DontSpeak::global(cx) else {
        return v_flex()
            .id("dontspeak-page")
            .track_scroll(scroll_handle)
            .size_full()
            .pt_2p5()
            .px_8()
            .pb_16()
            .child(dashed_card(
                "The DontSpeak integration did not initialize in this build of Zed.",
                cx,
            ))
            .into_any_element();
    };

    let state = DontSpeakPageState::get_or_create(&dontspeak, cx);
    let status = dontspeak.read(cx).status();

    let content = match status {
        Status::Absent => render_not_installed(&dontspeak, cx),
        Status::Disabled => render_disabled(cx),
        Status::Connecting => render_connecting(cx),
        Status::Error(error) => render_error(error.to_string().into(), &dontspeak, cx),
        Status::Connected => render_connected(&state, cx),
    };

    v_flex()
        .id("dontspeak-page")
        .track_scroll(scroll_handle)
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .overflow_y_scroll()
        .child(content)
        .into_any_element()
}

/// The page's shared state, global so it survives settings-window
/// navigation (the sub-page render function is a plain `fn` and cannot
/// store state in the window itself).
struct GlobalDontSpeakPage {
    state: Entity<DontSpeakPageState>,
    /// The settings window currently observing `state` — re-observed when a
    /// new settings window is opened.
    observer: Option<EntityId>,
}

impl Global for GlobalDontSpeakPage {}

/// A parsed reply to the daemon's `status` verb: the TTS-queue snapshot.
#[derive(Clone, Copy)]
struct QueueSnapshot {
    tts_active: bool,
    queued: usize,
    paused: bool,
    muted: bool,
}

/// State of the "Test Recognition" harness.
enum TestRecognition {
    Idle,
    /// `test_recognition_start` sent; waiting for the daemon's `listening`.
    Starting,
    Listening {
        partial: SharedString,
    },
    Finished {
        transcript: SharedString,
    },
    Failed {
        message: SharedString,
    },
}

/// A progress event from the background test-recognition stream.
enum TestEvent {
    Listening,
    Partial(String),
    Transcript(String),
    Failed(String),
}

struct DontSpeakPageState {
    dontspeak: Entity<DontSpeak>,
    /// `None` while a fetch is in flight (or the daemon is unreachable).
    queue: Option<Result<QueueSnapshot, SharedString>>,
    model_status: Option<Result<Value, SharedString>>,
    test: TestRecognition,
    _refresh_task: Option<Task<()>>,
    _test_task: Option<Task<()>>,
    _dontspeak_subscription: gpui::Subscription,
}

impl DontSpeakPageState {
    fn get_or_create(
        dontspeak: &Entity<DontSpeak>,
        cx: &mut Context<SettingsWindow>,
    ) -> Entity<Self> {
        if !cx.has_global::<GlobalDontSpeakPage>() {
            let state = cx.new(|cx| DontSpeakPageState::new(dontspeak.clone(), cx));
            cx.set_global(GlobalDontSpeakPage {
                state,
                observer: None,
            });
        }
        let window_id = cx.entity_id();
        let global = cx.global::<GlobalDontSpeakPage>();
        let state = global.state.clone();
        if global.observer != Some(window_id) {
            cx.observe(&state, |_, _, cx| cx.notify()).detach();
            cx.global_mut::<GlobalDontSpeakPage>().observer = Some(window_id);
        }
        state
    }

    fn new(dontspeak: Entity<DontSpeak>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&dontspeak, |this, _, event, cx| {
            if let dontspeak::Event::StatusChanged = event {
                this.handle_status_changed(cx);
            }
        });
        let mut this = Self {
            dontspeak,
            queue: None,
            model_status: None,
            test: TestRecognition::Idle,
            _refresh_task: None,
            _test_task: None,
            _dontspeak_subscription: subscription,
        };
        this.refresh(cx);
        this
    }

    fn handle_status_changed(&mut self, cx: &mut Context<Self>) {
        if self.dontspeak.read(cx).status().is_connected() {
            self.refresh(cx);
        } else {
            // The daemon went away: drop stale data and any in-flight test.
            self.queue = None;
            self.model_status = None;
            self.test = TestRecognition::Idle;
            self._refresh_task = None;
            self._test_task = None;
        }
        cx.notify();
    }

    /// Fetch the queue snapshot and model readiness on fresh one-shot
    /// connections.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let queue_task = self.dontspeak.read(cx).request(Request::Status, cx);
        let model_task = self.dontspeak.read(cx).request(Request::ModelStatus, cx);
        self.queue = None;
        self.model_status = None;
        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            let queue = match queue_task.await {
                Ok(Response::Status {
                    tts_active,
                    queued,
                    paused,
                    muted,
                }) => Ok(QueueSnapshot {
                    tts_active,
                    queued,
                    paused,
                    muted,
                }),
                Ok(Response::Error { message }) => Err(message.into()),
                Ok(_) => Err("Unexpected reply to the status request.".into()),
                Err(error) => Err(format!("{error:#}").into()),
            };
            let model_status = match model_task.await {
                Ok(Response::ModelStatus { status }) => Ok(status),
                Ok(Response::Error { message }) => Err(message.into()),
                Ok(_) => Err("Unexpected reply to the model-status request.".into()),
                Err(error) => Err(format!("{error:#}").into()),
            };
            this.update(cx, |this, cx| {
                this.queue = Some(queue);
                this.model_status = Some(model_status);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn test_running(&self) -> bool {
        matches!(
            self.test,
            TestRecognition::Starting | TestRecognition::Listening { .. }
        )
    }

    fn start_test(&mut self, cx: &mut Context<Self>) {
        if self.test_running() {
            return;
        }
        self.test = TestRecognition::Starting;
        let socket_path = client::socket_path();
        let (event_tx, mut event_rx) = mpsc::unbounded();
        let io_task = cx.background_spawn(run_test_recognition(socket_path, event_tx));
        self._test_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.next().await {
                if this
                    .update(cx, |this, cx| this.apply_test_event(event, cx))
                    .is_err()
                {
                    return;
                }
            }
            io_task.await;
        }));
        cx.notify();
    }

    /// Ends the active test-recognition session. Sent on a second, one-shot
    /// connection: the first is busy streaming partials, and answers with
    /// its terminal transcript once the daemon stops the session.
    fn stop_test(&self, cx: &mut Context<Self>) {
        self.dontspeak
            .read(cx)
            .request(Request::TestRecognitionStop, cx)
            .detach_and_log_err(cx);
    }

    fn apply_test_event(&mut self, event: TestEvent, cx: &mut Context<Self>) {
        self.test = match event {
            TestEvent::Listening => TestRecognition::Listening {
                partial: SharedString::default(),
            },
            TestEvent::Partial(text) => TestRecognition::Listening {
                partial: text.into(),
            },
            TestEvent::Transcript(text) => TestRecognition::Finished {
                transcript: text.into(),
            },
            TestEvent::Failed(message) => TestRecognition::Failed {
                message: message.into(),
            },
        };
        cx.notify();
    }
}

/// Drive one `test_recognition_start` connection, forwarding progress into
/// `tx`. Any failure is reported as a terminal [`TestEvent::Failed`].
async fn run_test_recognition(socket_path: PathBuf, tx: mpsc::UnboundedSender<TestEvent>) {
    if let Err(error) = stream_test_recognition(&socket_path, &tx).await {
        tx.unbounded_send(TestEvent::Failed(format!("{error:#}")))
            .ok();
    }
}

async fn stream_test_recognition(
    socket_path: &Path,
    tx: &mpsc::UnboundedSender<TestEvent>,
) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .context("connecting to the DontSpeak daemon")?;
    let (mut read, mut write) = stream.split();
    let mut line = serde_json::to_string(&Request::TestRecognitionStart)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;

    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        while let Some(newline_ix) = buffer.iter().position(|&byte| byte == b'\n') {
            let mut raw: Vec<u8> = buffer.drain(..=newline_ix).collect();
            raw.pop();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
            let text = String::from_utf8(raw)?;
            if text.trim().is_empty() {
                continue;
            }
            let response: Response = serde_json::from_str(&text)
                .with_context(|| format!("parsing daemon line {text:?}"))?;
            let event = match response {
                Response::Listening => TestEvent::Listening,
                Response::Partial { text } => TestEvent::Partial(text),
                Response::Transcript { text } => TestEvent::Transcript(text),
                Response::Error { message } => bail!("{message}"),
                other if other.is_terminal() => {
                    bail!("unexpected daemon reply to test recognition: {other:?}")
                }
                _ => continue,
            };
            let terminal = matches!(event, TestEvent::Transcript(_));
            if tx.unbounded_send(event).is_err() || terminal {
                return Ok(());
            }
        }
        let byte_count = read.read(&mut chunk).await?;
        if byte_count == 0 {
            bail!("the daemon closed the connection before a final transcript");
        }
        buffer.extend_from_slice(&chunk[..byte_count]);
    }
}

// --- Rendering ---

fn render_not_installed(dontspeak: &Entity<DontSpeak>, cx: &App) -> AnyElement {
    let dontspeak = dontspeak.downgrade();
    v_flex()
        .p_4()
        .gap_2()
        .items_center()
        .justify_center()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .rounded_sm()
        .child(Label::new("DontSpeak is not installed or not running"))
        .child(
            Label::new(
                "DontSpeak provides CapsLock dictation and spoken narration of agent replies. \
                 Zed connects to its local daemon automatically once it is running.",
            )
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("dontspeak-install", "Get DontSpeak")
                        .style(ButtonStyle::Outlined)
                        .tab_index(0_isize)
                        .on_click(|_, _, cx| cx.open_url("https://dontspeak.org")),
                )
                .child(check_again_button(dontspeak)),
        )
        .into_any_element()
}

fn render_disabled(cx: &App) -> AnyElement {
    dashed_card(
        "The DontSpeak integration is turned off. Enable it on the Voice page to connect to the daemon.",
        cx,
    )
}

fn render_connecting(cx: &App) -> AnyElement {
    dashed_card("Connecting to the DontSpeak daemon…", cx)
}

fn render_error(error: SharedString, dontspeak: &Entity<DontSpeak>, cx: &App) -> AnyElement {
    let dontspeak = dontspeak.downgrade();
    v_flex()
        .p_4()
        .gap_2()
        .items_center()
        .justify_center()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .rounded_sm()
        .child(Label::new("Failed to connect to the DontSpeak daemon"))
        .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
        .child(check_again_button(dontspeak))
        .into_any_element()
}

fn check_again_button(dontspeak: WeakEntity<DontSpeak>) -> Button {
    Button::new("dontspeak-reconnect", "Check Again")
        .style(ButtonStyle::Outlined)
        .tab_index(0_isize)
        .on_click(move |_, _, cx| {
            dontspeak
                .update(cx, |dontspeak, cx| dontspeak.reconnect(cx))
                .ok();
        })
}

fn render_connected(state: &Entity<DontSpeakPageState>, cx: &App) -> AnyElement {
    let page = state.read(cx);
    v_flex()
        .w_full()
        .gap_4()
        .child(render_daemon_section(state, page))
        .child(Divider::horizontal())
        .child(render_models_section(page))
        .child(Divider::horizontal())
        .child(render_test_section(state, page))
        .into_any_element()
}

fn render_daemon_section(
    state: &Entity<DontSpeakPageState>,
    page: &DontSpeakPageState,
) -> AnyElement {
    let refresh_state = state.downgrade();
    let mut section = v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .child(
                    v_flex().child(Label::new("Daemon")).child(
                        Label::new("Connection and speech-queue state.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    IconButton::new("dontspeak-refresh", IconName::RotateCw)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Refresh"))
                        .tab_index(0_isize)
                        .on_click(move |_, _, cx| {
                            refresh_state.update(cx, |state, cx| state.refresh(cx)).ok();
                        }),
                ),
        )
        .child(status_row(
            "Status",
            "Connected",
            Some((IconName::Check, Color::Success)),
        ));

    match &page.queue {
        None => {
            section = section.child(
                Label::new("Loading queue state…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        Some(Err(error)) => {
            section = section.child(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            );
        }
        Some(Ok(queue)) => {
            let speech = if queue.tts_active {
                format!("Speaking ({} queued)", queue.queued)
            } else if queue.queued > 0 {
                format!("Idle ({} queued)", queue.queued)
            } else {
                "Idle".to_string()
            };
            section = section
                .child(status_row("Speech", &speech, None))
                .child(status_row(
                    "Paused",
                    if queue.paused { "Yes" } else { "No" },
                    None,
                ))
                .child(status_row(
                    "Muted",
                    if queue.muted { "Yes" } else { "No" },
                    None,
                ));
        }
    }

    section.into_any_element()
}

fn render_models_section(page: &DontSpeakPageState) -> AnyElement {
    let mut section = v_flex().w_full().gap_2().child(
        v_flex().child(Label::new("Models")).child(
            Label::new("Readiness of DontSpeak's speech-recognition and speech models.")
                .size(LabelSize::Small)
                .color(Color::Muted),
        ),
    );

    match &page.model_status {
        None => {
            section = section.child(
                Label::new("Loading model status…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        Some(Err(error)) => {
            section = section.child(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            );
        }
        Some(Ok(status)) => {
            let rows = model_status_rows(status);
            if rows.is_empty() {
                section = section.child(
                    Label::new("The daemon reported no models.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            for (name, value) in rows {
                let (text, icon) = match value {
                    Value::Bool(true) => {
                        ("Ready".to_string(), Some((IconName::Check, Color::Success)))
                    }
                    Value::Bool(false) => (
                        "Not ready".to_string(),
                        Some((IconName::Close, Color::Muted)),
                    ),
                    Value::String(text) => (text, None),
                    Value::Number(number) => (number.to_string(), None),
                    Value::Null => ("—".to_string(), None),
                    other => (other.to_string(), None),
                };
                section = section.child(status_row(name, &text, icon));
            }
        }
    }

    section.into_any_element()
}

/// Flatten the daemon's raw `model_status` JSON object into displayable
/// rows: top-level scalars become rows, and one level of nesting becomes
/// dotted "parent.child" rows. The shape is daemon-owned (kept as raw JSON
/// in the protocol mirror), so rendering stays generic.
fn model_status_rows(status: &Value) -> Vec<(SharedString, Value)> {
    let Some(object) = status.as_object() else {
        return vec![("status".into(), status.clone())];
    };
    let mut rows = Vec::new();
    for (key, value) in object {
        match value {
            Value::Object(nested) => {
                for (nested_key, nested_value) in nested {
                    rows.push((
                        SharedString::from(format!("{key}.{nested_key}")),
                        nested_value.clone(),
                    ));
                }
            }
            other => rows.push((SharedString::from(key.clone()), other.clone())),
        }
    }
    rows
}

fn render_test_section(
    state: &Entity<DontSpeakPageState>,
    page: &DontSpeakPageState,
) -> AnyElement {
    let running = page.test_running();
    let button = if running {
        let stop_state = state.downgrade();
        Button::new("dontspeak-test-stop", "Stop")
            .style(ButtonStyle::Outlined)
            .tab_index(0_isize)
            .on_click(move |_, _, cx| {
                stop_state.update(cx, |state, cx| state.stop_test(cx)).ok();
            })
    } else {
        let start_state = state.downgrade();
        Button::new("dontspeak-test-start", "Start Test")
            .style(ButtonStyle::Outlined)
            .tab_index(0_isize)
            .on_click(move |_, _, cx| {
                start_state
                    .update(cx, |state, cx| state.start_test(cx))
                    .ok();
            })
    };

    let status_line: Option<AnyElement> = match &page.test {
        TestRecognition::Idle => None,
        TestRecognition::Starting => Some(
            Label::new("Starting…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        ),
        TestRecognition::Listening { partial } => Some(
            v_flex()
                .gap_1()
                .child(
                    Label::new("Listening — speak now")
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
                .when(!partial.is_empty(), |this| {
                    this.child(
                        Label::new(partial.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
        ),
        TestRecognition::Finished { transcript } => Some(
            v_flex()
                .gap_1()
                .child(
                    Label::new("Transcript")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(transcript.clone()).size(LabelSize::Small))
                .into_any_element(),
        ),
        TestRecognition::Failed { message } => Some(
            Label::new(message.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
        ),
    };

    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .child(
                    v_flex().child(Label::new("Test Recognition")).child(
                        Label::new(
                            "Record a short utterance through the daemon to verify the microphone \
                             and speech-recognition setup. Partial results appear live.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                )
                .child(button),
        )
        .children(status_line)
        .into_any_element()
}

fn status_row(
    name: impl Into<SharedString>,
    value: &str,
    icon: Option<(IconName, Color)>,
) -> AnyElement {
    h_flex()
        .w_full()
        .gap_2()
        .child(
            Label::new(name.into())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(div().flex_1())
        .when_some(icon, |this, (icon, color)| {
            this.child(Icon::new(icon).size(IconSize::Small).color(color))
        })
        .child(Label::new(value.to_string()).size(LabelSize::Small))
        .into_any_element()
}

fn dashed_card(message: &'static str, cx: &App) -> AnyElement {
    h_flex()
        .p_4()
        .justify_center()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .rounded_sm()
        .child(
            Label::new(message)
                .color(Color::Muted)
                .size(LabelSize::Small),
        )
        .into_any_element()
}
