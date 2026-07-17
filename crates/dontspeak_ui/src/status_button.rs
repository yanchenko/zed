//! The DontSpeak status-bar button (modeled on `EditPredictionButton`).
//!
//! Self-hiding: renders nothing when the integration is disabled in
//! settings, when the daemon is absent, or when `status_bar_button` is
//! false — so Zed looks exactly like upstream unless DontSpeak is actually
//! running. Otherwise it shows the dictation phase (idle / recording with a
//! pulse / awaiting-confirm), surfaces connection errors as a toast with an
//! "Open Voice Settings" recovery action, and hosts a popover menu with the
//! daemon controls.

use std::sync::Arc;
use std::time::Duration;

use dontspeak::{DontSpeak, DontSpeakSettings, OpenVoiceSettings, Reconnect, Status, StopSpeech};
use gpui::{
    Action as _, Anchor, Animation, AnimationExt as _, AnyElement, App, Context, Entity,
    Styled as _, Subscription, Window, div, pulsating_between,
};
use settings::Settings as _;
use ui::{ContextMenu, IconButton, Indicator, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*};
use workspace::{
    HideStatusItem, StatusItemView, Toast, Workspace, item::ItemHandle,
    notifications::NotificationId,
};
use zed_actions::OpenSettingsPage;

use crate::{DictationController, DictationPhase, VOICE_SETTINGS_PAGE};

struct DontSpeakErrorToast;

pub struct DontSpeakStatusButton {
    dontspeak: Option<Entity<DontSpeak>>,
    controller: Option<Entity<DictationController>>,
    popover_menu_handle: PopoverMenuHandle<ContextMenu>,
    _subscriptions: Vec<Subscription>,
}

impl DontSpeakStatusButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let dontspeak = DontSpeak::global(cx);
        let controller = DictationController::global(cx);
        let mut subscriptions = Vec::new();
        if let Some(dontspeak) = &dontspeak {
            subscriptions.push(cx.observe(dontspeak, |_, _, cx| cx.notify()));
        }
        if let Some(controller) = &controller {
            subscriptions.push(cx.observe(controller, |_, _, cx| cx.notify()));
        }
        Self {
            dontspeak,
            controller,
            popover_menu_handle: PopoverMenuHandle::default(),
            _subscriptions: subscriptions,
        }
    }

    fn build_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        let connected = self
            .dontspeak
            .as_ref()
            .is_some_and(|dontspeak| dontspeak.read(cx).status().is_connected());
        ContextMenu::build(window, cx, |menu, _, _| {
            let mut menu = menu.header("DontSpeak");
            if connected {
                menu = menu.action("Stop Speech", StopSpeech.boxed_clone());
            } else {
                menu = menu.action("Reconnect", Reconnect.boxed_clone());
            }
            menu.separator()
                .action("Open Voice Settings", OpenVoiceSettings.boxed_clone())
        })
    }

    fn render_error(&self, error: Arc<str>, cx: &mut Context<Self>) -> AnyElement {
        div()
            .child(
                IconButton::new("dontspeak-error", IconName::MicMute)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Error)
                    .on_click(cx.listener(move |_, _, window, cx| {
                        let error = error.clone();
                        if let Some(workspace) = Workspace::for_window(window, cx) {
                            workspace.update(cx, |workspace, cx| {
                                workspace.show_toast(
                                    Toast::new(
                                        NotificationId::unique::<DontSpeakErrorToast>(),
                                        format!("DontSpeak can't connect: {error}"),
                                    )
                                    .on_click(
                                        "Open Voice Settings",
                                        |window, cx| {
                                            window.dispatch_action(
                                                OpenSettingsPage {
                                                    page: VOICE_SETTINGS_PAGE.into(),
                                                    target: None,
                                                }
                                                .boxed_clone(),
                                                cx,
                                            );
                                        },
                                    ),
                                    cx,
                                );
                            });
                        }
                    }))
                    .tooltip(Tooltip::text("DontSpeak Error")),
            )
            .into_any_element()
    }

    fn render_button(&self, connected: bool, cx: &mut Context<Self>) -> AnyElement {
        let phase = if connected {
            self.controller
                .as_ref()
                .map_or(DictationPhase::Idle, |controller| {
                    controller.read(cx).phase()
                })
        } else {
            DictationPhase::Idle
        };

        let (icon_color, indicator) = match phase {
            _ if !connected => (Color::Disabled, None),
            DictationPhase::Idle => (Color::Muted, None),
            DictationPhase::Recording => (Color::Error, Some(Indicator::dot().color(Color::Error))),
            DictationPhase::AwaitingConfirm => {
                (Color::Accent, Some(Indicator::dot().color(Color::Accent)))
            }
        };

        let mut button = IconButton::new("dontspeak-icon", IconName::Mic)
            .icon_size(IconSize::Small)
            .icon_color(icon_color);
        if let Some(indicator) = indicator {
            button = button.indicator(indicator);
        }

        let this = cx.weak_entity();
        let container = div().child(
            PopoverMenu::new("dontspeak")
                .menu(move |window, cx| {
                    this.update(cx, |this, cx| this.build_menu(window, cx)).ok()
                })
                .anchor(Anchor::BottomRight)
                .trigger_with_tooltip(button, Tooltip::text("DontSpeak"))
                .with_handle(self.popover_menu_handle.clone()),
        );

        if phase == DictationPhase::Recording {
            container
                .with_animation(
                    "dontspeak-recording-pulse",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.4, 0.9)),
                    |container, delta| container.opacity(delta),
                )
                .into_any_element()
        } else {
            container.into_any_element()
        }
    }
}

impl Render for DontSpeakStatusButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !DontSpeakSettings::get_global(cx).status_bar_button {
            return div().hidden().into_any_element();
        }
        let Some(dontspeak) = self.dontspeak.clone() else {
            return div().hidden().into_any_element();
        };
        match dontspeak.read(cx).status() {
            // Absent means DontSpeak is not installed or not running: Zed
            // must look exactly like upstream.
            Status::Disabled | Status::Absent => div().hidden().into_any_element(),
            Status::Error(error) => self.render_error(error, cx),
            Status::Connecting => self.render_button(false, cx),
            Status::Connected => self.render_button(true, cx),
        }
    }
}

impl StatusItemView for DontSpeakStatusButton {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|content| {
            content.dontspeak.get_or_insert_default().status_bar_button = Some(false);
        }))
    }
}
