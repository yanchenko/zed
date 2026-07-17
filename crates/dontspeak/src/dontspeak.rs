//! Core (non-UI) side of Zed's native DontSpeak integration.
//!
//! Zed is a *frontend client* of the DontSpeak daemon (`dontspeakd`): the
//! daemon keeps sole ownership of CapsLock, the mic, STT/TTS engines and
//! narration logic, and Zed subscribes to dictation events over the
//! daemon's local NDJSON socket. This crate owns the connection lifecycle
//! ([`DontSpeak`] global entity + [`Status`] projection), the wire-protocol
//! mirror ([`protocol`]), the socket client ([`client`]), and the settings
//! ([`DontSpeakSettings`]). Rendering (status-bar button, dictation marked
//! text) lives in `dontspeak_ui`.
//!
//! Everything degrades silently: when the daemon socket is absent, the
//! status is [`Status::Absent`], no UI is shown, and Zed behaves exactly
//! like upstream.

pub mod client;
mod dontspeak_settings;
pub mod protocol;

use std::any::TypeId;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use command_palette_hooks::CommandPaletteFilter;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Task, TaskExt as _,
    WeakEntity, actions,
};
use settings::{Settings as _, SettingsStore};

pub use crate::client::ReceivedEvent;
pub use crate::dontspeak_settings::{DontSpeakSettings, NarratePanelAgents};
pub use crate::protocol::{FrontendEvent, Request, Response};

actions!(
    dontspeak,
    [
        /// Toggles CapsLock dictation (start or confirm/stop recording).
        ToggleDictation,
        /// Stops all in-flight DontSpeak speech (global barge-in).
        StopSpeech,
        /// Opens the Voice settings page.
        OpenVoiceSettings,
        /// Reconnects to the DontSpeak daemon.
        Reconnect
    ]
);

/// How long to wait before the first reconnect attempt; doubles per attempt
/// up to [`MAX_RECONNECT_DELAY`].
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(500);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// Registers the global [`DontSpeak`] entity and the crate's global
/// actions. Call once at startup, after settings are initialized.
pub fn init(cx: &mut App) {
    let dontspeak = cx.new(|cx| DontSpeak::new(client::socket_path(), cx));
    cx.set_global(GlobalDontSpeak(dontspeak));

    cx.on_action(|_: &Reconnect, cx| {
        if let Some(dontspeak) = DontSpeak::global(cx) {
            dontspeak.update(cx, |dontspeak, cx| dontspeak.reconnect(cx));
        }
    });
    cx.on_action(|_: &StopSpeech, cx| {
        if let Some(dontspeak) = DontSpeak::global(cx) {
            dontspeak.read(cx).stop_speech(cx);
        }
    });
}

struct GlobalDontSpeak(Entity<DontSpeak>);

impl Global for GlobalDontSpeak {}

/// Public projection of the daemon-connection state, for UI and the
/// command-palette filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The integration is turned off in settings.
    Disabled,
    /// A connection attempt is in flight (also between reconnect attempts
    /// after losing an established connection).
    Connecting,
    /// Subscribed to the daemon as a frontend.
    Connected,
    /// Connecting failed with a real error (not just a missing daemon).
    Error(Arc<str>),
    /// No daemon is listening on the socket — DontSpeak is not installed
    /// or not running. Zed behaves exactly like upstream.
    Absent,
}

impl Status {
    pub fn is_connected(&self) -> bool {
        matches!(self, Status::Connected)
    }
}

enum DaemonConnection {
    Disabled,
    Connecting,
    Connected {
        acks: mpsc::UnboundedSender<(u64, bool)>,
    },
    Error(Arc<str>),
    Absent,
}

/// Events emitted by the global [`DontSpeak`] entity.
pub enum Event {
    /// The connection status changed; re-read [`DontSpeak::status`].
    StatusChanged,
    /// A dictation-lifecycle event arrived from the daemon. `Deliver`
    /// events must be answered via [`DontSpeak::ack_deliver`].
    FrontendEvent(ReceivedEvent),
}

/// The global DontSpeak daemon connection.
pub struct DontSpeak {
    socket_path: PathBuf,
    connection: DaemonConnection,
    /// Bumped on every (re)start/stop; stale connection tasks observe the
    /// mismatch and bow out (belt-and-braces on top of task cancellation).
    generation: usize,
    _connection_task: Option<Task<()>>,
}

impl EventEmitter<Event> for DontSpeak {}

impl DontSpeak {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDontSpeak>()
            .map(|global| global.0.clone())
    }

    pub fn new(socket_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            socket_path,
            connection: DaemonConnection::Disabled,
            generation: 0,
            _connection_task: None,
        };
        this.handle_settings_changed(cx);
        cx.observe_global::<SettingsStore>(|this, cx| this.handle_settings_changed(cx))
            .detach();
        this
    }

    pub fn status(&self) -> Status {
        match &self.connection {
            DaemonConnection::Disabled => Status::Disabled,
            DaemonConnection::Connecting => Status::Connecting,
            DaemonConnection::Connected { .. } => Status::Connected,
            DaemonConnection::Error(error) => Status::Error(error.clone()),
            DaemonConnection::Absent => Status::Absent,
        }
    }

    /// One-shot request to the daemon on a fresh connection (the
    /// subscription connection carries only events and acks).
    pub fn request(&self, request: Request, cx: &App) -> Task<Result<Response>> {
        let socket_path = self.socket_path.clone();
        cx.background_spawn(async move { client::request(&socket_path, &request).await })
    }

    /// Global barge-in: stop all in-flight speech. Fire-and-forget.
    pub fn stop_speech(&self, cx: &App) {
        self.request(Request::StopSpeech { session: None }, cx)
            .detach_and_log_err(cx);
    }

    /// Acknowledge a `deliver` frontend event on the live subscription
    /// connection. No-op (a warning) when not connected — the daemon then
    /// times out and falls back to its clipboard-paste path, so no
    /// utterance is lost.
    pub fn ack_deliver(&self, seq: u64, ok: bool) {
        match &self.connection {
            DaemonConnection::Connected { acks } => {
                if acks.unbounded_send((seq, ok)).is_err() {
                    log::warn!("dontspeak: dropping ack {seq}: subscription is gone");
                }
            }
            _ => log::warn!("dontspeak: dropping ack {seq}: not connected"),
        }
    }

    /// Restart the connection loop now (e.g. the user just started the
    /// daemon and doesn't want to wait out the backoff).
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        if DontSpeakSettings::get_global(cx).enabled {
            self.start(cx);
        }
    }

    fn handle_settings_changed(&mut self, cx: &mut Context<Self>) {
        let enabled = DontSpeakSettings::get_global(cx).enabled;
        let running = !matches!(self.connection, DaemonConnection::Disabled);
        if enabled && !running {
            self.start(cx);
        } else if !enabled && running {
            self.stop(cx);
        } else {
            // No connection-state change (e.g. starting up with the
            // integration disabled), but the palette filter still needs to
            // reflect the current status.
            self.update_action_visibilities(cx);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let socket_path = self.socket_path.clone();
        self.set_connection(DaemonConnection::Connecting, cx);
        self._connection_task = Some(cx.spawn(async move |this, cx| {
            Self::run_connection_loop(this, generation, socket_path, cx).await;
        }));
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self._connection_task = None;
        self.set_connection(DaemonConnection::Disabled, cx);
    }

    /// Set the connection state — but only if `generation` is still
    /// current. Returns false when the caller's connection loop is stale
    /// and must exit.
    fn set_connection_if_current(
        this: &WeakEntity<Self>,
        generation: usize,
        connection: DaemonConnection,
        cx: &mut AsyncApp,
    ) -> bool {
        this.update(cx, |this, cx| {
            if this.generation != generation {
                return false;
            }
            this.set_connection(connection, cx);
            true
        })
        .unwrap_or(false)
    }

    fn set_connection(&mut self, connection: DaemonConnection, cx: &mut Context<Self>) {
        self.connection = connection;
        self.update_action_visibilities(cx);
        cx.emit(Event::StatusChanged);
        cx.notify();
    }

    async fn run_connection_loop(
        this: WeakEntity<Self>,
        generation: usize,
        socket_path: PathBuf,
        cx: &mut AsyncApp,
    ) {
        if std::env::var("ZED_FORCE_DONTSPEAK_ERROR").is_ok() {
            Self::set_connection_if_current(
                &this,
                generation,
                DaemonConnection::Error(
                    "Forced error for testing (ZED_FORCE_DONTSPEAK_ERROR)".into(),
                ),
                cx,
            );
            return;
        }

        let mut delay = INITIAL_RECONNECT_DELAY;
        loop {
            match client::subscribe(&socket_path, client::APP_TAG).await {
                Ok((events, sink)) => {
                    delay = INITIAL_RECONNECT_DELAY;
                    let (ack_tx, ack_rx) = mpsc::unbounded();
                    let (event_tx, mut event_rx) = mpsc::unbounded();
                    if !Self::set_connection_if_current(
                        &this,
                        generation,
                        DaemonConnection::Connected { acks: ack_tx },
                        cx,
                    ) {
                        return;
                    }
                    log::info!("dontspeak: subscribed to the daemon at {socket_path:?}");

                    let driver = cx
                        .background_spawn(client::run_subscription(events, sink, ack_rx, event_tx));
                    while let Some(event) = event_rx.next().await {
                        let forwarded = this
                            .update(cx, |this, cx| {
                                if this.generation != generation {
                                    return false;
                                }
                                cx.emit(Event::FrontendEvent(event));
                                true
                            })
                            .unwrap_or(false);
                        if !forwarded {
                            return;
                        }
                    }
                    if let Err(error) = driver.await {
                        log::warn!("dontspeak: lost the daemon connection: {error:#}");
                    }
                    if !Self::set_connection_if_current(
                        &this,
                        generation,
                        DaemonConnection::Connecting,
                        cx,
                    ) {
                        return;
                    }
                }
                Err(error) => {
                    let connection = if client::is_absent_error(&error) {
                        log::debug!("dontspeak: daemon not running at {socket_path:?}");
                        DaemonConnection::Absent
                    } else {
                        log::warn!("dontspeak: failed to connect: {error:#}");
                        DaemonConnection::Error(format!("{error:#}").into())
                    };
                    if !Self::set_connection_if_current(&this, generation, connection, cx) {
                        return;
                    }
                }
            }

            // Back off between every attempt, including while the daemon is
            // absent: the delay only resets on a successful subscribe (above),
            // so an uninstalled/stopped daemon settles into a cheap poll at
            // `MAX_RECONNECT_DELAY`. The `Reconnect` action calls `start()`,
            // which resets it for an immediate retry.
            cx.background_executor().timer(delay).await;
            delay = (delay * 2).min(MAX_RECONNECT_DELAY);
        }
    }

    fn update_action_visibilities(&self, cx: &mut App) {
        let connected_actions = [TypeId::of::<ToggleDictation>(), TypeId::of::<StopSpeech>()];
        let enabled_actions = [TypeId::of::<OpenVoiceSettings>(), TypeId::of::<Reconnect>()];
        let status = self.status();
        CommandPaletteFilter::update_global(cx, |filter, _| match status {
            Status::Disabled => {
                filter.hide_action_types(&connected_actions);
                filter.hide_action_types(&enabled_actions);
            }
            Status::Connected => {
                filter.show_action_types(connected_actions.iter().chain(&enabled_actions));
            }
            Status::Connecting | Status::Error(_) | Status::Absent => {
                filter.hide_action_types(&connected_actions);
                filter.show_action_types(&enabled_actions);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            command_palette_hooks::init(cx);
        });
    }

    fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.dontspeak.get_or_insert_default().enabled = Some(enabled);
                });
            });
        });
    }

    #[gpui::test]
    async fn test_disabled_by_settings_never_connects(cx: &mut TestAppContext) {
        init_test(cx);
        set_enabled(cx, false);

        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("dontspeak.sock");
        let dontspeak = cx.new(|cx| DontSpeak::new(socket_path, cx));
        assert_eq!(
            dontspeak.read_with(cx, |this, _| this.status()),
            Status::Disabled
        );
        cx.run_until_parked();
        assert_eq!(
            dontspeak.read_with(cx, |this, _| this.status()),
            Status::Disabled
        );
    }

    #[gpui::test]
    async fn test_starting_disabled_hides_all_palette_actions(cx: &mut TestAppContext) {
        init_test(cx);
        set_enabled(cx, false);

        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("dontspeak.sock");
        let _dontspeak = cx.new(|cx| DontSpeak::new(socket_path, cx));
        cx.update(|cx| {
            let filter = CommandPaletteFilter::try_global(cx).unwrap();
            assert!(filter.is_hidden(&ToggleDictation));
            assert!(filter.is_hidden(&StopSpeech));
            assert!(filter.is_hidden(&OpenVoiceSettings));
            assert!(filter.is_hidden(&Reconnect));
        });
    }

    #[gpui::test]
    async fn test_absent_daemon_degrades_silently(cx: &mut TestAppContext) {
        init_test(cx);

        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("no-daemon.sock");
        let dontspeak = cx.new(|cx| DontSpeak::new(socket_path, cx));
        cx.run_until_parked();
        assert_eq!(
            dontspeak.read_with(cx, |this, _| this.status()),
            Status::Absent
        );

        // Disabling tears the connection loop down.
        set_enabled(cx, false);
        cx.run_until_parked();
        assert_eq!(
            dontspeak.read_with(cx, |this, _| this.status()),
            Status::Disabled
        );

        // Re-enabling starts it again.
        set_enabled(cx, true);
        cx.run_until_parked();
        assert_eq!(
            dontspeak.read_with(cx, |this, _| this.status()),
            Status::Absent
        );
    }
}
