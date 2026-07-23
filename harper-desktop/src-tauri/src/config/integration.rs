use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integration {
    pub bundle_id: String,
    pub enabled: bool,
}

impl Integration {
    pub fn curated_integrations() -> Vec<Self> {
        #[cfg(target_os = "macos")]
        let ids: &[&str] = &[
            "com.apple.TextEdit",
            "com.apple.mail",
            "com.apple.MobileSMS",
            "com.apple.Notes",
            "com.tinyspeck.slackmacgap",
            "com.hnc.Discord",
        ];

        // Windows has no bundle identifiers; integrations are keyed by the
        // lowercase executable name of the process owning the foreground
        // window. olk.exe is new Outlook, ms-teams.exe is new Teams.
        #[cfg(target_os = "windows")]
        let ids: &[&str] = &[
            "notepad.exe",
            "olk.exe",
            "ms-teams.exe",
            "slack.exe",
            "discord.exe",
        ];

        // No highlighter broker exists for other platforms yet, so there are
        // no meaningful defaults to curate.
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let ids: &[&str] = &[];

        ids.iter()
            .map(|bundle_id| Integration {
                bundle_id: bundle_id.to_string(),
                enabled: true,
            })
            .collect()
    }
    pub fn is_integration_enabled_in(integrations: &[Self], bundle_id: &str) -> bool {
        integrations
            .iter()
            .any(|integration| integration.bundle_id == bundle_id && integration.enabled)
    }
}
