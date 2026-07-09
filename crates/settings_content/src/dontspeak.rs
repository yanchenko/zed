use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Configuration of the DontSpeak voice integration (dictation + narration).
#[with_fallible_options]
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct DontSpeakSettingsContent {
    /// Whether to integrate with a locally running DontSpeak daemon
    /// (CapsLock dictation into Zed inputs, narration of agent replies).
    /// When the daemon is not installed or not running, this setting has
    /// no visible effect.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Which agent-panel agents have their replies narrated by DontSpeak.
    ///
    /// Default: auto
    pub narrate_panel_agents: Option<NarratePanelAgents>,
    /// Whether to show the DontSpeak status button in the status bar.
    ///
    /// Default: true
    pub status_bar_button: Option<bool>,
}

/// Which agent-panel agents have their replies narrated by DontSpeak.
///
/// Default: auto
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    JsonSchema,
    MergeFrom,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum NarratePanelAgents {
    /// Narrate only agents without DontSpeak hook wiring of their own
    /// (hook-wired agents already narrate through the daemon's hooks).
    #[default]
    Auto,
    /// Narrate every agent-panel agent.
    All,
    /// Never narrate agent-panel agents.
    None,
}
