//! Live in-memory model of the day's Scoro tasks, read from a JSON file that an
//! external process keeps up to date. The store is a `Global` observed by the
//! Today panel; it watches the file and re-parses on change. Modelled on
//! `crate::…` sibling stores (e.g. database_pull's `DatabasePullStore`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task, WeakEntity};
use serde::Deserialize;
use settings::Settings as _;

use crate::ScoroSettings;

pub const DEFAULT_TASKS_FILE: &str = "scoro-tasks.json";

/// The on-disk JSON contract. All fields but `id`, `title` and `path` are
/// optional so a minimal producer can emit just those.
#[derive(Debug, Clone, Deserialize)]
pub struct TasksFile {
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub tasks: Vec<TaskEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub project: Option<String>,
    pub path: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub due: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

/// A task resolved for display: `path` tilde-expanded, `project` filled in from
/// the directory name when the producer omitted it.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoroTask {
    pub id: SharedString,
    pub title: SharedString,
    pub project: SharedString,
    pub path: PathBuf,
    pub status: Option<SharedString>,
    pub due: Option<SharedString>,
    pub url: Option<SharedString>,
    pub branch: Option<SharedString>,
}

impl ScoroTask {
    fn from_entry(entry: TaskEntry) -> Self {
        let path = PathBuf::from(shellexpand::tilde(&entry.path).into_owned());
        let project = entry
            .project
            .filter(|project| !project.trim().is_empty())
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.path.clone())
            });
        Self {
            id: entry.id.into(),
            title: entry.title.into(),
            project: project.into(),
            path,
            status: entry.status.map(Into::into),
            due: entry.due.map(Into::into),
            url: entry.url.map(Into::into),
            branch: entry.branch.map(Into::into),
        }
    }
}

/// Parses the JSON contract into resolved tasks plus the optional `generated_at`.
pub fn parse_tasks(json: &str) -> Result<(Vec<ScoroTask>, Option<SharedString>)> {
    let file: TasksFile = serde_json::from_str(json)?;
    let generated_at = file.generated_at.map(Into::into);
    let tasks = file.tasks.into_iter().map(ScoroTask::from_entry).collect();
    Ok((tasks, generated_at))
}

/// Groups tasks by project label, preserving first-seen project order and task
/// order within each group.
pub fn group_tasks(tasks: &[ScoroTask]) -> Vec<(SharedString, Vec<ScoroTask>)> {
    let mut groups: Vec<(SharedString, Vec<ScoroTask>)> = Vec::new();
    for task in tasks {
        if let Some(group) = groups.iter_mut().find(|(project, _)| *project == task.project) {
            group.1.push(task.clone());
        } else {
            groups.push((task.project.clone(), vec![task.clone()]));
        }
    }
    groups
}

pub struct ScoroStore {
    tasks: Vec<ScoroTask>,
    generated_at: Option<SharedString>,
    error: Option<SharedString>,
    _watch: Option<Task<()>>,
}

pub enum ScoroEvent {
    Updated,
}

impl EventEmitter<ScoroEvent> for ScoroStore {}

struct GlobalScoroStore(Entity<ScoroStore>);

impl Global for GlobalScoroStore {}

pub fn init(cx: &mut App) {
    let store = cx.new(|_| ScoroStore {
        tasks: Vec::new(),
        generated_at: None,
        error: None,
        _watch: None,
    });
    cx.set_global(GlobalScoroStore(store.clone()));

    let fs = <dyn Fs>::global(cx);
    let path = tasks_file_path(cx);
    let watch = start_watch(store.downgrade(), fs, path, cx);
    store.update(cx, |store, _| store._watch = Some(watch));
}

fn tasks_file_path(cx: &App) -> PathBuf {
    match &ScoroSettings::get_global(cx).tasks_file {
        Some(path) if !path.trim().is_empty() => {
            PathBuf::from(shellexpand::tilde(path).into_owned())
        }
        _ => paths::config_dir().join(DEFAULT_TASKS_FILE),
    }
}

/// Watches the tasks file for changes (via its parent directory, so atomic
/// rename-replaces are caught) and reloads the store on each change.
fn start_watch(
    store: WeakEntity<ScoroStore>,
    fs: Arc<dyn Fs>,
    path: PathBuf,
    cx: &mut App,
) -> Task<()> {
    cx.spawn(async move |cx| {
        reload(&store, &fs, &path, cx).await;

        let Some(parent) = path.parent().map(|parent| parent.to_path_buf()) else {
            return;
        };
        let file_name = path.file_name().map(|name| name.to_os_string());
        let (mut events, _watcher) = fs.watch(&parent, Duration::from_millis(200)).await;
        while let Some(batch) = events.next().await {
            let touched = batch.iter().any(|event| {
                event.path == path
                    || (file_name.is_some()
                        && event.path.file_name().map(|name| name.to_os_string()) == file_name)
            });
            if touched {
                reload(&store, &fs, &path, cx).await;
            }
        }
    })
}

async fn reload(
    store: &WeakEntity<ScoroStore>,
    fs: &Arc<dyn Fs>,
    path: &Path,
    cx: &mut gpui::AsyncApp,
) {
    let loaded = fs.load(path).await;
    store
        .update(cx, |store, cx| match loaded {
            // Missing or empty file → no tasks, no error (the producer may not
            // have written it yet).
            Ok(text) if text.trim().is_empty() => store.apply(Vec::new(), None, None, cx),
            Ok(text) => match parse_tasks(&text) {
                Ok((tasks, generated_at)) => store.apply(tasks, generated_at, None, cx),
                // Keep the last good tasks visible; surface the parse error.
                Err(error) => store.fail(format!("{error:#}").into(), cx),
            },
            Err(_) => store.apply(Vec::new(), None, None, cx),
        })
        .ok();
}

impl ScoroStore {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalScoroStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalScoroStore>().map(|global| global.0.clone())
    }

    pub fn tasks(&self) -> &[ScoroTask] {
        &self.tasks
    }

    pub fn generated_at(&self) -> Option<&SharedString> {
        self.generated_at.as_ref()
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    fn apply(
        &mut self,
        tasks: Vec<ScoroTask>,
        generated_at: Option<SharedString>,
        error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.tasks = tasks;
        self.generated_at = generated_at;
        self.error = error;
        cx.emit(ScoroEvent::Updated);
        cx.notify();
    }

    fn fail(&mut self, error: SharedString, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.emit(ScoroEvent::Updated);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_and_groups() {
        let json = r#"{
            "generated_at": "2026-07-15T09:00:00Z",
            "tasks": [
                { "id": "1", "title": "Fix cart", "project": "Vendella", "path": "/x/vendella", "status": "in_progress", "due": "2026-07-15", "url": "https://s/1" },
                { "id": "2", "title": "Invoice PDF", "project": "Vendella", "path": "/x/vendella" },
                { "id": "3", "title": "Auction timer", "project": "SBL", "path": "/x/sbl" }
            ]
        }"#;
        let (tasks, generated_at) = parse_tasks(json).expect("should parse");
        assert_eq!(generated_at.as_deref(), Some("2026-07-15T09:00:00Z"));
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].title, SharedString::from("Fix cart"));
        assert_eq!(tasks[0].status.as_deref(), Some("in_progress"));

        let groups = group_tasks(&tasks);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, SharedString::from("Vendella"));
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, SharedString::from("SBL"));
    }

    #[test]
    fn missing_optionals_ok_and_project_falls_back_to_dir() {
        let json = r#"{ "tasks": [ { "id": "9", "title": "T", "path": "/home/me/projects/acme" } ] }"#;
        let (tasks, generated_at) = parse_tasks(json).expect("should parse");
        assert!(generated_at.is_none());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].project, SharedString::from("acme"));
        assert!(tasks[0].due.is_none());
        assert!(tasks[0].url.is_none());
    }

    #[test]
    fn tilde_expands_in_path() {
        let json = r#"{ "tasks": [ { "id": "1", "title": "T", "path": "~/Herd/vendella" } ] }"#;
        let (tasks, _) = parse_tasks(json).expect("should parse");
        assert!(
            !tasks[0].path.to_string_lossy().contains('~'),
            "tilde should expand"
        );
        assert_eq!(tasks[0].project, SharedString::from("vendella"));
    }

    #[test]
    fn malformed_json_errors() {
        assert!(parse_tasks("{ not json").is_err());
        // Missing required field `path` is an error.
        assert!(parse_tasks(r#"{ "tasks": [ { "id": "1", "title": "T" } ] }"#).is_err());
    }

    #[test]
    fn empty_tasks_array_is_ok() {
        let (tasks, _) = parse_tasks(r#"{ "tasks": [] }"#).expect("should parse");
        assert!(tasks.is_empty());
    }
}
