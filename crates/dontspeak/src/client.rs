//! NDJSON socket client for the DontSpeak daemon: per-OS socket/config
//! paths, one-shot requests, and the persistent frontend subscription
//! (events in, acks out) over the in-tree `net` crate's AF_UNIX streams.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _, ReadHalf, WriteHalf};
use futures::{FutureExt as _, StreamExt as _, select_biased};
use net::async_net::UnixStream;
use serde::Serialize;

use crate::protocol::{FrontendEvent, Request, Response};

/// The app tag Zed subscribes under; the daemon maps it to its per-OS
/// frontmost-app identity tables.
pub const APP_TAG: &str = "zed";

/// The DontSpeak daemon's socket: `state_dir/dontspeak.sock`.
///
/// Overridable via `ZED_DONTSPEAK_SOCKET` for testing against a daemon with
/// a non-standard state dir.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ZED_DONTSPEAK_SOCKET") {
        return PathBuf::from(path);
    }
    state_dir().join("dontspeak.sock")
}

/// DontSpeak's machine-local state root (mirrors `ds-config`'s
/// `paths.rs::state_root`):
/// - Windows: `%LOCALAPPDATA%\DontSpeak`
/// - macOS: `~/Library/Application Support/DontSpeak`
/// - Linux: `$XDG_STATE_HOME/dontspeak` (fallback `~/.local/state/dontspeak`)
fn state_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| paths::home_dir().join("AppData").join("Local"))
            .join("DontSpeak")
    }
    #[cfg(target_os = "macos")]
    {
        paths::home_dir()
            .join("Library")
            .join("Application Support")
            .join("DontSpeak")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| paths::home_dir().join(".local").join("state"))
            .join("dontspeak")
    }
}

/// DontSpeak's roaming config root — `narration-spec.md`, `config.toml` —
/// which differs from the state root on Windows and Linux (mirrors
/// `ds-config`'s `paths.rs::data_dir`):
/// - Windows: `%APPDATA%\DontSpeak`
/// - macOS: `~/Library/Application Support/DontSpeak`
/// - Linux: `$XDG_CONFIG_HOME/dontspeak` (fallback `~/.config/dontspeak`)
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| paths::home_dir().join("AppData").join("Roaming"))
            .join("DontSpeak")
    }
    #[cfg(target_os = "macos")]
    {
        paths::home_dir()
            .join("Library")
            .join("Application Support")
            .join("DontSpeak")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| paths::home_dir().join(".config"))
            .join("dontspeak")
    }
}

/// Does this error mean "no daemon is listening" (socket file missing or
/// stale) rather than a real failure? Checked against the whole error
/// chain, so callers may add context freely.
pub fn is_absent_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|io_error| {
            matches!(
                io_error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::AddrNotAvailable
            )
        })
    })
}

async fn write_line(
    writer: &mut (impl futures::io::AsyncWrite + Unpin),
    request: &(impl Serialize + std::fmt::Debug),
) -> Result<()> {
    let mut line =
        serde_json::to_string(request).with_context(|| format!("encoding {request:?}"))?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// One-shot request: connect, send `request`, read lines until a terminal
/// [`Response`], return it. Streaming (non-terminal) lines are discarded —
/// use [`subscribe`] for long-lived streams.
pub async fn request(socket_path: &Path, request: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket_path).await?;
    let (read, mut write) = stream.split();
    write_line(&mut write, request).await?;
    let mut lines = LineReader::new(read);
    loop {
        let Some(line) = lines.next_line().await? else {
            anyhow::bail!("the daemon closed the connection before a terminal response");
        };
        let response: Response =
            serde_json::from_str(&line).with_context(|| format!("parsing daemon line {line:?}"))?;
        if response.is_terminal() {
            return Ok(response);
        }
    }
}

/// A frontend event received from the daemon, with its delivery sequence
/// number (needed to ack `deliver` events).
#[derive(Debug, Clone, PartialEq)]
pub struct ReceivedEvent {
    pub seq: u64,
    pub event: FrontendEvent,
}

/// Cancellation-safe NDJSON line reader: a dropped `next_line` future never
/// loses buffered bytes (unlike `AsyncBufReadExt::read_line`, which takes
/// the caller's buffer for the duration of the future), so it is safe to
/// use inside `select!`.
struct LineReader {
    read: ReadHalf<UnixStream>,
    buffer: Vec<u8>,
}

impl LineReader {
    fn new(read: ReadHalf<UnixStream>) -> Self {
        Self {
            read,
            buffer: Vec::new(),
        }
    }

    /// The next complete line (without its trailing newline), or `None` on
    /// EOF. A trailing partial line at EOF is discarded — NDJSON peers
    /// always terminate lines before closing.
    async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(newline_ix) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=newline_ix).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8(line)?));
            }
            let mut chunk = [0u8; 4096];
            let byte_count = self.read.read(&mut chunk).await?;
            if byte_count == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..byte_count]);
        }
    }
}

/// The read side of a frontend subscription: a stream of daemon-pushed
/// dictation events.
pub struct FrontendEvents {
    lines: LineReader,
}

impl FrontendEvents {
    /// The next frontend event; `Ok(None)` when the daemon closes the
    /// connection. Non-event lines are skipped (forward compat), except a
    /// daemon `error` line, which fails the subscription (e.g. the
    /// `frontend_enabled` kill-switch rejecting the subscribe).
    pub async fn next(&mut self) -> Result<Option<ReceivedEvent>> {
        loop {
            let Some(line) = self.lines.next_line().await? else {
                return Ok(None);
            };
            if line.trim().is_empty() {
                continue;
            }
            let response: Response = serde_json::from_str(&line)
                .with_context(|| format!("parsing daemon line {line:?}"))?;
            match response {
                Response::FrontendEvent { event, seq } => {
                    return Ok(Some(ReceivedEvent { seq, event }));
                }
                Response::Error { message } => {
                    anyhow::bail!("the daemon rejected the frontend subscription: {message}");
                }
                _ => continue,
            }
        }
    }
}

/// The write side of a frontend subscription: `ack_deliver` lines back to
/// the daemon.
pub struct FrontendSink {
    write: WriteHalf<UnixStream>,
}

impl FrontendSink {
    pub async fn ack(&mut self, seq: u64, ok: bool) -> Result<()> {
        write_line(&mut self.write, &Request::AckDeliver { seq, ok }).await
    }
}

/// Open a persistent frontend subscription: connect and send
/// `subscribe_frontend`. The returned halves stay valid until either side
/// closes the connection; reconnecting with backoff is the caller's job
/// (see `DontSpeak`'s connection loop).
pub async fn subscribe(socket_path: &Path, app: &str) -> Result<(FrontendEvents, FrontendSink)> {
    let stream = UnixStream::connect(socket_path).await?;
    let (read, mut write) = stream.split();
    write_line(
        &mut write,
        &Request::SubscribeFrontend {
            app: app.to_string(),
        },
    )
    .await?;
    Ok((
        FrontendEvents {
            lines: LineReader::new(read),
        },
        FrontendSink { write },
    ))
}

/// Drive one subscription connection to completion: forward daemon events
/// into `events_tx`, and write acks arriving on `acks` back to the daemon.
///
/// Returns `Ok(())` when the owner hangs up (drops the ack sender or the
/// event receiver) and `Err(_)` when the connection itself fails or the
/// daemon closes it — the caller reconnects on `Err`.
pub async fn run_subscription(
    mut events: FrontendEvents,
    mut sink: FrontendSink,
    mut acks: mpsc::UnboundedReceiver<(u64, bool)>,
    events_tx: mpsc::UnboundedSender<ReceivedEvent>,
) -> Result<()> {
    loop {
        let mut next_event = std::pin::pin!(events.next().fuse());
        select_biased! {
            ack = acks.next() => match ack {
                Some((seq, ok)) => sink.ack(seq, ok).await?,
                None => return Ok(()),
            },
            event = next_event => match event? {
                Some(event) => {
                    if events_tx.unbounded_send(event).is_err() {
                        return Ok(());
                    }
                }
                None => anyhow::bail!("the daemon closed the frontend connection"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FrontendEvent;
    use std::io::{BufRead as _, BufReader, Write as _};

    /// A scripted fake daemon on a real AF_UNIX socket, driven step-by-step
    /// from a std thread.
    struct FakeServer {
        path: PathBuf,
        _thread: std::thread::JoinHandle<()>,
    }

    enum ServerStep {
        /// Read one line and assert it equals this exact JSON.
        ExpectLine(String),
        /// Write this exact JSON line to the client.
        SendLine(String),
        /// Read until EOF (keeps the connection open until the client
        /// hangs up).
        AwaitHangup,
        /// Close the connection (by dropping the stream).
        Close,
    }

    impl FakeServer {
        fn start(path: &Path, script: Vec<ServerStep>) -> Self {
            // Windows AF_UNIX cannot rebind an existing socket file.
            std::fs::remove_file(path).ok();
            let listener = net::UnixListener::bind(path).expect("bind fake daemon socket");
            let thread = std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream);
                for step in script {
                    match step {
                        ServerStep::ExpectLine(expected) => {
                            let mut line = String::new();
                            reader.read_line(&mut line).expect("read client line");
                            assert_eq!(line.trim_end(), expected);
                        }
                        ServerStep::SendLine(mut line) => {
                            line.push('\n');
                            reader.get_mut().write_all(line.as_bytes()).expect("write");
                        }
                        ServerStep::AwaitHangup => {
                            let mut line = String::new();
                            while reader.read_line(&mut line).map_or(false, |n| n > 0) {
                                line.clear();
                            }
                        }
                        ServerStep::Close => break,
                    }
                }
            });
            Self {
                path: path.to_path_buf(),
                _thread: thread,
            }
        }

        fn finish(self) {
            self._thread.join().expect("fake server panicked");
            std::fs::remove_file(&self.path).ok();
        }
    }

    fn frontend_event_line(json: &str) -> ServerStep {
        ServerStep::SendLine(json.to_string())
    }

    #[test]
    fn one_shot_request_reads_to_the_terminal_response() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dontspeak.sock");
        let server = FakeServer::start(
            &path,
            vec![
                ServerStep::ExpectLine(r#"{"cmd":"status"}"#.into()),
                // A non-terminal line first: the client must skip it.
                ServerStep::SendLine(r#"{"ok":"listening"}"#.into()),
                ServerStep::SendLine(
                    r#"{"ok":"status","tts_active":false,"queued":0,"paused":false,"muted":true}"#
                        .into(),
                ),
                ServerStep::Close,
            ],
        );

        let response = smol::block_on(request(&path, &Request::Status)).unwrap();
        assert_eq!(
            response,
            Response::Status {
                tts_active: false,
                queued: 0,
                paused: false,
                muted: true,
            }
        );
        server.finish();
    }

    #[test]
    fn connecting_to_a_missing_socket_is_an_absent_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("no-daemon-here.sock");
        let error = smol::block_on(request(&path, &Request::Status)).unwrap_err();
        assert!(
            is_absent_error(&error),
            "expected an absent-daemon error, got: {error:?}"
        );
    }

    #[test]
    fn subscribe_streams_events_acks_deliveries_and_reconnects() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dontspeak.sock");

        // Round 1: subscribe, three events, a deliver + ack, then the
        // daemon goes away.
        let server = FakeServer::start(
            &path,
            vec![
                ServerStep::ExpectLine(r#"{"cmd":"subscribe_frontend","app":"zed"}"#.into()),
                frontend_event_line(
                    r#"{"ok":"frontend_event","event":"recording_started","seq":1}"#,
                ),
                frontend_event_line(
                    r#"{"ok":"frontend_event","event":"partial","text":"hello wor","seq":2}"#,
                ),
                frontend_event_line(
                    r#"{"ok":"frontend_event","event":"awaiting_confirm","text":"hello world","seq":3}"#,
                ),
                frontend_event_line(
                    r#"{"ok":"frontend_event","event":"deliver","text":"hello world","submit":true,"seq":4}"#,
                ),
                ServerStep::ExpectLine(r#"{"cmd":"ack_deliver","seq":4,"ok":true}"#.into()),
                ServerStep::Close,
            ],
        );

        smol::block_on(async {
            let (mut events, mut sink) = subscribe(&path, APP_TAG).await.unwrap();
            assert_eq!(
                events.next().await.unwrap().unwrap(),
                ReceivedEvent {
                    seq: 1,
                    event: FrontendEvent::RecordingStarted,
                }
            );
            assert_eq!(
                events.next().await.unwrap().unwrap(),
                ReceivedEvent {
                    seq: 2,
                    event: FrontendEvent::Partial {
                        text: "hello wor".into()
                    },
                }
            );
            assert_eq!(
                events.next().await.unwrap().unwrap(),
                ReceivedEvent {
                    seq: 3,
                    event: FrontendEvent::AwaitingConfirm {
                        text: "hello world".into()
                    },
                }
            );
            assert_eq!(
                events.next().await.unwrap().unwrap(),
                ReceivedEvent {
                    seq: 4,
                    event: FrontendEvent::Deliver {
                        text: "hello world".into(),
                        submit: true,
                    },
                }
            );
            sink.ack(4, true).await.unwrap();
            // The daemon hangs up after the ack: clean EOF.
            assert_eq!(events.next().await.unwrap(), None);
        });
        server.finish();

        // Round 2: the daemon comes back; a fresh subscribe works.
        let server = FakeServer::start(
            &path,
            vec![
                ServerStep::ExpectLine(r#"{"cmd":"subscribe_frontend","app":"zed"}"#.into()),
                frontend_event_line(r#"{"ok":"frontend_event","event":"cancelled","seq":1}"#),
                ServerStep::Close,
            ],
        );
        smol::block_on(async {
            let (mut events, _sink) = subscribe(&path, APP_TAG).await.unwrap();
            assert_eq!(
                events.next().await.unwrap().unwrap(),
                ReceivedEvent {
                    seq: 1,
                    event: FrontendEvent::Cancelled,
                }
            );
        });
        server.finish();
    }

    #[test]
    fn daemon_error_line_fails_the_subscription() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dontspeak.sock");
        let server = FakeServer::start(
            &path,
            vec![
                ServerStep::ExpectLine(r#"{"cmd":"subscribe_frontend","app":"zed"}"#.into()),
                ServerStep::SendLine(r#"{"ok":"error","message":"frontend disabled"}"#.into()),
                ServerStep::Close,
            ],
        );
        smol::block_on(async {
            let (mut events, _sink) = subscribe(&path, APP_TAG).await.unwrap();
            let error = events.next().await.unwrap_err();
            assert!(error.to_string().contains("frontend disabled"));
        });
        server.finish();
    }

    #[test]
    fn run_subscription_pumps_events_and_acks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dontspeak.sock");
        let server = FakeServer::start(
            &path,
            vec![
                ServerStep::ExpectLine(r#"{"cmd":"subscribe_frontend","app":"zed"}"#.into()),
                frontend_event_line(
                    r#"{"ok":"frontend_event","event":"deliver","text":"hi","submit":false,"seq":7}"#,
                ),
                ServerStep::ExpectLine(r#"{"cmd":"ack_deliver","seq":7,"ok":true}"#.into()),
                frontend_event_line(r#"{"ok":"frontend_event","event":"cancelled","seq":8}"#),
                // Stay connected until the client hangs up, so the driver's
                // exit is deterministically "owner dropped the ack sender".
                ServerStep::AwaitHangup,
            ],
        );

        smol::block_on(async {
            let (events, sink) = subscribe(&path, APP_TAG).await.unwrap();
            let (ack_tx, ack_rx) = mpsc::unbounded();
            let (event_tx, mut event_rx) = mpsc::unbounded();
            let driver = run_subscription(events, sink, ack_rx, event_tx);
            let consumer = async {
                let event = event_rx.next().await.unwrap();
                assert_eq!(
                    event,
                    ReceivedEvent {
                        seq: 7,
                        event: FrontendEvent::Deliver {
                            text: "hi".into(),
                            submit: false,
                        },
                    }
                );
                ack_tx.unbounded_send((7, true)).unwrap();
                let event = event_rx.next().await.unwrap();
                assert_eq!(
                    event,
                    ReceivedEvent {
                        seq: 8,
                        event: FrontendEvent::Cancelled,
                    }
                );
                drop(ack_tx);
            };
            let (driver_result, ()) = futures::join!(driver, consumer);
            driver_result.unwrap();
        });
        server.finish();
    }

    #[test]
    fn run_subscription_reports_a_daemon_hangup_as_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dontspeak.sock");
        let server = FakeServer::start(
            &path,
            vec![
                ServerStep::ExpectLine(r#"{"cmd":"subscribe_frontend","app":"zed"}"#.into()),
                ServerStep::Close,
            ],
        );
        smol::block_on(async {
            let (events, sink) = subscribe(&path, APP_TAG).await.unwrap();
            let (_ack_tx, ack_rx) = mpsc::unbounded();
            let (event_tx, _event_rx) = mpsc::unbounded();
            let result = run_subscription(events, sink, ack_rx, event_tx).await;
            assert!(result.is_err(), "a daemon hangup must surface as Err");
        });
        server.finish();
    }
}
