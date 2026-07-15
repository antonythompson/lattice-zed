mod commands;
mod gdrive;
mod pipeline;
mod pull_form;
mod pull_indicator;
mod pull_modal;
mod pull_picker;
mod pull_settings;
mod pull_store;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, PromptLevel, Render,
    SharedString, WeakEntity, Window, actions,
};
use project::ProjectGroupKey;
use settings::{Settings as _, SettingsLocation};
use ui::prelude::*;
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::notifications::NotificationId;
use workspace::{ModalView, Toast, Workspace};

pub use pull_indicator::PullIndicator;
pub use pull_settings::{
    DatabasePullConfig, DatabasePullSettings, PullEnvironment, PullSource, PullTarget,
};
pub use pull_store::{DatabasePullStore, PullEvent, PullStage, PullState};

/// Machine-level keychain entry for Google Drive service-account credentials
/// (shared across projects; only the Drive folder is per-project).
pub const GDRIVE_KEYCHAIN_URL: &str = "lattice://database-pull/gdrive/service-account";

actions!(
    database_pull,
    [
        /// Opens the Database Pull window: a dashboard of the last run plus a
        /// configuration editor for this project's sources and targets.
        OpenDatabasePull,
        /// Pulls the remote database configured for this project into the
        /// chosen target (local database, file, or Google Drive).
        PullDatabase,
        /// Cancels the database pull running for this project.
        CancelPull,
        /// Stores the local database password in the system keychain.
        SetLocalDatabasePassword,
        /// Stores the remote database password in the system keychain.
        SetRemoteDatabasePassword,
        /// Imports a Google service-account JSON key file into the system
        /// keychain for Google Drive targets.
        ImportGoogleDriveCredentials,
    ]
);

pub fn init(cx: &mut App) {
    pull_store::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(pull_database);
        workspace.register_action(|workspace, _: &OpenDatabasePull, window, cx| {
            pull_modal::DatabasePullModal::toggle(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &CancelPull, _window, cx| {
            let key = project_group_key(workspace, cx);
            DatabasePullStore::global(cx).update(cx, |store, cx| store.cancel(&key, cx));
        });
        workspace.register_action(|workspace, _: &SetLocalDatabasePassword, window, cx| {
            let key = project_group_key(workspace, cx);
            open_prompt(
                workspace,
                PromptKind::Password {
                    keychain_url: commands::keychain_url(&key, "local"),
                },
                "Local database password for this project",
                "Password (leave empty to remove)",
                true,
                window,
                cx,
            );
        });
        workspace.register_action(|workspace, _: &SetRemoteDatabasePassword, window, cx| {
            let key = project_group_key(workspace, cx);
            open_prompt(
                workspace,
                PromptKind::Password {
                    keychain_url: commands::keychain_url(&key, "remote-db"),
                },
                "Remote database password for this project",
                "Password (leave empty to remove)",
                true,
                window,
                cx,
            );
        });
        workspace.register_action(|workspace, _: &ImportGoogleDriveCredentials, window, cx| {
            open_prompt(
                workspace,
                PromptKind::GdriveImport,
                "Import Google Drive service-account credentials",
                "Path to the JSON key file (empty to remove stored credentials)",
                false,
                window,
                cx,
            );
        });
    })
    .detach();
}

fn project_group_key(workspace: &Workspace, cx: &App) -> ProjectGroupKey {
    ProjectGroupKey::from_project(workspace.project().read(cx), cx)
}

struct DatabasePullToast;

fn show_toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<DatabasePullToast>(), message),
        cx,
    );
}

#[derive(Clone)]
struct PendingPull {
    key: ProjectGroupKey,
    config: DatabasePullConfig,
    project_dir: Option<PathBuf>,
}

/// Starts the interactive pull flow (environment → source → target pickers).
/// Called from the `PullDatabase` action and from the dashboard's Pull button.
pub(crate) fn begin_pull(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    pull_database(workspace, &PullDatabase, window, cx);
}

fn pull_database(
    workspace: &mut Workspace,
    _: &PullDatabase,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let key = project_group_key(workspace, cx);
    let store = DatabasePullStore::global(cx);

    if store.read(cx).is_running(&key) {
        show_toast(
            workspace,
            "A database pull is already running for this project.".to_string(),
            cx,
        );
        return;
    }

    let Some(worktree) = project.read(cx).visible_worktrees(cx).next() else {
        show_toast(
            workspace,
            "Open a project folder to pull a database.".to_string(),
            cx,
        );
        return;
    };
    let settings_location = SettingsLocation {
        worktree_id: worktree.read(cx).id(),
        path: RelPath::empty(),
    };
    let project_dir = worktree.read(cx).abs_path().to_path_buf();

    let settings = DatabasePullSettings::get(Some(settings_location), cx);
    let Some(content) = settings.content.as_ref() else {
        show_toast(
            workspace,
            "Database Pull is not configured for this project. \
             Add a \"database_pull\" section to .zed/settings.json."
                .to_string(),
            cx,
        );
        return;
    };
    let config = match DatabasePullConfig::from_content(content) {
        Ok(config) => config,
        Err(error) => {
            workspace.show_error(&error, cx);
            return;
        }
    };

    let pending = PendingPull {
        key,
        config,
        project_dir: Some(project_dir),
    };
    let workspace_entity = cx.entity();
    // Deferred so the picker modals run outside this workspace update.
    window.defer(cx, move |window, cx| {
        choose_environment(workspace_entity, pending, window, cx);
    });
}

fn choose_environment(
    workspace: Entity<Workspace>,
    pending: PendingPull,
    window: &mut Window,
    cx: &mut App,
) {
    if pending.config.environments.len() > 1 {
        let items = pending
            .config
            .environments
            .iter()
            .map(|environment| environment.name.clone())
            .collect::<Vec<_>>();
        let picker_workspace = workspace.clone();
        workspace.update(cx, |workspace, cx| {
            pull_picker::pick(
                workspace,
                "Pull which environment?".into(),
                items,
                window,
                cx,
                move |index, window, cx| {
                    let Some(environment) = pending.config.environments.get(index).cloned() else {
                        return;
                    };
                    choose_source(picker_workspace, pending, environment, window, cx);
                },
            );
        });
    } else if let Some(environment) = pending.config.environments.first().cloned() {
        choose_source(workspace, pending, environment, window, cx);
    }
}

fn choose_source(
    workspace: Entity<Workspace>,
    pending: PendingPull,
    environment: PullEnvironment,
    window: &mut Window,
    cx: &mut App,
) {
    let sources = environment.sources();
    if sources.len() > 1 {
        let items = sources
            .iter()
            .map(|source| source.name().clone())
            .collect::<Vec<_>>();
        let picker_workspace = workspace.clone();
        workspace.update(cx, |workspace, cx| {
            pull_picker::pick(
                workspace,
                "Pull from…".into(),
                items,
                window,
                cx,
                move |index, window, cx| {
                    let Some(source) = sources.get(index).cloned() else {
                        return;
                    };
                    choose_target(picker_workspace, pending, environment, source, window, cx);
                },
            );
        });
    } else if let Some(source) = sources.into_iter().next() {
        choose_target(workspace, pending, environment, source, window, cx);
    }
}

fn choose_target(
    workspace: Entity<Workspace>,
    pending: PendingPull,
    environment: PullEnvironment,
    source: PullSource,
    window: &mut Window,
    cx: &mut App,
) {
    let targets = environment.available_targets(&pending.config.shared_targets);

    if targets.is_empty() {
        let name = environment.name;
        workspace.update(cx, |workspace, cx| {
            show_toast(
                workspace,
                format!("No targets are configured for environment \"{name}\"."),
                cx,
            );
        });
        return;
    }

    if targets.len() > 1 {
        let items = targets
            .iter()
            .map(|target| target.name().clone())
            .collect::<Vec<_>>();
        let picker_workspace = workspace.clone();
        let environment_name = environment.name;
        workspace.update(cx, |workspace, cx| {
            pull_picker::pick(
                workspace,
                "Pull to…".into(),
                items,
                window,
                cx,
                move |index, window, cx| {
                    let Some(target) = targets.get(index).cloned() else {
                        return;
                    };
                    confirm_and_start(
                        picker_workspace,
                        pending,
                        environment_name.clone(),
                        source,
                        target,
                        window,
                        cx,
                    );
                },
            );
        });
    } else if let Some(target) = targets.into_iter().next() {
        confirm_and_start(workspace, pending, environment.name, source, target, window, cx);
    }
}

fn confirm_and_start(
    workspace: Entity<Workspace>,
    pending: PendingPull,
    environment: SharedString,
    source: PullSource,
    target: PullTarget,
    window: &mut Window,
    cx: &mut App,
) {
    let store = DatabasePullStore::global(cx);
    // Only database targets are destructive; file and Drive targets only
    // create new files.
    let confirmation = if let PullTarget::Database(local) = &target {
        Some(window.prompt(
            PromptLevel::Warning,
            &format!("Replace local database \"{}\"?", local.database),
            Some(&format!(
                "This drops and re-imports \"{}\" on {}:{} with data pulled from {}.",
                local.database, local.host, local.port, source.ssh().host
            )),
            &["Replace Database", "Cancel"],
            cx,
        ))
    } else {
        None
    };

    let workspace = workspace.downgrade();
    window
        .spawn(cx, async move |cx| {
            if let Some(confirmation) = confirmation
                && confirmation.await != Ok(0)
            {
                return anyhow::Ok(());
            }
            let started_at = Instant::now();
            let key = pending.key.clone();
            let start_result = store.update(cx, |store, cx| {
                store.start_pull(
                    key.clone(),
                    pending.config,
                    environment,
                    source,
                    target,
                    pending.project_dir,
                    cx,
                )
            });
            if let Err(error) = start_result {
                workspace.update(cx, |workspace, cx| workspace.show_error(&error, cx))?;
                return anyhow::Ok(());
            }

            // Watch the pull and surface the outcome as a workspace notification.
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let stage = store.read_with(cx, |store, _| {
                    store.state(&key).map(|state| state.stage.clone())
                });
                match stage {
                    None | Some(PullStage::Cancelled) => return anyhow::Ok(()),
                    Some(PullStage::Done { message }) => {
                        workspace.update(cx, |workspace, cx| {
                            show_toast(
                                workspace,
                                format!("{message} ({})", format_duration(started_at.elapsed())),
                                cx,
                            );
                        })?;
                        return anyhow::Ok(());
                    }
                    Some(PullStage::Failed { .. }) => {
                        let error = store.read_with(cx, |store, _| {
                            store.state(&key).and_then(|state| state.full_error())
                        });
                        if let Some(error) = error {
                            workspace
                                .update(cx, |workspace, cx| workspace.show_error(&error, cx))?;
                        }
                        return anyhow::Ok(());
                    }
                    Some(_) => {}
                }
            }
        })
        .detach_and_log_err(cx);
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

enum PromptKind {
    /// Store/delete a password at a keychain URL.
    Password { keychain_url: String },
    /// Read a service-account JSON file and store it in the keychain.
    GdriveImport,
}

fn open_prompt(
    workspace: &mut Workspace,
    kind: PromptKind,
    label: &'static str,
    placeholder: &'static str,
    masked: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| {
        InputPrompt::new(kind, label.into(), placeholder, masked, workspace_handle, window, cx)
    });
}

/// A one-field modal used for keychain passwords and the Google Drive
/// credentials import. Empty submissions delete the stored value.
struct InputPrompt {
    kind: PromptKind,
    label: SharedString,
    editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
}

impl InputPrompt {
    fn new(
        kind: PromptKind,
        label: SharedString,
        placeholder: &'static str,
        masked: bool,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor.set_masked(masked, cx);
            editor
        });
        Self {
            kind,
            label,
            editor,
            workspace,
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let input = self.editor.read(cx).text(cx);
        match &self.kind {
            PromptKind::Password { keychain_url } => {
                let url = keychain_url.clone();
                let task = if input.is_empty() {
                    cx.delete_credentials(&url)
                } else {
                    cx.write_credentials(&url, "database", input.as_bytes())
                };
                cx.spawn(async move |_, _| {
                    task.await
                        .with_context(|| format!("failed to update keychain entry {url}"))
                        .log_err();
                })
                .detach();
            }
            PromptKind::GdriveImport => {
                let workspace = self.workspace.clone();
                cx.spawn(async move |_, cx| {
                    let result = if input.is_empty() {
                        let task = cx.update(|cx| cx.delete_credentials(GDRIVE_KEYCHAIN_URL));
                        task.await
                            .context("removing Google Drive credentials")
                            .map(|_| "Google Drive credentials removed".to_string())
                    } else {
                        async {
                            let path =
                                PathBuf::from(shellexpand::tilde(input.trim()).into_owned());
                            let bytes = smol::fs::read(&path)
                                .await
                                .with_context(|| format!("reading {path:?}"))?;
                            gdrive::validate_service_account_json(&bytes)?;
                            cx.update(|cx| {
                                cx.write_credentials(
                                    GDRIVE_KEYCHAIN_URL,
                                    "service-account",
                                    &bytes,
                                )
                            })
                            .await
                            .context("writing to the keychain")?;
                            anyhow::Ok(
                                "Google Drive credentials imported — you can delete the JSON file"
                                    .to_string(),
                            )
                        }
                        .await
                    };
                    workspace
                        .update(cx, |workspace, cx| match result {
                            Ok(message) => show_toast(workspace, message, cx),
                            Err(error) => workspace.show_error(&error, cx),
                        })
                        .ok();
                })
                .detach();
            }
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl ModalView for InputPrompt {}

impl EventEmitter<DismissEvent> for InputPrompt {}

impl Focusable for InputPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for InputPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PasswordPrompt")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_3(cx)
            .w(rems(34.))
            .p_4()
            .gap_2()
            .child(Label::new(self.label.clone()))
            .child(self.editor.clone())
            .child(
                Label::new("Enter to save · Esc to cancel")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}
