use settings::{RegisterSetting, Settings};

pub use settings::NarratePanelAgents;

/// Resolved DontSpeak integration settings (see `DontSpeakSettingsContent`
/// in `settings_content` and the `"dontspeak"` section of
/// `assets/settings/default.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, RegisterSetting)]
pub struct DontSpeakSettings {
    /// Whether to integrate with a locally running DontSpeak daemon.
    pub enabled: bool,
    /// Which agent-panel agents have their replies narrated by DontSpeak.
    pub narrate_panel_agents: NarratePanelAgents,
    /// Whether to show the DontSpeak status button in the status bar.
    pub status_bar_button: bool,
}

impl Settings for DontSpeakSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let dontspeak = content.dontspeak.as_ref().unwrap();
        DontSpeakSettings {
            enabled: dontspeak.enabled.unwrap(),
            narrate_panel_agents: dontspeak.narrate_panel_agents.unwrap(),
            status_bar_button: dontspeak.status_bar_button.unwrap(),
        }
    }
}
