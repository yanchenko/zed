//! UI side of Zed's native DontSpeak integration (Copilot's `copilot_ui`
//! pattern): the [`DictationController`] that renders daemon dictation
//! events as IME-style marked text in the focused input, and the
//! self-hiding status-bar button. The connection lifecycle, wire protocol,
//! and settings live in the `dontspeak` core crate.

mod dictation_controller;
mod status_button;

use std::rc::Rc;

use dontspeak::{DontSpeak, OpenVoiceSettings};
use gpui::{Action as _, App, AppContext as _, Entity, Global};
use workspace::Workspace;
use zed_actions::OpenSettingsPage;

pub use crate::dictation_controller::{DictationController, DictationPhase, DictationWindowOps};
pub use crate::status_button::DontSpeakStatusButton;

/// Wires the global [`DictationController`] to the [`DontSpeak`] global and
/// registers workspace-level action handlers. Call once at startup, after
/// `dontspeak::init`. A no-op when the `dontspeak` global is missing.
pub fn init(cx: &mut App) {
    let Some(dontspeak) = DontSpeak::global(cx) else {
        return;
    };
    let controller = cx.new(|cx| {
        DictationController::new(
            Rc::new(dictation_controller::ZedWindowOps::new(
                dontspeak.downgrade(),
            )),
            Some(&dontspeak),
            cx,
        )
    });
    cx.set_global(GlobalDictationController(controller));

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_, _: &OpenVoiceSettings, window, cx| {
            window.dispatch_action(
                OpenSettingsPage {
                    page: VOICE_SETTINGS_PAGE.into(),
                    target: None,
                }
                .boxed_clone(),
                cx,
            );
        });
    })
    .detach();
}

/// The settings-UI page title targeted by "Open Voice Settings" (added by
/// the Voice settings page task).
pub(crate) const VOICE_SETTINGS_PAGE: &str = "Voice";

struct GlobalDictationController(Entity<DictationController>);

impl Global for GlobalDictationController {}

impl DictationController {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDictationController>()
            .map(|global| global.0.clone())
    }
}
