# DontSpeak ⇄ Zed Native Integration Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make DontSpeak's speech-to-text and text-to-speech first-class, native features of Zed — CapsLock-triggered dictation whose recognized text appears inline in whatever Zed input is focused (agent panel message editor, any terminal, any editor), spoken narration of agent replies (quoted `>` digest lines + mid-turn updates) for agents running either in Zed's native agent panel or in terminals inside Zed, and DontSpeak status/configuration surfaced inside Zed's own settings UI — while leaving all of DontSpeak's existing gesture/STT/TTS/narration logic unchanged.

**Architecture:** DontSpeak's daemon (`dontspeakd`, hosted in the DontSpeak tray app) keeps sole ownership of the physical CapsLock key, the gesture state machine, microphone capture, STT engines, the TTS queue, and the blockquote-narration logic. Zed becomes a *native frontend client* of the daemon over DontSpeak's existing local NDJSON socket (`state_dir/dontspeak.sock`, AF_UNIX on all three OSes). Zed renders dictation state natively (IME-style marked text in the focused input + a status-bar button) instead of DontSpeak's floating overlay, receives final transcripts over an acknowledged socket delivery instead of clipboard-paste, and feeds agent-panel message streams back to the daemon for narration. Zed-side packaging follows the Copilot pattern (core crate + `_ui` crate, `Status` projection, settings-observer lifecycle, self-hiding status item, settings-UI page). A separate, independent feature adds multiple concurrent agent views to Zed by hosting `ConversationView` as a center-pane workspace `Item`.

**Tech Stack:** Rust both sides. Zed: GPUI, `smol`, in-tree `net` crate (AF_UNIX incl. Windows). DontSpeak: existing `ds-ipc` NDJSON protocol (refactored server contract, see B1), `ds-narrate`/`ds-config` narration library, `dontspeakd` engine.

**Repos & pinned refs (line numbers reference these commits; both repos move fast — re-verify on drift):**
- Zed fork: `yanchenko/zed`, branch `dontspeak-integration`, base `2c4e44704c37` (2026-07-09).
- DontSpeak: `delllusional/DontSpeak` @ `98573c0` (2026-07-09); changes land there on a feature branch, e.g. `zed-frontend`. **Process note:** DontSpeak's CLAUDE.md treats `ds-ipc` protocol changes as risk-listed (its plan-review-implement workflow requires the `ds-risk-auditor` stage) and its AGENTS.md requires working in a worktree — follow that repo's process for Part B.

## Global Constraints

- **No cross-version compatibility burden in DontSpeak.** The daemon, tray apps, MCP binary, and hooks all ship from one repo in lockstep — refactor properly rather than adding compat shims. In particular: the `ds-ipc` `Handler` contract gets a real redesign (B1), and the ds-status wire-contract test plus the two tray-app DTO mirrors (`apps/windows/winui/Native.cs`, `apps/macos/Sources/DontSpeak/DontSpeakCore.swift` — per DontSpeak AGENTS.md) are simply updated in the same change when the wire shape moves. The only externally-frozen surfaces are the agent-facing ones (hook JSON contract with Claude Code/Qwen/Codex, MCP tool schemas) — this plan does not touch them.
- DontSpeak gains a **config kill-switch**: `frontend_enabled: bool` (default `true`) in `VoiceConfig`; when false the daemon rejects `SubscribeFrontend` — a misbehaving frontend can be shut off without quitting Zed (precedent: `codex_stream` gate, `rust/crates/ds-config/src/voice.rs:307-323`).
- All Zed changes are **feature-gated behind settings** (`"dontspeak": { "enabled": ... }`) and degrade silently when the daemon socket is absent — Zed must behave exactly like upstream when DontSpeak is not installed/running.
- No CapsLock keybinding in Zed. GPUI cannot bind CapsLock (no capslock modifier in `Modifiers`, `crates/gpui/src/platform/keystroke.rs:448-471`); DontSpeak already owns the key globally and suppresses the OS toggle. Zed only *reacts* to daemon state.
- Existing DontSpeak *behaviors* (gesture semantics; deferred-confirm flow; `double_tap_submits`; narration gates; mic-active suppression; per-session voice pool; **always-listening mode**) must not change for non-Zed targets — and always-listening delivery stays on the classic paste path in v1 (see B3). When Zed is not frontmost or not subscribed, behavior is today's behavior (floating overlay + clipboard paste — Zed is already whitelisted: `CUSTOM_TEXT_BUNDLES = ["dev.zed.Zed", "dev.zed.Zed-Preview"]` at `rust/crates/ds-platform/src/macos.rs:323`, `CUSTOM_TEXT_EXES = ["zed.exe"]` at `rust/crates/ds-platform/src/windows.rs:85`).
- Zed repo conventions (`.rules`): no `mod.rs`, explicit `[lib] path = "src/<crate>.rs"`, new crates registered in root `Cargo.toml` under both `[workspace].members` and `[workspace.dependencies]`, `./script/clippy` before commit. Zed's feature checklist (`docs/src/development/feature-process.md:36-51`) requires settings-UI coverage for any new setting — Task C5 satisfies it.

---

# Part A — Design

## A.1 Who does what (division of labor)

| Concern | Owner | Status |
|---|---|---|
| CapsLock ownership, gesture state machine (tap/double-tap/long-press), LED | DontSpeak `dontspeakd` engine (`rust/crates/dontspeakd/src/engine.rs`) | **Reused unchanged** |
| Mic capture, STT engines (FastConformer ONNX / CoreML / system), partials | DontSpeak warm helper (`ds-helper --serve`) | **Reused unchanged** |
| TTS queue, voices, barge-in, per-session routing, earcons | DontSpeak `ttsq` + `TtsManager` | **Reused unchanged** |
| Blockquote-digest narration logic (`>` lines + shorts) | `ds_config::all_blockquotes_state` + `ds_narrate` | **Reused unchanged** (invoked from a new IPC verb) |
| Narration for CLI agents in Zed terminals (Claude Code, Qwen Code, Codex) | DontSpeak hooks (`MessageDisplay`/`Stop`/`UserPromptSubmit`) | **Reused unchanged** — hooks fire regardless of which terminal app hosts the CLI |
| Dictation UI when Zed is frontmost | **Zed** (new): IME-style marked text in focused input + status-bar button | New |
| Final transcript delivery into Zed | **Daemon → Zed over socket** with ack; paste fallback on nack/timeout | New |
| Narration of Zed agent-panel (ACP) threads | **Zed** extracts message text, daemon narrates via new `NarrateBatch` verb | New |
| DontSpeak status/config UI inside Zed | **Zed** settings-UI "Voice" page + status-bar popover | New |
| Multiple agent views | **Zed** `agent_ui` (independent feature) | New |

## A.2 STT data flow (CapsLock dictation into Zed)

```
CapsLock tap ─► dontspeakd gesture engine (unchanged)
                 │ start_recording() → helper mic + STT (unchanged)
                 │ PARTIAL stream → PasteBuf.partial (unchanged)
                 ▼
        NEW: dictation events derived from the engine's existing
             status-transition site (publish_status_change) and written
             SYNCHRONOUSLY to the subscriber's socket (observed errors)
                 ▼
Zed DictationController (new crate `dontspeak_ui`)
   recording_started → show status-bar state; mark empty text in focused input
   partial(text)     → set marked text in focused input        [IME-style]
   awaiting_confirm  → keep marked text
   deliver(text, submit, seq) → clear mark; insert text; if submit: dispatch
                                "enter"; reply ack{seq, ok} on the same socket
   cancelled/refused → clear mark, reset indicator
```

Key properties:
- Marked text is exactly the mechanism IMEs use, and **both** targets already implement it: `Editor` via `EntityInputHandler` (`crates/editor/src/input.rs:2742`) and terminals via `TerminalInputHandler` (`crates/terminal_view/src/terminal_element.rs:1511`) whose marked text is a local overlay never sent to the PTY until commit (`crates/terminal_view/src/terminal_view.rs:372-402`, commit path `terminal_element.rs:1551-1561`). One generic path covers the agent panel's `MessageEditor` (an `Editor`: `crates/agent_ui/src/message_editor.rs:476`), every terminal, every buffer, and every `ui_input` field.
- "Which input is focused" is answered by GPUI: the platform window holds exactly one `PlatformInputHandler` — the one registered by the focused text input during paint (`crates/gpui/src/window.rs:2682-2690`, gate `:4359`). The handler stays present even when the window is not OS-active (e.g. `gpui_windows` holds it in a `Cell`, cleared only by re-registration).
- **Marked-target tracking:** the controller records which window it marked; before retargeting (focus/window change between events) it clears the old mark first. Without this, marked text — which in an `Editor` is real buffer text + highlight — would be orphaned on focus moves (`Editor::handle_blur` does not clear composition, `crates/editor/src/editor.rs:10430-10452`; `TerminalView::focus_out` does not clear `ime_state`, `terminal_view.rs:1311-1318`).
- **Acknowledged delivery:** the daemon treats a `deliver` as successful only after Zed's `ack{seq, ok:true}` (bounded wait ~300 ms). Zed replies `ok:false` when it cannot insert (no active window; no input handler; input rejects text). On nack/timeout the daemon falls back to the classic clipboard-paste path — an utterance is never lost. (Known limitation: a Vim-normal-mode editor passes the ack check but drops text via `InputIgnored` (`crates/editor/src/input.rs:2809-2812`) because `input_enabled` and `accepts_text_input`'s `expects_character_input` are set independently by vim (`crates/vim/src/vim.rs:2269-2270`); documented, acceptable v1 — dictating into a normal-mode vim buffer is out of scope.)
- The daemon's gesture flow is unchanged; only the *delivery edge* inside `confirm_paste()` (`rust/crates/dontspeakd/src/engine.rs:680`) branches. The deliver branch must also perform the submit bookkeeping the Enter path does today — `cancel_for_submit` + `note_voice_submit` — so the subsequent `UserPromptSubmit → MarkActive` is deduplicated as the same voice submit (`rust/crates/dontspeakd/src/ipc.rs:19-84`).
- **Scope: PTT (CapsLock) dictations only.** Always-listening mode (`listener.rs` submits via its own `type_text` at `rust/crates/dontspeakd/src/listener.rs:273`) keeps classic overlay + paste in v1; frontend ownership is decided per-dictation at confirm time, and the overlay-suppression state is only emitted for frontend-owned PTT dictations.

## A.3 TTS data flow (narration of agent replies)

Two sources, two paths — both ending in the same unchanged TTS queue:

**(1) CLI agents in Zed terminals — zero new code.** Claude Code / Qwen Code / Codex run DontSpeak's hooks themselves (`MessageDisplay`/`Stop` → `dontspeak notify` → `Request::SpeakNarration`), independent of which terminal hosts them. Works in Zed terminals today. The only fix: DontSpeak's focus gates (`pause_in_background`) must treat Zed as terminal-like — via a **table split**, not a blanket `KNOWN_TERMINALS` row (see B5; a blanket row would let `ClaudeNative` inject the push-to-talk chord into Zed buffers, `rust/crates/ds-stt/src/claude_native.rs:64-70`).

**(2) Agent-panel (ACP) threads — new bridge.** Zed's `AcpThread` emits `NewEntry` / `EntryUpdated(usize)` / `EntriesRemoved(Range<usize>)` / `Stopped` / `Error` / `Refusal` (`crates/acp_thread/src/acp_thread.rs:2148-2171`), with per-message text public (`entries()` `:2386`, `session_id()` `:2419`, chunks `:320-348`, `to_markdown` `:1704`). A per-thread `ThreadNarrator` forwards each assistant message's cumulative text:

```
AcpThreadEvent::EntryUpdated ─► ThreadNarrator (new, debounced ~300 ms)
   entry = AssistantMessage → cumulative text of Message chunks (skip Thought)
   ▼
Request::NarrateBatch { session: <acp session id>,
                        key: "<session>#<generation>#<entry-ix>",
                        text: <cumulative>, is_final: false }        (NEW verb)
   ▼
dontspeakd handler → ds_narrate::narrate_batch(...)  [same fn + DisplayState
   file-lock dedup the hooks use — concurrent callers safe by design]
   → per utterance: ttsq.enqueue(text, None, None, session)  [same as the
     SpeakNarration arm, ipc.rs:158-163 → session voice pool applies]
Stopped / Error / Refusal ─► NarrateBatch { ..., is_final: true }
EntriesRemoved ─► bump <generation>  [truncation reuses entry indices:
   acp_thread.rs:3848-3851, 4013-4015 — generation keeps keys unique]
```

- Mic gating happens daemon-side using the **system-wide mic probe** (`MicState`, as the daemon's own codex_stream narrator does at `rust/crates/dontspeakd/src/codex_stream/mod.rs:808`) — not the engine's PTT-only `stt_active`.
- **Double-narration guard:** Claude Code under `claude-acp` may still execute settings hooks. Zed's `narrate_panel_agents` setting defaults to `auto` = narrate only agents **without** DontSpeak hook wiring (Zed native agent, Gemini, …); hook-wired agent ids default off pending verification V2.3.
- **Narration spec injection:** hook-wired agents get the spec from the existing `provide` hook (unchanged, incl. its dynamic `MUTED_NOTICE`). For panel-narrated agents Zed injects the spec as a **request-only content block** — split "display blocks" from "request blocks" at the `AcpThread::send` seam (a leading block added via `MessageEditor::contents` would render inside the user's visible message bubble and persist in history: `crates/acp_thread/src/acp_thread.rs:3624-3679`). Spec text is read from DontSpeak's `config_dir/narration-spec.md` — **config_dir, not state_dir** (they differ on Windows `%APPDATA%` vs `%LOCALAPPDATA%` and Linux `~/.config` vs `~/.local/state`; `rust/crates/ds-config/src/paths.rs:117,190-192,334-350`) — falling back to an embedded copy of `DEFAULT_NARRATION_SPEC`.
- **Session lifecycle mirroring:** prompt send → `MarkActive { session }`; thread close (`ConnectedServerState::close_all_sessions`, invoked from `ConversationView`'s release: `crates/agent_ui/src/conversation_view.rs:791-804,852-856`) → `SessionEnd { session }`. Both verbs exist (`rust/crates/ds-ipc/src/protocol.rs:41-57,99`).

## A.4 Zed-side packaging (Copilot pattern)

- Two crates: `crates/dontspeak` (core: client, protocol, settings, `Status`, actions — no GPUI views) + `crates/dontspeak_ui` (status-bar button, `DictationController`, settings-page components). Mirrors `copilot`/`copilot_ui`.
- Core entity: `GlobalDontSpeak(Entity<DontSpeak>)` global; internal `enum DaemonConnection { Disabled, Connecting, Connected, Error(Arc<str>), Absent }` projected to public `Status`; `EventEmitter<Event>`; `cx.observe_global::<SettingsStore>` connects/disconnects on setting flips (model: `crates/copilot/src/copilot.rs:66-145,265-306,394-427`).
- Actions: `actions!(dontspeak, [ToggleDictation, StopSpeech, OpenVoiceSettings, Reconnect])` + `CommandPaletteFilter` visibility tied to status (model: `copilot.rs:1299-1335`).
- Status-bar button modeled on **EditPredictionButton** (self-hiding when disabled/absent, state icon family, popover menu, error → toast with recovery action: `crates/edit_prediction_ui/src/edit_prediction_button.rs:77-138,642-670`).
- Settings-UI **"Voice" page** replaces DontSpeak's tray/status window inside Zed: daemon status card (model: MCP servers page, `crates/settings_ui/src/pages/mcp_servers_page.rs:159-248,373-503`), model-readiness rows (via existing `ModelStatus` verb), daemon-fed voice picker (model: `crates/settings_ui/src/components/ollama_model_picker.rs:17-60`), test-recognition button (via existing `TestRecognitionStart` verb), "not installed" card with install link (model: featured-agent install buttons, `crates/onboarding/src/basics_page.rs:531-591`).
- No feature_flags (staff/server-oriented; debug builds auto-enable staff flags). Settings gate + cargo feature only.

## A.5 Rejected alternatives

- **Zed binds CapsLock** — impossible + fights DontSpeak's global ownership (Windows `WH_KEYBOARD_LL` suppression `rust/crates/ds-platform/src/windows.rs:176-208`; macOS `hidutil` remap; Linux XKB `caps:none`). Rejected.
- **Zed links DontSpeak engines in-process (ds-core FFI)** — duplicates warm model residency, fights the single-speaker pidfile contract, loses cross-app behavior. Rejected.
- **Zed drives STT via `TestRecognitionStart`** — works, but CapsLock gestures wouldn't control it. Used only for the settings-page test button.
- **MCP as the frontend channel** — request/response only; the frontend needs server-push. Rejected (MCP server stays for agents).
- **Extension instead of fork** — no UI/audio/keybinding/focus/text-insertion in the extension surface (`crates/extension_host/src/wasm_host.rs:550-804`). Fork confirmed.
- **Multiple `AgentPanel` instances** — fights ~123 type-keyed `panel::<AgentPanel>()` call sites + per-type persistence (`crates/workspace/src/dock.rs:484`, `workspace.rs:2546-2553`). Rejected in favor of ConversationView-as-Item (Part D).

---

# Part B — What changes in DontSpeak

Branch `zed-frontend` in `delllusional/DontSpeak`; follow that repo's plan-review-implement + risk-audit process (ds-ipc changes are risk-listed). Estimated ~900 LOC + tests.

### Task B1: `ds-ipc` server refactor + frontend subscription verb + registry with acknowledged delivery

**Files:**
- Modify: `rust/crates/ds-ipc/src/server.rs` (**Handler contract redesign**)
- Modify: `rust/crates/ds-ipc/src/protocol.rs` (Request/Response variants)
- Modify: `rust/crates/dontspeakd/src/ipc.rs` (all handler arms mechanical-update + `FrontendRegistry`)
- Modify: `rust/crates/dontspeakd/src/stt_test.rs` (benefits from the refactor — streamer can now observe disconnects)
- Modify: `rust/crates/ds-config/src/voice.rs` (`frontend_enabled: bool`, default true)
- Test: serde round-trips in `protocol.rs`; server contract tests in `ds-ipc`; registry tests in `dontspeakd`

**Why the server contract must be redesigned (no compat shims — per Global Constraints):** today's `Handler` is `fn handle(&self, req, emit: &mut dyn FnMut(&Response))` and the emit closure discards write failures (`server.rs:119-122` `let _ = write_line(...)`). A handler can neither stream from an external event source nor observe client disconnects — the existing `TestRecognitionStart` streamer only terminates via an external stop on a *second* connection, and a naïve blocking subscribe handler would leak its thread + `active_conns` slot on every Zed reconnect until `MAX_CONNECTIONS = 64` (`server.rs:35`) bricks all daemon IPC.

**Refactor:** replace the emit closure with a connection object owned by the handler call:

```rust
pub struct Conn { /* stream halves, peer info */ }
impl Conn {
    pub fn send(&mut self, resp: &Response) -> io::Result<()>;      // observable writes
    pub fn recv_deadline(&mut self, d: Duration) -> io::Result<Option<Request>>; // for acks
    pub fn into_stream(self) -> Stream;                              // takeover for long-lived subscriptions
}
pub trait Handler {
    fn handle(&self, req: Request, conn: Conn) -> HandleOutcome;    // Done | TookOver
}
```

All existing arms update mechanically (`conn.send(&resp)?` instead of `emit(&resp)`); `handle_conn` keeps the read loop for `Done` outcomes and releases its `active_conns` slot immediately on `TookOver` (taken-over connections are bounded separately: registering a frontend evicts and closes any previous subscriber with the same app tag). As a side benefit, `stt_test.rs` streaming can now abort on client disconnect instead of leaking until an external stop.

**Interfaces:**
- Wire (client→daemon, persistent connection):
  ```json
  {"cmd":"subscribe_frontend","app":"zed"}
  {"cmd":"ack_deliver","seq":4,"ok":true}
  ```
- Wire (daemon→client, streamed on that connection):
  ```json
  {"ok":"frontend_event","event":"recording_started","seq":1}
  {"ok":"frontend_event","event":"partial","text":"hello wor","seq":2}
  {"ok":"frontend_event","event":"awaiting_confirm","text":"hello world","seq":3}
  {"ok":"frontend_event","event":"deliver","text":"hello world","submit":true,"seq":4}
  {"ok":"frontend_event","event":"cancelled","seq":5}
  {"ok":"frontend_event","event":"refused","seq":6}
  ```
- Registry: `FrontendRegistry` owning per-subscriber `{ app: String, conn: Mutex<Conn> }`; `broadcast(event)` (drop subscriber on write error) and `deliver_to_frontmost(text, submit) -> DeliverOutcome` — synchronous write, then `recv_deadline(~300ms)` for the `ack_deliver`; `DeliverOutcome::{Delivered, Failed}`; `Failed` (write error, nack, timeout) → caller falls back to paste, subscriber dropped.

- [ ] **Step 1:** Protocol variants + serde round-trip tests (`SubscribeFrontend`, `AckDeliver`; `FrontendEvent` non-terminal in `is_terminal()`). All wire keys explicit via `#[serde(rename)]` where field names differ, so C1's fixture tests match byte-for-byte.
- [ ] **Step 2:** Server refactor (`Conn`, `HandleOutcome`, mechanical arm updates, takeover slot accounting). Contract tests: takeover releases the connection slot; same-app resubscribe evicts the old subscriber; write-error surfaces to the handler.
- [ ] **Step 3:** `FrontendRegistry` + tests: sync write failure → drop; no subscriber → `Failed`; ack round-trip; nack → `Failed`; timeout → `Failed`.
- [ ] **Step 4:** `frontend_enabled` config gate (reject subscribe when false) + activity-log lines for subscribe/evict/deliver-fallback (`ds-log` source, repo convention). Update `stt_test.rs` to abort on disconnect.
- [ ] **Step 5:** Run the full DontSpeak test suite (the refactor touches every IPC consumer); commit.

### Task B2: Frontmost-app matching for frontends

**Files:**
- Modify: `rust/crates/ds-platform/src/lib.rs` (trait method), `macos.rs`, `windows.rs`, `linux.rs`
- Test: pure matcher unit tests

**Interfaces:**
- Produces: `fn is_app_frontmost(&self, app_tag: &str) -> bool` on `FrontmostWindow`; `"zed"` maps to the existing identity tables (macOS bundles `dev.zed.Zed`/`dev.zed.Zed-Preview`; Windows exe `zed.exe`; Linux wm_class `dev.zed.Zed`; Wayland fail-open matching `is_terminal_frontmost`, `rust/crates/ds-platform/src/linux.rs:366-372`).

- [ ] **Step 1:** Trait method (default `false`) + per-OS impls reusing each platform's frontmost lookup; unit-test the pure tag→identity matching.
- [ ] **Step 2:** Commit.

### Task B3: Dictation events from the status-transition site; overlay & paste suppression for frontend-owned PTT dictations

**Files:**
- Modify: `rust/crates/dontspeakd/src/engine.rs` (`publish_status_change` ~`:519-551`; `confirm_paste` ~`:680`; refusal arm ~`:856`)
- Modify: `rust/crates/dontspeakd/src/status.rs` (state-token override when frontend-owned)
- Test: engine tests with the existing MockPlatform (`type_text_calls`/`press_enter_calls` counters) + a scripted registry

**Design (fixes from review):**
- **Single emission site:** derive frontend events from the same state-digest transition that wakes the overlay — `publish_status_change` on the 30 ms poll thread (~≤33 Hz), *not* six hand-placed emits. This automatically covers every dictation-ending path the hand-placed version would miss: empty-final disarm (`FinalState::Empty` → `PasteBuf::disarm`) and `teardown_hold` both surface as a transition to `hidden` → emit `cancelled`.
- **Overlay suppression via the canonical token:** while a dictation is frontend-owned, the engine reports `dictation.state = "hidden"` in `model_status` — the token all three tray hosts already switch on (`status.rs:608-613`, `DictationPanel.swift:322-329`, `App.xaml.cs:363-367`, `overlay.rs:119-131`). This keeps visibility "decided once in the engine" (the repo's own design rule) with zero per-host derivation. If the Zed settings page later wants richer daemon-side state, extending the `Dictation` struct is fine — just update the DTO mirrors + wire-contract test in the same change (no compat constraint).
- **Ownership decided per dictation:** at `start_recording` (PTT path only), if a live subscriber's app is frontmost (`B2`), tag the in-flight dictation frontend-owned; the tag drives both the token override and the confirm-time branch. Always-listening submissions (`listener.rs:273`) are untagged → classic behavior.
- **Deliver branch parity:** in `confirm_paste`, when frontend-owned: `registry.deliver_to_frontmost(text, submit)`; on `Delivered` skip `type_text`/`press_enter` **but still run `cancel_for_submit` + `note_voice_submit` when submitting** (voice-submit echo dedup, `ipc.rs:19-84`); on `Failed` fall through to the unchanged paste path (and clear the frontend-owned tag so the overlay token reverts).

- [ ] **Step 1:** Failing engine tests: (a) subscribed + Zed frontmost + PTT cycle → `deliver` outcome consumed, zero `type_text` calls, `note_voice_submit` recorded when submitting; (b) no subscriber → `type_text` exactly as today; (c) deliver `Failed` → paste fallback fires; (d) empty-final and long-press-cancel paths each produce a `cancelled` event; (e) always-listening submit → no frontend events, overlay token normal.
- [ ] **Step 2:** Implement per the design above.
- [ ] **Step 3:** Run engine suite; commit.

### Task B4: `NarrateBatch` verb

**Files:**
- Modify: `rust/crates/ds-ipc/src/protocol.rs` (Request variant), `rust/crates/dontspeakd/src/ipc.rs` (handler), `rust/crates/dontspeakd/src/boot.rs` (pass `mic_watcher.handle()` into `spawn_ipc_server`, as `spawn_supervisor` already receives it, `boot.rs:273-279`)
- Test: handler test with fake queue

**Interfaces:**
- Wire: `{"cmd":"narrate_batch","session":"<id>","key":"<key>","text":"<cumulative>","is_final":false}` → `{"ok":"done"}`.
- Handler: build `StreamBatch { key, payload: Cumulative(text), is_final }` → `ds_narrate::narrate_batch(paths, session, batch, mic_active, digests_on, shorts_on)` (`rust/crates/ds-narrate/src/stream.rs:192`; its `with_state_lock` file lock makes concurrent hook/daemon callers for one session safe) → per utterance `ttsq.enqueue(text, None, None, session)` (exactly the `SpeakNarration` arm, `ipc.rs:158-163`). `mic_active` from **`MicState::is_active()`** (system-wide probe — parity with hooks `hook_narrate.rs:142,258` and codex_stream `mod.rs:808`), *not* `stt_active`.

- [ ] **Step 1:** Failing test: growing cumulative text with one complete blockquote across two calls → exactly one enqueue; `is_final:true` shorts fallback; mic-active → suppressed.
- [ ] **Step 2:** Implement (incl. `MicState` plumbing into the IPC handler shared state); activity-log line per narrate-batch session start.
- [ ] **Step 3:** Commit. (In-daemon precedent for the whole shape: `codex_stream/mod.rs:797-823`.)

### Task B5: Zed in the focus-gate terminal table (split from key-injection) + protocol docs

**Files:**
- Modify: `rust/crates/ds-platform/src/lib.rs` (`KnownTerminal` gains `inject_keys: bool` — existing rows true, Zed row false), `ds-stt/src/claude_native.rs` call-site gate
- Create: `docs/ZED-FRONTEND.md` (subscriber protocol contract: verbs, events, ack, fallback semantics — the JSON in B1 verbatim)

- [ ] **Step 1:** Add Zed row (windows_exe `zed.exe`, macos bundles `dev.zed.Zed`/`-Preview`, linux wm_class `dev.zed.Zed`) with `inject_keys: false`; `ClaudeNative`'s `is_terminal_frontmost` gate (`claude_native.rs:64-70`) consults only inject-eligible rows — otherwise a Caps tap with Zed frontmost and `stt_engine=claude_code` would inject the push-to-talk chord into a Zed buffer. `pause_in_background`/`set_terminal_front` (`ttsq.rs:1069-1077`) and `has_paste_target` short-circuits keep using the full table. Document the `terminal_seen` latch implication (once Zed is seen, the focus gate arms) in the config docs.
- [ ] **Step 2:** Tests for the split gate; write `docs/ZED-FRONTEND.md`; commit.

---

# Part C — What changes in Zed

Branch `dontspeak-integration` in `yanchenko/zed`. Estimated ~2,000 LOC + tests.

### Task C1: `dontspeak` core crate — client, protocol, settings, status

**Files:**
- Create: `crates/dontspeak/Cargo.toml` (`[lib] path = "src/dontspeak.rs"`), `src/dontspeak.rs` (global, `Status`, actions, `init`), `src/protocol.rs`, `src/client.rs`, `src/dontspeak_settings.rs`
- Modify: root `Cargo.toml` (**both** `[workspace].members` and `[workspace.dependencies]`)
- Modify: `crates/settings_content/src/settings_content.rs` (+ new content module) — `pub dontspeak: Option<DontSpeakSettingsContent>` field (alphabetical list at `:116-266`; derive set per `AudioSettingsContent` `:468-478`)
- Modify: `assets/settings/default.json` (defaults + doc comments)
- Modify: `crates/zed/src/main.rs` (`dontspeak::init(cx)` in the init region `:589-788`)
- Test: protocol fixture tests (JSON strings copied verbatim from B1 — cross-repo contract), client tests against an in-process fake server

**Interfaces:**
- `protocol.rs`: serde mirror of the wire shapes Zed uses (`SubscribeFrontend`, `AckDeliver`, `NarrateBatch`, `MarkActive`, `SessionEnd`, `StopSpeech`, `Status`, `ModelStatus`, `TestRecognitionStart/Stop`, `FrontendEvent`). Mirrored, not a git dep; header comment pins source file + DontSpeak commit.
- `client.rs`: `socket_path()` (per-OS `state_dir`: Windows `%LOCALAPPDATA%\DontSpeak\dontspeak.sock`, macOS `~/Library/Application Support/DontSpeak/dontspeak.sock`, Linux `$XDG_STATE_HOME/dontspeak/dontspeak.sock` — mirrors `paths.rs:334-350`) **and** `config_dir()` (Windows `%APPDATA%\DontSpeak`, macOS same as state dir, Linux `$XDG_CONFIG_HOME/dontspeak` — for `narration-spec.md`); `request(req) -> Result<Response>` one-shot; `subscribe_frontend(...)` persistent duplex connection (reads events, writes acks) with auto-reconnect/backoff via `net::async_net::UnixStream` (`crates/net/src/net.rs:1-18`).
- `dontspeak.rs`: `GlobalDontSpeak(Entity<DontSpeak>)`; `DaemonConnection` enum → public `Status`; `EventEmitter<Event>`; settings observer start/stop; `actions!(dontspeak, [ToggleDictation, StopSpeech, OpenVoiceSettings, Reconnect])`; `CommandPaletteFilter` visibility sync; `ZED_FORCE_DONTSPEAK_ERROR` test hook (copilot precedent `copilot.rs:715-720`).
- `dontspeak_settings.rs`: `#[derive(RegisterSetting)] DontSpeakSettings { enabled: bool, narrate_panel_agents: NarratePanelAgents, status_bar_button: bool }` (`RegisterSetting` = inventory-based auto-registration, `crates/settings_macros/src/settings_macros.rs:85-91`); `NarratePanelAgents { Auto, All, None }` derives `strum::VariantArray + VariantNames` so the settings-UI dropdown renders for free.

- [ ] **Step 1:** Crate skeleton + settings_content/default.json wiring; `cargo build -p dontspeak` green.
- [ ] **Step 2:** Protocol types + fixture tests (exact B1 JSON).
- [ ] **Step 3:** Failing client test: fake server → subscribe → three events → drop → reconnect to revived server; ack write path; `deliver` followed by client `ack`.
- [ ] **Step 4:** Global + status machine + actions + palette filter; init wiring; no-op when disabled/socket absent.
- [ ] **Step 5:** `cargo test -p dontspeak`; commit.

### Task C2: GPUI focused-input text APIs (marked text + insert)

**Files:**
- Modify: `crates/gpui/src/window.rs`
- Test: bespoke `InputHandler` test view against `TestPlatform` (test window supports set/take handler: `crates/gpui/src/platform/test/window.rs:174-178`) — gpui cannot depend on `editor`, and `crates/gpui/tests/` currently holds only `action_macros.rs`

**Interfaces (public, on `Window`):**
```rust
pub fn set_marked_text_in_focused_input(&mut self, text: &str, cx: &mut App) -> bool;
pub fn clear_marked_text_in_focused_input(&mut self, commit: bool, cx: &mut App) -> bool;
pub fn insert_text_into_focused_input(&mut self, text: &str, cx: &mut App) -> bool;
```
**Implementation note (from review):** must use the *inner-handler* call pattern — `take_input_handler` → call the handler's methods **with `(window, cx)`** → `set_input_handler` (exactly what `dispatch_keystroke`'s IME fallback does, `window.rs:4505-4510`, and `window.rs:5010-5015`). Do **not** call the outer `PlatformInputHandler::replace_and_mark_text_in_range` (`platform.rs:1236`) from inside `Window` — it re-enters via `self.cx.update` and would deadlock/panic while `&mut Window` is held. Gate on the inner `accepts_text_input` (`platform.rs:1338-1340`). All three return `false` when no handler is registered.

- [ ] **Step 1:** Failing tests with a scripted test `InputHandler`: mark → replace-mark → commit-clear → plain insert; all-false with nothing focused.
- [ ] **Step 2:** Implement; `cargo test -p gpui`.
- [ ] **Step 3:** Commit.

### Task C3: `dontspeak_ui` crate — DictationController + status-bar button

**Files:**
- Create: `crates/dontspeak_ui/Cargo.toml` (`[lib] path = "src/dontspeak_ui.rs"`), `src/dontspeak_ui.rs`, `src/dictation_controller.rs`, `src/status_button.rs`
- Modify: root `Cargo.toml` (members + workspace.dependencies), `crates/zed/Cargo.toml` (dep), `crates/zed/src/zed.rs` (status-bar registration in the block at `zed.rs:611-627`)
- Test: controller state-machine tests against a mocked window-ops trait

**Interfaces:**
- `DictationController`: consumes the C1 event receiver; **tracks the marked target** (window handle recorded at mark time; clears the old window's mark before retargeting on focus/window change; buffers a `deliver` when no window is active and nacks after deadline). Event handling: `recording_started`→indicator+empty mark; `partial`→marked text (retarget-safe); `awaiting_confirm`→keep; `deliver{text,submit,seq}`→clear-mark, `insert_text_into_focused_input`, on success + `submit` → `window.dispatch_keystroke(Keystroke::parse("enter"), cx)` (`window.rs:4491`; verified end-to-end: MessageEditor `agent::Chat` binding, terminal `terminal::SendKeystroke enter`, buffers `editor::Newline` — identical to a physical Enter), then `ack{seq, ok:<insert result>}`; `cancelled`/`refused`→clear mark. Unfocused-Zed behavior: events still update the status button; text ops no-op until a window is active.
- `status_button.rs`: `StatusItemView` modeled on **EditPredictionButton** (`edit_prediction_button.rs:77-138`): hidden when disabled or daemon absent (and when `status_bar_button:false`); states idle / recording (pulsing `IconName::Mic`) / awaiting-confirm / error (toast with "Open Voice Settings" action); popover: toggle dictation, stop speech, mute, Open Voice Settings (`zed::OpenSettingsPage { page: "Voice" }`, `crates/zed_actions/src/lib.rs:154-164`).

- [ ] **Step 1:** Failing controller tests: the sequences from A.2 incl. focus-moved-mid-dictation (old mark cleared, new target marked), deliver-with-no-window (nack), deliver-insert-false (nack), submit bookkeeping order (clear→insert→enter→ack).
- [ ] **Step 2:** Implement controller + button; register status item; `cargo test -p dontspeak_ui`.
- [ ] **Step 3:** Commit.

### Task C4: Agent-panel narration bridge (`ThreadNarrator`)

**Files:**
- Create: `crates/agent_ui/src/thread_narrator.rs`
- Modify: `crates/agent_ui/src/conversation_view.rs` (instantiate beside the thread subscription at `:1340`; prompt-send → `MarkActive`; release path → `SessionEnd`)
- Modify: `crates/acp_thread/src/acp_thread.rs` (**additive**: allow request-only content blocks on send — split display blocks from request blocks so the narration spec never renders in the user bubble, seam at `send_inner` `:3624-3679`)
- Modify: `crates/agent_ui/Cargo.toml` (optional dep + feature `dontspeak = ["dep:dontspeak"]`, pattern `audio` at `:25,40`), `crates/zed/Cargo.toml` (enable the feature on the `agent_ui` dependency line, pattern `:72`)
- Test: narrator tests with scripted thread fixtures + recording fake client

**Behavior (fixes from review):**
- Key: `"{session}#{generation}#{entry_ix}"`; `EntriesRemoved` bumps the generation (indices are reused after truncation: `acp_thread.rs:3848-3851,4013-4015`).
- Finalize on `Stopped` **and** `Error` **and** `Refusal` (error path never emits `Stopped`: `acp_thread.rs:3873` vs `:3885`).
- Debounce ~300 ms per entry; cumulative Message-chunk text only (skip `Thought` chunks); daemon-side `DisplayState` dedup makes resends harmless; `StreamingTextBuffer` lag only delays cumulative text and is flushed before `Stopped` (`:2859-2937,3786`).
- Gating: `narrate_panel_agents` (`Auto` skips hook-wired agent ids `claude-acp`/`codex-acp`/qwen customs).
- Spec injection via the new request-only block; spec text from `config_dir/narration-spec.md` (C1 `config_dir()`), embedded fallback.

- [ ] **Step 1:** Failing tests: one blockquote streamed → one non-final + one final `NarrateBatch`; truncate-then-new-message → distinct keys; error-terminated turn → final sent; thought-only → nothing; `Auto` gating.
- [ ] **Step 2:** Implement narrator + `acp_thread` request-only-block seam + lifecycle verbs.
- [ ] **Step 3:** `cargo test -p agent_ui -p acp_thread`; commit.

### Task C5: Settings-UI "Voice" page

**Files:**
- Create: `crates/settings_ui/src/pages/dontspeak_page.rs`
- Modify: `crates/settings_ui/src/page_data.rs` (new page entry in `settings_data()` `:65-83`), `crates/settings_ui/Cargo.toml` (deps on `dontspeak`/`dontspeak_ui` — precedent: it already deps `copilot_ui`)

**Content:**
- Section "General": `enabled` toggle + `narrate_panel_agents` dropdown + `status_bar_button` toggle — plain `SettingItem`s, widgets auto-derived from types.
- `SubPageLink { title: "DontSpeak", in_json: false }` → custom page: daemon status card (Status → status rows w/ inline error, model: `mcp_servers_page.rs:159-248,373-503`); STT/TTS model-readiness rows (daemon `ModelStatus` verb); voice picker fed by daemon voice list writing back via daemon `set_config` (model: `ollama_model_picker.rs:17-60`); "Test recognition" button driving `TestRecognitionStart/Stop` with partials shown inline; when the daemon is absent: "DontSpeak is not installed" card with an install link to dontspeak.org (model: featured-agent install buttons `basics_page.rs:531-591`).

- [ ] **Step 1:** Page + wiring; manual walkthrough with daemon running and stopped.
- [ ] **Step 2:** Commit. (Optional follow-up, non-blocking: activity-indicator branch for daemon model-download progress, template `activity_indicator.rs:609-638`.)

### Task C6: End-to-end verification (manual, scripted checklist)

**Files:** Create `docs/superpowers/plans/2026-07-09-dontspeak-zed-verification.md` (record results)

- [ ] **V1 (STT):** daemon (B1-B5) + Zed: CapsLock in agent-panel editor → status button pulses, **no floating overlay**, partials as marked text → confirm → text committed; submit variant sends the message. Repeat in terminal (IME overlay; commit types to PTY) and plain buffer. Focus-switch mid-dictation → no orphaned text.
- [ ] **V2 (fallbacks):** Zed absent → overlay + paste exactly as before. Zed running, other app frontmost → classic path. Kill Zed mid-recording → `deliver` times out → paste fallback, nothing lost. Vim normal mode → nack/limitation documented.
- [ ] **V2.3 (hooks under ACP):** claude-acp thread; check DontSpeak activity log for MessageDisplay/Stop hook narration. Record; set `Auto` defaults accordingly.
- [ ] **V3 (TTS, terminal):** Claude Code in Zed terminal → `>` digests spoken; `pause_in_background=true` + Zed frontmost → speaks (B5); non-Zed editor frontmost → held.
- [ ] **V4 (TTS, panel):** native-agent thread → digests spoken mid-turn + at stop; dictation while speaking → TTS pauses/resumes; error-terminated turn → final narration still fires.
- [ ] **V5 (sessions):** two panel threads → distinct voices; closing one → only its queue cleared; `stt_engine=claude_code` + Zed frontmost + Caps tap → **no** chord injected into Zed (B5 gate split).

---

# Part D — Multiple agent views in Zed (independent feature)

Verdict from research: the one-view limit is presentational. `AgentPanel` already retains N live `ConversationView`s (`retained_threads`, `crates/agent_ui/src/agent_panel.rs:1168`; re-parenting proven at `:4150-4178`), and one agent process already serves N concurrent ACP sessions (`AcpConnection.sessions`, `crates/agent_servers/src/acp.rs:393`; no prompt serialization `:1944-2010`; connections cached per agent key, `crates/agent_ui/src/agent_connection_store.rs:69-73,143-151`). Chosen: **(b) ConversationView as center-pane `Item`**; panel remains coordinator. Production model: `AgentDiffPane` (`crates/agent_ui/src/agent_diff.rs:41-130`, deploy+dedupe `:62-81`); in-tree proof a `ConversationView` renders fine in a pane: test-only `ThreadViewItem` (`conversation_view.rs:5670-5704`).

**Review-mandated correction:** thread-creation inputs are **not** all workspace-derivable — `AgentConnectionStore`, `ThreadStore`, `fs` are panel-owned (created at `agent_panel.rs:1527`, field `:1162`; used by `create_agent_thread_inner` `:4520-4548`). D1 therefore routes all creation through the panel (including its async-load path — pattern: `crates/sidebar/src/sidebar.rs:3676-3690`); a center item never builds its own store (which would spawn a duplicate agent process).

### Task D1: `ConversationItem` wrapper
- Create `crates/agent_ui/src/conversation_item.rs`: `Item` impl (`type Event = ()`, `tab_content_text` = thread title, `Focusable` → view, render = `WithRemSize` agent-font wrapper (`agent_panel.rs:6533`) + title strip (chrome the panel toolbar owns today, `agent_panel.rs:5325+`) + view; `clone_on_split = None` — a thread is hosted exactly once).
- [ ] Implement + `cargo test -p agent_ui`; commit.

### Task D2: `ConversationHost` abstraction
- `trait ConversationHost { fn is_view_visible(...) -> bool; fn reveal_thread(...); }` implemented by `AgentPanel` and the item host; migrate the three notification couplings (`conversation_view.rs:2816-2828`, `:2991-3009`, `:3064-3081`). Existing notification tests must pass unchanged.
- [ ] Implement; commit.

### Task D3: Open/move actions + uniqueness
- `agent::OpenThreadInCenter`: panel transfers the live `Entity<ConversationView>` into a `ConversationItem` via `workspace.add_item_to_center` with `items_of_type::<ConversationItem>` dedupe (`agent_diff.rs:62-81` pattern); panel drops it from `retained_threads`. `agent::MoveThreadToPanel` reverses. Invariant: a `ThreadId` lives in exactly one host.
- Route `MentionUri::Thread` (`thread_view.rs:12109-12113`) and sidebar activation (`sidebar.rs:3629-3690`) to center items first.
- [ ] Implement; manual check: two threads side-by-side generating concurrently (one process, two sessions); actions land in the focused view (`key_context("AcpThread")`, `thread_view.rs:11698`). Commit.
- Deferred, non-blocking: `SerializableItem` restore (model `terminal_view.rs:1842`); in-panel PaneGroup splits reusing `ConversationItem` (TerminalPanel pattern, `terminal_panel.rs:77-105`).

---

# Part E — Reused as-is (no changes)

**DontSpeak:** CapsLock ownership + gesture engine + LED; warm-helper STT stack incl. streaming partials; TTS queue (pause-for-dictation, barge-in, voice pool, earcons); narration logic (`all_blockquotes_state`, `Accum`, `narrate_batch`, `DisplayState` dedup, spec const + `provide`-hook injection incl. `MUTED_NOTICE`); hook wiring for CLI agents (covers Zed terminals with zero Zed code); MCP server; NDJSON socket transport + permission model; clipboard-paste fallback incl. existing Zed whitelisting.

**Zed:** GPUI focus system + platform-input-handler singleton; Editor/Terminal marked-text (IME) implementations; `net` AF_UNIX; `AcpThread` event stream + text accessors; Copilot packaging pattern (global/Status/palette-filter), EditPredictionButton status-item pattern, MCP-servers settings-page pattern, ollama voice-picker pattern; ACP multi-session multiplexing + `AgentConnectionStore`; `Item` trait + `AgentDiffPane`/`TerminalView` hosting precedents; `retained_threads` re-parenting.

# Execution order & milestones

- Part B: B1→B2→B3 sequential; B4, B5 independent of B3. Run under DontSpeak's own plan-review-implement + risk-audit process.
- Part C: C1 needs B1's wire shapes (fixtures); C2 independent; C3 needs C1+C2; C4 needs C1+B4 (+ its own `acp_thread` seam); C5 needs C1 (richer with B4).
- Part D: fully independent.
- **M1** = B1+B2+B3+C1+C2+C3 (native dictation) — independently shippable.
- **M2** = B4+B5+C4+C5 (panel narration + Voice settings page).
- **M3** = D1-D3 (multiple agent views).

# Open questions / risks

1. **Hooks under ACP (V2.3):** determines `Auto` narration defaults for `claude-acp`. Either outcome is handled (hook path with witness/session logic, or panel bridge).
2. **Wayland frontmost fail-open:** `deliver` may target Zed while another app is frontmost; mitigated by the ack protocol (no active Zed window → nack → paste fallback) — strictly better than today's Wayland behavior.
3. **Vim normal mode** drops synthetic insert despite ack-gating (flag mismatch documented in A.2) — v1 limitation; revisit with an editor-side `input_enabled` probe if it bites.
4. **Cross-repo protocol drift:** C1 fixtures pin exact JSON; `docs/ZED-FRONTEND.md` is the contract; both repos pin the counterpart commit. Consider extracting a shared protocol crate if drift bites.
5. **Repo velocity:** both trees moved during planning (DontSpeak `98573c0` landed mid-review shifting engine.rs by ~10 lines). Line refs are anchors, not gospel — re-locate by symbol name.
