//! The Scoro "Today" tasks integration: a global store that watches a JSON file
//! (kept fresh by an external process) listing the day's tasks. The tasks are
//! rendered in the window-level workspace sidebar (see `crates/sidebar`), which
//! opens the matching local project when a task is clicked.

mod scoro_store;

use gpui::App;
use settings::{RegisterSetting, Settings};

pub use scoro_store::{ScoroEvent, ScoroStore, ScoroTask, group_tasks};

/// Runtime settings for the Scoro Today feature.
#[derive(Debug, Clone, RegisterSetting)]
pub struct ScoroSettings {
    pub tasks_file: Option<String>,
    pub button: bool,
}

impl Settings for ScoroSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let scoro = content.scoro.clone().unwrap_or_default();
        Self {
            tasks_file: scoro.tasks_file,
            button: scoro.button.unwrap_or(true),
        }
    }
}

pub fn init(cx: &mut App) {
    scoro_store::init(cx);
}
