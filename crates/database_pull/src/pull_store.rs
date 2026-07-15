use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use project::ProjectGroupKey;

use crate::pipeline;
use crate::pull_settings::{DatabasePullConfig, PullSource, PullTarget, SshTarget};

const STDERR_TAIL_LINES: usize = 30;
const DONE_DISPLAY_DURATION: Duration = Duration::from_secs(5);
const CANCELLED_DISPLAY_DURATION: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub enum PullStage {
    Connecting,
    Dumping,
    LocatingBackup,
    Downloading { bytes: u64, total: u64 },
    Importing { bytes: u64, total: u64 },
    Uploading { bytes: u64, total: u64 },
    PostStep { index: usize, count: usize, name: SharedString },
    Done { message: SharedString },
    Failed { stage: SharedString, message: SharedString },
    Cancelled,
}

impl PullStage {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Dumping => "dumping the remote database",
            Self::LocatingBackup => "locating the backup file",
            Self::Downloading { .. } => "downloading the dump",
            Self::Importing { .. } => "importing the dump",
            Self::Uploading { .. } => "uploading to Google Drive",
            Self::PostStep { .. } => "running post-import steps",
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn status_message(&self) -> String {
        fn percentage(bytes: u64, total: u64) -> u64 {
            bytes.saturating_mul(100).checked_div(total).unwrap_or(0)
        }
        match self {
            Self::Connecting => "Connecting to server…".to_string(),
            Self::Dumping => "Dumping remote database…".to_string(),
            Self::LocatingBackup => "Locating newest backup…".to_string(),
            Self::Downloading { bytes, total } => {
                format!("Downloading database {}%", percentage(*bytes, *total))
            }
            Self::Importing { bytes, total } => {
                format!("Importing database {}%", percentage(*bytes, *total))
            }
            Self::Uploading { bytes, total } => {
                format!("Uploading to Drive {}%", percentage(*bytes, *total))
            }
            Self::PostStep { index, count, name } => {
                format!("{} ({}/{})", name, index + 1, count)
            }
            Self::Done { message } => message.to_string(),
            Self::Failed { stage, .. } => format!("Database pull failed while {stage}"),
            Self::Cancelled => "Database pull cancelled".to_string(),
        }
    }

    /// Byte progress for stages with measurable progress.
    pub fn progress(&self) -> Option<(u64, u64)> {
        match self {
            Self::Downloading { bytes, total }
            | Self::Importing { bytes, total }
            | Self::Uploading { bytes, total } => Some((*bytes, *total)),
            _ => None,
        }
    }

    pub fn is_running(&self) -> bool {
        !matches!(self, Self::Done { .. } | Self::Failed { .. } | Self::Cancelled)
    }
}

/// Files created by a pull that must be removed when it finishes, fails, or
/// is cancelled.
#[derive(Clone, Debug, Default)]
pub struct PullCleanup {
    pub ssh: Option<SshTarget>,
    pub remote_tmp_path: Option<String>,
    pub local_dump_path: Option<PathBuf>,
}

pub struct PullState {
    pub stage: PullStage,
    pub started_at: Instant,
    pub stderr_tail: VecDeque<String>,
    cleanup: PullCleanup,
    task: Option<Task<()>>,
}

impl PullState {
    pub fn full_error(&self) -> Option<String> {
        let PullStage::Failed { stage, message } = &self.stage else {
            return None;
        };
        let mut error = format!("Database pull failed while {stage}: {message}");
        if !self.stderr_tail.is_empty() {
            error.push_str("\n\nRecent output:\n");
            for line in &self.stderr_tail {
                error.push_str(line);
                error.push('\n');
            }
        }
        Some(error)
    }
}

/// The outcome of the most recent finished pull for a project, kept after the
/// live [`PullState`] is cleared so the dashboard can show the last result.
#[derive(Clone, Debug)]
pub struct LastRun {
    pub environment: SharedString,
    pub source: SharedString,
    pub target: SharedString,
    pub outcome: Result<SharedString, String>,
    pub finished_at: Instant,
}

pub struct DatabasePullStore {
    pulls: HashMap<ProjectGroupKey, PullState>,
    last_runs: HashMap<ProjectGroupKey, LastRun>,
}

pub enum PullEvent {
    Updated,
}

impl EventEmitter<PullEvent> for DatabasePullStore {}

struct GlobalDatabasePullStore(Entity<DatabasePullStore>);

impl Global for GlobalDatabasePullStore {}

pub fn init(cx: &mut App) {
    let store = cx.new(|_| DatabasePullStore {
        pulls: HashMap::default(),
        last_runs: HashMap::default(),
    });
    cx.set_global(GlobalDatabasePullStore(store));
}

impl DatabasePullStore {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalDatabasePullStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDatabasePullStore>()
            .map(|global| global.0.clone())
    }

    pub fn state(&self, key: &ProjectGroupKey) -> Option<&PullState> {
        self.pulls
            .iter()
            .find(|(existing, _)| existing.matches(key))
            .map(|(_, state)| state)
    }

    pub fn is_running(&self, key: &ProjectGroupKey) -> bool {
        self.state(key)
            .is_some_and(|state| state.stage.is_running())
    }

    /// The most recent finished pull for a project, if any. Survives the live
    /// [`PullState`] being auto-cleared, so the dashboard can show it.
    pub fn last_run(&self, key: &ProjectGroupKey) -> Option<&LastRun> {
        self.last_runs
            .iter()
            .find(|(existing, _)| existing.matches(key))
            .map(|(_, run)| run)
    }

    pub fn start_pull(
        &mut self,
        key: ProjectGroupKey,
        config: DatabasePullConfig,
        environment: SharedString,
        source: PullSource,
        target: PullTarget,
        project_dir: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.is_running(&key),
            "a database pull is already running for this project"
        );

        let source_name = source.name().clone();
        let target_name = target.name().clone();

        let task = cx.spawn({
            let key = key.clone();
            async move |store, cx| {
                let result =
                    pipeline::run_pull(config, source, target, project_dir, store.clone(), &key, cx)
                        .await;

                let cleanup = store
                    .update(cx, |store, _| {
                        store
                            .pulls
                            .iter_mut()
                            .find(|(existing, _)| existing.matches(&key))
                            .map(|(_, state)| std::mem::take(&mut state.cleanup))
                    })
                    .ok()
                    .flatten();
                if let Some(cleanup) = cleanup {
                    cx.background_spawn(pipeline::cleanup(cleanup)).detach();
                }

                match result {
                    Ok(message) => {
                        store
                            .update(cx, |store, cx| {
                                store.last_runs.insert(
                                    key.clone(),
                                    LastRun {
                                        environment: environment.clone(),
                                        source: source_name.clone(),
                                        target: target_name.clone(),
                                        outcome: Ok(message.clone()),
                                        finished_at: Instant::now(),
                                    },
                                );
                                store.set_stage(&key, PullStage::Done { message }, cx);
                                store.remove_after(key.clone(), DONE_DISPLAY_DURATION, cx);
                            })
                            .ok();
                    }
                    Err(error) => {
                        log::error!(target: "database_pull", "pull failed: {error:#}");
                        store
                            .update(cx, |store, cx| {
                                let stage_name = store
                                    .state(&key)
                                    .map(|state| state.stage.name())
                                    .unwrap_or("running");
                                store.set_stage(
                                    &key,
                                    PullStage::Failed {
                                        stage: stage_name.into(),
                                        message: format!("{error:#}").into(),
                                    },
                                    cx,
                                );
                                let error_text = store
                                    .state(&key)
                                    .and_then(|state| state.full_error())
                                    .unwrap_or_else(|| format!("{error:#}"));
                                store.last_runs.insert(
                                    key.clone(),
                                    LastRun {
                                        environment: environment.clone(),
                                        source: source_name.clone(),
                                        target: target_name.clone(),
                                        outcome: Err(error_text),
                                        finished_at: Instant::now(),
                                    },
                                );
                            })
                            .ok();
                    }
                }
            }
        });

        self.pulls.retain(|existing, _| !existing.matches(&key));
        self.pulls.insert(
            key,
            PullState {
                stage: PullStage::Connecting,
                started_at: Instant::now(),
                stderr_tail: VecDeque::new(),
                cleanup: PullCleanup::default(),
                task: Some(task),
            },
        );
        cx.emit(PullEvent::Updated);
        cx.notify();
        Ok(())
    }

    /// Cancels a running pull. Dropping the task kills all child processes
    /// (they are spawned with `kill_on_drop`), so cleanup of temp files is
    /// done here instead of in the pipeline.
    pub fn cancel(&mut self, key: &ProjectGroupKey, cx: &mut Context<Self>) {
        let Some((_, state)) = self
            .pulls
            .iter_mut()
            .find(|(existing, _)| existing.matches(key))
        else {
            return;
        };
        if !state.stage.is_running() {
            return;
        }
        state.task.take();
        state.stage = PullStage::Cancelled;
        let cleanup = std::mem::take(&mut state.cleanup);
        cx.background_spawn(pipeline::cleanup(cleanup)).detach();
        self.remove_after(key.clone(), CANCELLED_DISPLAY_DURATION, cx);
        cx.emit(PullEvent::Updated);
        cx.notify();
    }

    pub fn dismiss(&mut self, key: &ProjectGroupKey, cx: &mut Context<Self>) {
        let running = self.is_running(key);
        if !running {
            self.pulls.retain(|existing, _| !existing.matches(key));
            cx.emit(PullEvent::Updated);
            cx.notify();
        }
    }

    pub(crate) fn set_stage(&mut self, key: &ProjectGroupKey, stage: PullStage, cx: &mut Context<Self>) {
        if let Some((_, state)) = self
            .pulls
            .iter_mut()
            .find(|(existing, _)| existing.matches(key))
        {
            state.stage = stage;
            cx.emit(PullEvent::Updated);
            cx.notify();
        }
    }

    pub(crate) fn push_stderr(&mut self, key: &ProjectGroupKey, lines: Vec<String>, cx: &mut Context<Self>) {
        if let Some((_, state)) = self
            .pulls
            .iter_mut()
            .find(|(existing, _)| existing.matches(key))
        {
            for line in lines {
                if state.stderr_tail.len() >= STDERR_TAIL_LINES {
                    state.stderr_tail.pop_front();
                }
                state.stderr_tail.push_back(line);
            }
            cx.emit(PullEvent::Updated);
            cx.notify();
        }
    }

    pub(crate) fn record_cleanup(
        &mut self,
        key: &ProjectGroupKey,
        update: impl FnOnce(&mut PullCleanup),
    ) {
        if let Some((_, state)) = self
            .pulls
            .iter_mut()
            .find(|(existing, _)| existing.matches(key))
        {
            update(&mut state.cleanup);
        }
    }

    fn remove_after(&mut self, key: ProjectGroupKey, duration: Duration, cx: &mut Context<Self>) {
        cx.spawn(async move |store, cx| {
            cx.background_executor().timer(duration).await;
            store
                .update(cx, |store, cx| {
                    // Don't remove a pull that was restarted in the meantime.
                    if store
                        .state(&key)
                        .is_some_and(|state| !state.stage.is_running())
                    {
                        store.pulls.retain(|existing, _| !existing.matches(&key));
                        cx.emit(PullEvent::Updated);
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }
}
