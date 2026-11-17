//! The settings dialog's state and key map.
//!
//! The panel itself is drawn in [`super::modals::render_settings`] — the border
//! is what puts a dialog in that file, and the no-boxes guard allows a border
//! nowhere else. This module owns the editable fields, the masked key, the
//! async apply's status and the whole key map as a free function, so a key can
//! be injected into `TuiApp::dispatch` from a test without a terminal.
//!
//! The raw API key lives in `key` and is **never** rendered, logged or put in a
//! URL. [`Settings::masked_key`] is the only representation a view ever sees;
//! the bytes travel to `Config` and from there to the adapter as a header, and
//! to `.env` through `Config::save_to_env`.

use crate::config::{Config, ProviderKind};
use crossterm::event::{self, KeyCode, KeyModifiers};
use std::sync::{Arc, Mutex};

/// The providers the dialog cycles through, in display order. Indexes into this
/// table are what [`Settings::provider`] holds, so the cycle wraps over its
/// length rather than over a magic number.
pub const PROVIDERS: [&str; 3] = ["Ollama", "OpenRouter", "OpenCode"];

/// Field indexes. Named because the key map branches on them, and a bare
/// number would be the kind of thing a reordered panel changes silently.
pub const PROVIDER_FIELD: usize = 0;
pub const KEY_FIELD: usize = 1;
pub const MODEL_FIELD: usize = 2;
pub const APPLY_FIELD: usize = 3;
pub const CANCEL_FIELD: usize = 4;
pub const FIELD_COUNT: usize = 5;

/// Where an Apply got to. Shared behind an `Arc<Mutex<_>>` because the async
/// task that builds the stack updates it while the render loop reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsStatus {
    Idle,
    Applying,
    Applied(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Index into [`PROVIDERS`].
    pub provider: usize,
    /// The raw key. Never drawn, logged or placed in a URL — only the masked
    /// form from [`Settings::masked_key`] reaches a view.
    pub key: String,
    pub model: String,
    pub focus: usize,
    pub status: Arc<Mutex<SettingsStatus>>,
}

impl Settings {
    pub fn new(provider: ProviderKind, key: &str, model: &str) -> Self {
        let index = match provider {
            ProviderKind::Ollama => 0,
            ProviderKind::OpenRouter => 1,
            ProviderKind::OpenCode => 2,
        };
        Self {
            provider: index,
            key: key.to_string(),
            model: model.to_string(),
            focus: PROVIDER_FIELD,
            status: Arc::new(Mutex::new(SettingsStatus::Idle)),
        }
    }

    pub fn provider_kind(&self) -> ProviderKind {
        match self.provider {
            1 => ProviderKind::OpenRouter,
            2 => ProviderKind::OpenCode,
            _ => ProviderKind::Ollama,
        }
    }

    /// Only Ollama runs without a credential. The others need one to talk to
    /// their upstream at all, so an Apply with an empty key is refused before
    /// any stack is built or any file is written.
    pub fn needs_key(&self) -> bool {
        self.provider_kind() != ProviderKind::Ollama
    }

    /// The only view of the key a dialog is allowed to show. A fixed mask and a
    /// set/unset marker: neither the bytes nor their length is exposed.
    pub fn masked_key(&self) -> &'static str {
        if self.key.is_empty() {
            "(unset)"
        } else {
            "••••••••  (set)"
        }
    }

    /// Validate the three fields and build the `Config` an Apply installs.
    ///
    /// Reads the current environment for the fields this dialog does not edit,
    /// so port, URLs, embedding and router tuning survive a hot swap. Errors
    /// name the field and carry no key material.
    pub fn validated_config(&self) -> Result<Config, String> {
        let mut config = Config::from_env().map_err(|err| err.to_string())?;
        let provider = self.provider_kind();
        let model = self.model.trim();
        if model.is_empty() {
            return Err("model cannot be empty".to_string());
        }
        if self.needs_key() && self.key.trim().is_empty() {
            return Err(format!("{provider} requires an API key"));
        }
        config.provider = provider;
        config.default_model = model.to_string();
        match provider {
            ProviderKind::OpenRouter => config.openrouter_api_key = Some(self.key.clone()),
            ProviderKind::OpenCode => config.opencode_api_key = Some(self.key.clone()),
            ProviderKind::Ollama => {}
        }
        Ok(config)
    }
}

/// What a single key press means in the settings dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    Cancel,
    NextField,
    PrevField,
    CycleProvider(isize),
    Edit(char),
    Backspace,
    Apply,
    None,
}

/// The dialog's whole key map, as a free function so `dispatch` can be tested
/// by injecting keys. It takes every key while the dialog is up, so nothing is
/// written as a hard-coded hint in the panel — the row lives in `hints_for`.
pub fn settings_action(settings: &Settings, key: event::KeyEvent) -> SettingsAction {
    match key.code {
        KeyCode::Esc => SettingsAction::Cancel,
        KeyCode::Tab | KeyCode::Down => SettingsAction::NextField,
        KeyCode::BackTab | KeyCode::Up => SettingsAction::PrevField,
        KeyCode::Left | KeyCode::Right if settings.focus == PROVIDER_FIELD => {
            let delta = if key.code == KeyCode::Right { 1 } else { -1 };
            SettingsAction::CycleProvider(delta)
        }
        KeyCode::Enter if settings.focus == APPLY_FIELD => SettingsAction::Apply,
        KeyCode::Enter if settings.focus == CANCEL_FIELD => SettingsAction::Cancel,
        KeyCode::Enter => SettingsAction::NextField,
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            SettingsAction::Apply
        }
        KeyCode::Char(c) if settings.focus == KEY_FIELD || settings.focus == MODEL_FIELD => {
            SettingsAction::Edit(c)
        }
        KeyCode::Backspace if settings.focus == KEY_FIELD || settings.focus == MODEL_FIELD => {
            SettingsAction::Backspace
        }
        _ => SettingsAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn the_raw_key_never_appears_in_the_mask() {
        let settings = Settings::new(ProviderKind::OpenRouter, "sk-super-secret-123", "m");
        assert!(!settings.masked_key().contains("sk-super-secret-123"));
        assert!(!settings.masked_key().contains("secret"));
        assert!(settings.masked_key().contains("(set)"));
        assert!(Settings::new(ProviderKind::Ollama, "", "m")
            .masked_key()
            .contains("(unset)"));
    }

    #[test]
    fn arrows_cycle_the_provider_only_on_its_field() {
        let mut settings = Settings::new(ProviderKind::Ollama, "", "m");
        assert_eq!(
            settings_action(&settings, key(KeyCode::Right)),
            SettingsAction::CycleProvider(1)
        );
        settings.focus = MODEL_FIELD;
        assert_eq!(
            settings_action(&settings, key(KeyCode::Right)),
            SettingsAction::None,
            "on a typing field the arrow is not a provider cycle"
        );
    }

    #[test]
    fn enter_on_apply_applies_and_enter_on_cancel_cancels() {
        let settings = Settings::new(ProviderKind::Ollama, "", "m");
        let mut on_apply = settings.clone();
        on_apply.focus = APPLY_FIELD;
        assert_eq!(
            settings_action(&on_apply, key(KeyCode::Enter)),
            SettingsAction::Apply
        );
        let mut on_cancel = settings;
        on_cancel.focus = CANCEL_FIELD;
        assert_eq!(
            settings_action(&on_cancel, key(KeyCode::Enter)),
            SettingsAction::Cancel
        );
    }

    #[test]
    fn an_empty_key_is_rejected_for_keyed_providers_only() {
        let _guard = crate::core::paths::lock_env_for_tests();
        let mut settings = Settings::new(ProviderKind::OpenRouter, "", "a-model");
        assert!(settings.validated_config().unwrap_err().contains("API key"));
        settings.provider = 0; // Ollama
        assert!(settings.validated_config().is_ok());
    }
}
