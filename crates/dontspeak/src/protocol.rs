//! Serde mirror of the DontSpeak daemon's NDJSON wire protocol — only the
//! shapes Zed uses.
//!
//! Mirrored (not a git dependency) from DontSpeak's
//! `rust/crates/ds-ipc/src/protocol.rs` as of DontSpeak commit `98573c0`,
//! plus the frontend-subscription additions (`subscribe_frontend`,
//! `ack_deliver`, `frontend_event`) and the `narrate_batch` verb specified in
//! `docs/superpowers/plans/2026-07-09-dontspeak-zed-integration.md` (Tasks B1
//! and B4) and documented in DontSpeak's `docs/ZED-FRONTEND.md`.
//!
//! One JSON [`Request`] per line client → daemon; one-or-more JSON
//! [`Response`] lines daemon → client. A subscribed frontend connection
//! stays open indefinitely: the daemon streams non-terminal
//! `frontend_event` lines, and the client writes `ack_deliver` lines back
//! on the same connection.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A client → daemon request line, e.g. `{"cmd":"status"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Subscribe this connection as a native frontend for `app` (Zed sends
    /// `"zed"`). The daemon takes the connection over and streams
    /// [`Response::FrontendEvent`] lines on it for as long as it lives.
    SubscribeFrontend { app: String },
    /// Acknowledge a `deliver` frontend event by its `seq`. `ok: false`
    /// (or no ack within the daemon's deadline) makes the daemon fall back
    /// to its classic clipboard-paste path for that utterance.
    AckDeliver { seq: u64, ok: bool },
    /// Feed an agent message's cumulative text to the daemon's
    /// blockquote-narration logic. `key` dedups re-sends of the same entry;
    /// `is_final` runs the end-of-turn shorts fallback.
    NarrateBatch {
        session: String,
        key: String,
        text: String,
        is_final: bool,
    },
    /// Mark this session as the ACTIVE terminal/thread — the one a prompt
    /// was just submitted to. `synthetic` stays `false` for real user
    /// submits (it exists for harness-injected continuations).
    MarkActive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default)]
        synthetic: bool,
    },
    /// A session/thread closed for good: drop its queued + in-flight speech
    /// and reclaim its session-scoped voice state.
    SessionEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Barge-in: stop in-flight speech. `None` is the global hard barge.
    StopSpeech {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Snapshot of the TTS queue's playback state → [`Response::Status`].
    Status,
    /// Model presence + per-subsystem running state → [`Response::ModelStatus`].
    ModelStatus,
    /// Start a live "test recognition" session; the daemon streams
    /// [`Response::Listening`]/[`Response::Partial`] lines, ending with a
    /// terminal [`Response::Transcript`].
    TestRecognitionStart,
    /// Stop the active test-recognition session (sent on a SECOND
    /// connection, since the first is busy streaming).
    TestRecognitionStop,
}

/// A daemon → client response line, e.g. `{"ok":"done"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum Response {
    /// Reply to [`Request::Status`] (TERMINAL).
    Status {
        tts_active: bool,
        queued: usize,
        paused: bool,
        muted: bool,
    },
    /// Generic success terminator for a request that returns no payload.
    Done,
    /// Test recognition: mic open, speak now (non-terminal).
    Listening,
    /// Test recognition: live partial transcript (non-terminal).
    Partial { text: String },
    /// Test recognition: final transcript (TERMINAL).
    Transcript { text: String },
    /// Model presence + removability + running state (TERMINAL). Kept as a
    /// raw JSON object, exactly as the daemon sends it.
    ModelStatus { status: Value },
    /// Terminal error for any request.
    Error { message: String },
    /// A dictation-lifecycle event streamed to a subscribed frontend
    /// (non-terminal — the subscription outlives every event).
    FrontendEvent {
        #[serde(flatten)]
        event: FrontendEvent,
        seq: u64,
    },
    /// Forward-compat fallback: any `ok` tag this build doesn't know about
    /// decodes to `Unknown` instead of hard-erroring, mirroring the
    /// daemon-side contract. Treated as TERMINAL so one-shot readers stop
    /// deterministically.
    #[serde(other)]
    Unknown,
}

/// The dictation-lifecycle payload of a [`Response::FrontendEvent`] line,
/// flattened into it on the wire via its `event` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum FrontendEvent {
    /// A CapsLock dictation started; the mic is live.
    RecordingStarted,
    /// Live partial transcript of the in-flight dictation.
    Partial { text: String },
    /// Recording ended; the daemon is waiting for the confirm gesture.
    AwaitingConfirm { text: String },
    /// Final transcript to insert into the focused input. Must be answered
    /// with [`Request::AckDeliver`] for the same `seq`; when `submit` is
    /// true the frontend also submits (Enter) after inserting.
    Deliver { text: String, submit: bool },
    /// The dictation was cancelled (long-press, empty final, teardown).
    Cancelled,
    /// The daemon refused the dictation (e.g. deferred-confirm refusal).
    Refused,
    /// Forward-compat fallback: a frontend `event` tag this build doesn't
    /// know about decodes to `Unknown` rather than failing the line — a
    /// parse error would drop the whole subscription and churn reconnects.
    /// The enclosing `frontend_event` stays non-terminal, and the dictation
    /// controller ignores `Unknown`. Mirrors [`Response::Unknown`].
    #[serde(other)]
    Unknown,
}

impl Response {
    /// Is this a terminal line (a one-shot client may stop reading)?
    /// `Listening`/`Partial` stream during test recognition, and
    /// `FrontendEvent` streams on a frontend subscription forever.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Response::Status { .. }
                | Response::Done
                | Response::Transcript { .. }
                | Response::ModelStatus { .. }
                | Response::Error { .. }
                | Response::Unknown
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests must serialize to the exact bytes the daemon parses — these
    /// fixtures are the cross-repo contract (plan Tasks B1/B4, DontSpeak
    /// `docs/ZED-FRONTEND.md`). Do not change them without changing the
    /// daemon in lockstep.
    #[test]
    fn request_fixtures_match_daemon_wire_shapes() {
        let cases: &[(Request, &str)] = &[
            (
                Request::SubscribeFrontend { app: "zed".into() },
                r#"{"cmd":"subscribe_frontend","app":"zed"}"#,
            ),
            (
                Request::AckDeliver { seq: 4, ok: true },
                r#"{"cmd":"ack_deliver","seq":4,"ok":true}"#,
            ),
            (
                Request::NarrateBatch {
                    session: "sess-1".into(),
                    key: "sess-1#0#3".into(),
                    text: "> Done.".into(),
                    is_final: false,
                },
                r#"{"cmd":"narrate_batch","session":"sess-1","key":"sess-1#0#3","text":"> Done.","is_final":false}"#,
            ),
            (
                Request::MarkActive {
                    session: Some("sess-1".into()),
                    synthetic: false,
                },
                r#"{"cmd":"mark_active","session":"sess-1","synthetic":false}"#,
            ),
            (
                Request::SessionEnd {
                    session: Some("sess-1".into()),
                },
                r#"{"cmd":"session_end","session":"sess-1"}"#,
            ),
            (
                Request::StopSpeech { session: None },
                r#"{"cmd":"stop_speech"}"#,
            ),
            (
                Request::StopSpeech {
                    session: Some("sess-1".into()),
                },
                r#"{"cmd":"stop_speech","session":"sess-1"}"#,
            ),
            (Request::Status, r#"{"cmd":"status"}"#),
            (Request::ModelStatus, r#"{"cmd":"model_status"}"#),
            (
                Request::TestRecognitionStart,
                r#"{"cmd":"test_recognition_start"}"#,
            ),
            (
                Request::TestRecognitionStop,
                r#"{"cmd":"test_recognition_stop"}"#,
            ),
        ];
        for (req, expected) in cases {
            let line = serde_json::to_string(req).unwrap();
            assert_eq!(&line, expected);
            assert!(!line.contains('\n'), "a request must be a single line");
            let back: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(&back, req);
        }
    }

    /// The daemon-emitted frontend-event lines from plan Task B1, verbatim.
    #[test]
    fn frontend_event_fixtures_parse() {
        let cases: &[(&str, FrontendEvent, u64)] = &[
            (
                r#"{"ok":"frontend_event","event":"recording_started","seq":1}"#,
                FrontendEvent::RecordingStarted,
                1,
            ),
            (
                r#"{"ok":"frontend_event","event":"partial","text":"hello wor","seq":2}"#,
                FrontendEvent::Partial {
                    text: "hello wor".into(),
                },
                2,
            ),
            (
                r#"{"ok":"frontend_event","event":"awaiting_confirm","text":"hello world","seq":3}"#,
                FrontendEvent::AwaitingConfirm {
                    text: "hello world".into(),
                },
                3,
            ),
            (
                r#"{"ok":"frontend_event","event":"deliver","text":"hello world","submit":true,"seq":4}"#,
                FrontendEvent::Deliver {
                    text: "hello world".into(),
                    submit: true,
                },
                4,
            ),
            (
                r#"{"ok":"frontend_event","event":"cancelled","seq":5}"#,
                FrontendEvent::Cancelled,
                5,
            ),
            (
                r#"{"ok":"frontend_event","event":"refused","seq":6}"#,
                FrontendEvent::Refused,
                6,
            ),
        ];
        for (line, expected_event, expected_seq) in cases {
            let resp: Response = serde_json::from_str(line).unwrap();
            match resp {
                Response::FrontendEvent { event, seq } => {
                    assert_eq!(&event, expected_event, "for line {line}");
                    assert_eq!(seq, *expected_seq, "for line {line}");
                }
                other => panic!("expected FrontendEvent for {line}, got {other:?}"),
            }
        }
    }

    #[test]
    fn response_fixtures_parse() {
        let status: Response = serde_json::from_str(
            r#"{"ok":"status","tts_active":true,"queued":2,"paused":false,"muted":false}"#,
        )
        .unwrap();
        assert_eq!(
            status,
            Response::Status {
                tts_active: true,
                queued: 2,
                paused: false,
                muted: false,
            }
        );

        let done: Response = serde_json::from_str(r#"{"ok":"done"}"#).unwrap();
        assert_eq!(done, Response::Done);

        let err: Response = serde_json::from_str(r#"{"ok":"error","message":"nope"}"#).unwrap();
        assert_eq!(
            err,
            Response::Error {
                message: "nope".into()
            }
        );

        let model_status: Response =
            serde_json::from_str(r#"{"ok":"model_status","status":{"running":{"caps":true}}}"#)
                .unwrap();
        match model_status {
            Response::ModelStatus { status } => {
                assert_eq!(status["running"]["caps"], Value::Bool(true));
            }
            other => panic!("expected ModelStatus, got {other:?}"),
        }
    }

    /// Version-skew guard, mirroring the daemon-side contract: an `ok` tag
    /// this build doesn't know about must decode to `Unknown` (terminal),
    /// not hard-error.
    #[test]
    fn unrecognized_response_tag_falls_back_to_unknown() {
        let resp: Response =
            serde_json::from_str(r#"{"ok":"some_future_variant","extra":"field","n":42}"#).unwrap();
        assert_eq!(resp, Response::Unknown);
        assert!(resp.is_terminal());
    }

    /// Version-skew guard for the flattened frontend-event tag: an unknown
    /// `event` must decode to `FrontendEvent::Unknown` inside a still-
    /// non-terminal `frontend_event`, not fail the line (which would drop
    /// the live subscription and churn reconnects).
    #[test]
    fn unrecognized_frontend_event_falls_back_to_unknown() {
        let resp: Response = serde_json::from_str(
            r#"{"ok":"frontend_event","event":"some_future_event","text":"x","seq":9}"#,
        )
        .unwrap();
        assert_eq!(
            resp,
            Response::FrontendEvent {
                event: FrontendEvent::Unknown,
                seq: 9,
            }
        );
        assert!(!resp.is_terminal());
    }

    #[test]
    fn terminal_classification() {
        assert!(Response::Done.is_terminal());
        assert!(
            Response::Error {
                message: "x".into()
            }
            .is_terminal()
        );
        assert!(!Response::Listening.is_terminal());
        assert!(!Response::Partial { text: "x".into() }.is_terminal());
        assert!(
            !Response::FrontendEvent {
                event: FrontendEvent::RecordingStarted,
                seq: 1,
            }
            .is_terminal()
        );
    }
}
