//! In-memory, editable model of the `database_pull` project settings, backing
//! the configuration editor in [`crate::pull_modal`]. Each text field is an
//! [`Entity<InputField>`] so it keeps its state across re-renders; the modal
//! owns the rendering and the add/remove handlers, mutating this model in place.

use gpui::{App, Entity, Window};
use settings::{
    DatabasePullEnvironmentContent, DatabasePullSettingsContent, DatabasePullSshContent,
    DatabasePullTargetContent, PostImportStepContent, RemoteDatabaseCredentialsContent,
};
use ui::prelude::*;
use ui_input::InputField;

fn field(placeholder: &str, text: Option<&str>, window: &mut Window, cx: &mut App) -> Entity<InputField> {
    cx.new(|cx| {
        let input = InputField::new(window, cx, placeholder);
        if let Some(text) = text
            && !text.is_empty()
        {
            input.set_text(text, window, cx);
        }
        input
    })
}

fn labeled(
    label: impl Into<SharedString>,
    placeholder: &str,
    text: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputField> {
    cx.new(|cx| {
        let input = InputField::new(window, cx, placeholder).label(label);
        if let Some(text) = text
            && !text.is_empty()
        {
            input.set_text(text, window, cx);
        }
        input
    })
}

/// A single empty input for a new `exclude_table_data` row.
pub fn exclude_input(window: &mut Window, cx: &mut App) -> Entity<InputField> {
    field("wp_actionscheduler_logs", None, window, cx)
}

fn read(input: &Entity<InputField>, cx: &App) -> String {
    input.read(cx).text(cx)
}

/// Reads an input, returning `None` when it is empty (for optional fields).
fn read_opt(input: &Entity<InputField>, cx: &App) -> Option<String> {
    let text = read(input, cx);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Splits a whitespace-separated list of shell arguments. Arguments in this
/// setting (e.g. `--column-statistics=0`) do not contain spaces, so a single
/// input field is sufficient.
fn split_args(input: &Entity<InputField>, cx: &App) -> Vec<String> {
    read(input, cx)
        .split_whitespace()
        .map(|arg| arg.to_string())
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CredentialsKind {
    ServerDefault,
    Keychain,
    Env,
}

impl CredentialsKind {
    pub const ALL: [CredentialsKind; 3] = [Self::ServerDefault, Self::Keychain, Self::Env];

    pub fn label(self) -> &'static str {
        match self {
            Self::ServerDefault => "Server default (~/.my.cnf)",
            Self::Keychain => "Keychain",
            Self::Env => "Environment variables",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Database,
    File,
    Gdrive,
    Rclone,
}

impl TargetKind {
    pub const ALL: [TargetKind; 4] = [Self::Database, Self::File, Self::Gdrive, Self::Rclone];

    pub fn label(self) -> &'static str {
        match self {
            Self::Database => "Local database (import)",
            Self::File => "Download to file",
            Self::Gdrive => "Google Drive",
            Self::Rclone => "rclone",
        }
    }
}

pub struct ConfigForm {
    pub environments: Vec<EnvironmentForm>,
    pub shared_targets: Vec<TargetForm>,
    pub exclude_table_data: Vec<Entity<InputField>>,
}

pub struct EnvironmentForm {
    pub name: Entity<InputField>,
    pub ssh_host: Entity<InputField>,
    pub ssh_username: Entity<InputField>,
    pub ssh_port: Entity<InputField>,
    pub ssh_args: Entity<InputField>,
    pub database: Entity<InputField>,
    pub backup_glob: Entity<InputField>,
    pub credentials: CredentialsForm,
    pub mysqldump_args: Entity<InputField>,
    pub targets: Vec<TargetForm>,
}

pub struct CredentialsForm {
    pub kind: CredentialsKind,
    pub keychain_username: Entity<InputField>,
    pub env_source: Entity<InputField>,
    pub env_user_var: Entity<InputField>,
    pub env_password_var: Entity<InputField>,
    pub env_host_var: Entity<InputField>,
}

pub struct TargetForm {
    pub kind: TargetKind,
    pub name: Entity<InputField>,
    pub db_host: Entity<InputField>,
    pub db_port: Entity<InputField>,
    pub db_username: Entity<InputField>,
    pub db_password: Entity<InputField>,
    pub db_database: Entity<InputField>,
    pub post_import: Vec<PostImportForm>,
    pub file_path: Entity<InputField>,
    pub gdrive_folder: Entity<InputField>,
    pub gdrive_auth: Entity<InputField>,
    pub rclone_dest: Entity<InputField>,
    pub rclone_path: Entity<InputField>,
}

pub struct PostImportForm {
    pub name: Entity<InputField>,
    pub command: Entity<InputField>,
}

impl ConfigForm {
    /// Builds the form from the current settings. When there is no config yet,
    /// starts with a single empty environment containing one database target so
    /// the shape is visible.
    pub fn from_content(
        content: Option<&DatabasePullSettingsContent>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let Some(content) = content else {
            let mut environment = EnvironmentForm::empty(window, cx);
            environment.targets.push(TargetForm::empty(window, cx));
            return Self {
                environments: vec![environment],
                shared_targets: Vec::new(),
                exclude_table_data: Vec::new(),
            };
        };

        let environments = content
            .environments
            .iter()
            .flatten()
            .map(|environment| EnvironmentForm::from_content(environment, window, cx))
            .collect();
        let shared_targets = content
            .shared_targets
            .iter()
            .flatten()
            .map(|target| TargetForm::from_content(target, window, cx))
            .collect();
        let exclude_table_data = content
            .exclude_table_data
            .iter()
            .flatten()
            .map(|table| field("wp_actionscheduler_logs", Some(table), window, cx))
            .collect();

        Self {
            environments,
            shared_targets,
            exclude_table_data,
        }
    }

    pub fn to_content(&self, cx: &App) -> DatabasePullSettingsContent {
        let environments = self
            .environments
            .iter()
            .map(|environment| environment.to_content(cx))
            .collect::<Vec<_>>();
        let shared_targets = self
            .shared_targets
            .iter()
            .map(|target| target.to_content(cx))
            .collect::<Vec<_>>();
        let exclude_table_data = self
            .exclude_table_data
            .iter()
            .filter_map(|input| read_opt(input, cx))
            .collect::<Vec<_>>();

        DatabasePullSettingsContent {
            environments: (!environments.is_empty()).then_some(environments),
            shared_targets: (!shared_targets.is_empty()).then_some(shared_targets),
            exclude_table_data: (!exclude_table_data.is_empty()).then_some(exclude_table_data),
        }
    }
}

impl EnvironmentForm {
    pub fn empty(window: &mut Window, cx: &mut App) -> Self {
        Self::build(None, window, cx)
    }

    fn from_content(
        content: &DatabasePullEnvironmentContent,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self::build(Some(content), window, cx)
    }

    fn build(
        content: Option<&DatabasePullEnvironmentContent>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let ssh = content.map(|content| &content.ssh);
        let port = content
            .and_then(|content| content.ssh.port)
            .map(|port| port.to_string());
        let args = content
            .map(|content| content.mysqldump_args.join(" "))
            .unwrap_or_default();
        let ssh_args = ssh.map(|ssh| ssh.args.join(" ")).unwrap_or_default();

        Self {
            name: labeled(
                "Name",
                "NZ",
                content.map(|content| content.name.as_str()),
                window,
                cx,
            ),
            ssh_host: labeled(
                "SSH host",
                "my-server",
                ssh.map(|ssh| ssh.host.as_str()),
                window,
                cx,
            ),
            ssh_username: labeled(
                "SSH user (optional)",
                "deploy",
                ssh.and_then(|ssh| ssh.username.as_deref()),
                window,
                cx,
            ),
            ssh_port: labeled("SSH port (optional)", "22", port.as_deref(), window, cx),
            ssh_args: labeled(
                "SSH args (optional)",
                "-o StrictHostKeyChecking=no",
                Some(&ssh_args),
                window,
                cx,
            ),
            database: labeled(
                "Remote database (enables live dump)",
                "app_live",
                content.and_then(|content| content.database.as_deref()),
                window,
                cx,
            ),
            backup_glob: labeled(
                "Backup glob (enables backup file)",
                "/var/backups/mysql/app_*.sql.gz",
                content.and_then(|content| content.backup_glob.as_deref()),
                window,
                cx,
            ),
            credentials: CredentialsForm::build(
                content.and_then(|content| content.credentials.clone()),
                window,
                cx,
            ),
            mysqldump_args: labeled(
                "mysqldump args (optional)",
                "--column-statistics=0",
                Some(&args),
                window,
                cx,
            ),
            targets: content
                .map(|content| {
                    content
                        .targets
                        .iter()
                        .flatten()
                        .map(|target| TargetForm::from_content(target, window, cx))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn to_content(&self, cx: &App) -> DatabasePullEnvironmentContent {
        let targets = self
            .targets
            .iter()
            .map(|target| target.to_content(cx))
            .collect::<Vec<_>>();

        DatabasePullEnvironmentContent {
            name: read(&self.name, cx),
            ssh: DatabasePullSshContent {
                host: read(&self.ssh_host, cx),
                username: read_opt(&self.ssh_username, cx),
                port: read_opt(&self.ssh_port, cx).and_then(|port| port.parse().ok()),
                args: split_args(&self.ssh_args, cx),
            },
            database: read_opt(&self.database, cx),
            credentials: self.credentials.to_content(cx),
            mysqldump_args: split_args(&self.mysqldump_args, cx),
            backup_glob: read_opt(&self.backup_glob, cx),
            targets: (!targets.is_empty()).then_some(targets),
        }
    }
}

impl CredentialsForm {
    fn build(
        content: Option<RemoteDatabaseCredentialsContent>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let (kind, keychain_username, env_source, env_user, env_password, env_host) = match content {
            Some(RemoteDatabaseCredentialsContent::Keychain { username }) => (
                CredentialsKind::Keychain,
                Some(username),
                None,
                None,
                None,
                None,
            ),
            Some(RemoteDatabaseCredentialsContent::Env {
                source,
                user_var,
                password_var,
                host_var,
            }) => (
                CredentialsKind::Env,
                None,
                source,
                user_var,
                password_var,
                host_var,
            ),
            _ => (CredentialsKind::ServerDefault, None, None, None, None, None),
        };

        Self {
            kind,
            keychain_username: labeled(
                "Keychain username",
                "root",
                keychain_username.as_deref(),
                window,
                cx,
            ),
            env_source: labeled(
                "Source file (optional)",
                "/container/application/current/.env",
                env_source.as_deref(),
                window,
                cx,
            ),
            env_user_var: labeled("User var", "DB_USERNAME", env_user.as_deref(), window, cx),
            env_password_var: labeled(
                "Password var",
                "DB_PASSWORD",
                env_password.as_deref(),
                window,
                cx,
            ),
            env_host_var: labeled("Host var", "DB_HOST", env_host.as_deref(), window, cx),
        }
    }

    fn to_content(&self, cx: &App) -> Option<RemoteDatabaseCredentialsContent> {
        match self.kind {
            // Omitted from the file; defaults to server_default.
            CredentialsKind::ServerDefault => None,
            CredentialsKind::Keychain => Some(RemoteDatabaseCredentialsContent::Keychain {
                username: read(&self.keychain_username, cx),
            }),
            CredentialsKind::Env => Some(RemoteDatabaseCredentialsContent::Env {
                source: read_opt(&self.env_source, cx),
                user_var: read_opt(&self.env_user_var, cx),
                password_var: read_opt(&self.env_password_var, cx),
                host_var: read_opt(&self.env_host_var, cx),
            }),
        }
    }
}

impl TargetForm {
    pub fn empty(window: &mut Window, cx: &mut App) -> Self {
        Self::build(None, window, cx)
    }

    fn from_content(
        content: &DatabasePullTargetContent,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self::build(Some(content), window, cx)
    }

    fn build(
        content: Option<&DatabasePullTargetContent>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        // Pull out per-kind values, defaulting to empty for the fields that do
        // not apply to the current kind.
        let mut kind = TargetKind::Database;
        let mut name = None;
        let mut db_host = None;
        let mut db_port = None;
        let mut db_username = None;
        let mut db_password = None;
        let mut db_database = None;
        let mut post_import_content: &[PostImportStepContent] = &[];
        let mut file_path = None;
        let mut gdrive_folder = None;
        let mut gdrive_auth = None;
        let mut rclone_dest = None;
        let mut rclone_path = None;

        match content {
            Some(DatabasePullTargetContent::Database {
                name: n,
                host,
                port,
                username,
                password,
                database,
                post_import,
            }) => {
                kind = TargetKind::Database;
                name = n.clone();
                db_host = host.clone();
                db_port = port.map(|port| port.to_string());
                db_username = username.clone();
                db_password = password.clone();
                db_database = Some(database.clone());
                post_import_content = post_import.as_deref().unwrap_or(&[]);
            }
            Some(DatabasePullTargetContent::File { name: n, path }) => {
                kind = TargetKind::File;
                name = n.clone();
                file_path = Some(path.clone());
            }
            Some(DatabasePullTargetContent::Gdrive {
                name: n,
                folder,
                auth,
            }) => {
                kind = TargetKind::Gdrive;
                name = n.clone();
                gdrive_folder = Some(folder.clone());
                gdrive_auth = Some(auth.clone());
            }
            Some(DatabasePullTargetContent::Rclone {
                name: n,
                dest,
                rclone_path: path,
            }) => {
                kind = TargetKind::Rclone;
                name = n.clone();
                rclone_dest = Some(dest.clone());
                rclone_path = path.clone();
            }
            None => {}
        }

        let post_import = post_import_content
            .iter()
            .map(|step| PostImportForm {
                name: labeled(
                    "Step name (optional)",
                    "Search-replace domain",
                    step.name.as_deref(),
                    window,
                    cx,
                ),
                command: labeled("Command", "wp search-replace …", Some(&step.command), window, cx),
            })
            .collect();

        Self {
            kind,
            name: labeled("Name (optional)", "Local DB", name.as_deref(), window, cx),
            db_host: labeled("Host", "127.0.0.1", db_host.as_deref(), window, cx),
            db_port: labeled("Port", "3306", db_port.as_deref(), window, cx),
            db_username: labeled("Username", "root", db_username.as_deref(), window, cx),
            db_password: cx.new(|cx| {
                let input = InputField::new(window, cx, "root")
                    .label("Password (optional)")
                    .masked(true);
                if let Some(password) = db_password.as_deref()
                    && !password.is_empty()
                {
                    input.set_text(password, window, cx);
                }
                input
            }),
            db_database: labeled("Local database", "app", db_database.as_deref(), window, cx),
            post_import,
            file_path: labeled(
                "Path (directory or .sql.gz file)",
                "~/Backups/app",
                file_path.as_deref(),
                window,
                cx,
            ),
            gdrive_folder: labeled(
                "Drive folder",
                "Backups/App",
                gdrive_folder.as_deref(),
                window,
                cx,
            ),
            gdrive_auth: labeled(
                "Auth (\"keychain\" or path to JSON)",
                "keychain",
                gdrive_auth.as_deref(),
                window,
                cx,
            ),
            rclone_dest: labeled(
                "Destination",
                "remote:Backups/App/",
                rclone_dest.as_deref(),
                window,
                cx,
            ),
            rclone_path: labeled(
                "rclone path (optional)",
                "rclone",
                rclone_path.as_deref(),
                window,
                cx,
            ),
        }
    }

    fn to_content(&self, cx: &App) -> DatabasePullTargetContent {
        let name = read_opt(&self.name, cx);
        match self.kind {
            TargetKind::Database => {
                let post_import = self
                    .post_import
                    .iter()
                    .map(|step| PostImportStepContent {
                        name: read_opt(&step.name, cx),
                        command: read(&step.command, cx),
                    })
                    .collect::<Vec<_>>();
                DatabasePullTargetContent::Database {
                    name,
                    host: read_opt(&self.db_host, cx),
                    port: read_opt(&self.db_port, cx).and_then(|port| port.parse().ok()),
                    username: read_opt(&self.db_username, cx),
                    password: read_opt(&self.db_password, cx),
                    database: read(&self.db_database, cx),
                    post_import: (!post_import.is_empty()).then_some(post_import),
                }
            }
            TargetKind::File => DatabasePullTargetContent::File {
                name,
                path: read(&self.file_path, cx),
            },
            TargetKind::Gdrive => DatabasePullTargetContent::Gdrive {
                name,
                folder: read(&self.gdrive_folder, cx),
                auth: read(&self.gdrive_auth, cx),
            },
            TargetKind::Rclone => DatabasePullTargetContent::Rclone {
                name,
                dest: read(&self.rclone_dest, cx),
                rclone_path: read_opt(&self.rclone_path, cx),
            },
        }
    }
}

impl PostImportForm {
    pub fn empty(window: &mut Window, cx: &mut App) -> Self {
        Self {
            name: labeled("Step name (optional)", "Search-replace domain", None, window, cx),
            command: labeled("Command", "wp search-replace …", None, window, cx),
        }
    }
}
