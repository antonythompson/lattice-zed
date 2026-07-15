use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use gpui::SharedString;
use settings::{
    DatabasePullEnvironmentContent, DatabasePullSettingsContent, DatabasePullSshContent,
    DatabasePullTargetContent, PostImportStepContent, RegisterSetting,
    RemoteDatabaseCredentialsContent, Settings,
};

/// The raw per-project `database_pull` settings. `content` is `None` when the
/// project has no `database_pull` section; validation into a usable
/// [`DatabasePullConfig`] happens at pull time so errors can be specific.
#[derive(Debug, Clone, RegisterSetting)]
pub struct DatabasePullSettings {
    pub content: Option<DatabasePullSettingsContent>,
}

impl Settings for DatabasePullSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            content: content.project.database_pull.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DatabasePullConfig {
    pub environments: Vec<PullEnvironment>,
    pub shared_targets: Vec<PullTarget>,
    pub exclude_table_data: Vec<String>,
}

/// A remote environment to pull from. Its sources (live dump / backup file)
/// are derived from `database` and `backup_glob`.
#[derive(Debug, Clone, PartialEq)]
pub struct PullEnvironment {
    pub name: SharedString,
    pub ssh: SshTarget,
    pub database: Option<String>,
    pub credentials: RemoteDatabaseCredentials,
    pub mysqldump_args: Vec<String>,
    pub backup_glob: Option<String>,
    pub targets: Vec<PullTarget>,
}

impl PullEnvironment {
    /// The sources this environment offers: a backup-file source when
    /// `backup_glob` is set, and a live-dump source when `database` is set.
    /// Backup is listed first as the cheaper, non-disruptive default.
    pub fn sources(&self) -> Vec<PullSource> {
        let mut sources = Vec::new();
        if let Some(path_glob) = &self.backup_glob {
            sources.push(PullSource::BackupFile {
                name: "Nightly backup".into(),
                ssh: self.ssh.clone(),
                path_glob: path_glob.clone(),
            });
        }
        if let Some(database) = &self.database {
            sources.push(PullSource::Dump {
                name: "Live dump".into(),
                ssh: self.ssh.clone(),
                database: database.clone(),
                credentials: self.credentials.clone(),
                mysqldump_args: self.mysqldump_args.clone(),
            });
        }
        sources
    }

    /// The targets offered when pulling this environment: its own targets
    /// followed by the project's shared targets.
    pub fn available_targets(&self, shared: &[PullTarget]) -> Vec<PullTarget> {
        self.targets
            .iter()
            .cloned()
            .chain(shared.iter().cloned())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SshTarget {
    pub host: String,
    pub username: Option<String>,
    pub port: Option<u16>,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PullSource {
    Dump {
        name: SharedString,
        ssh: SshTarget,
        database: String,
        credentials: RemoteDatabaseCredentials,
        mysqldump_args: Vec<String>,
    },
    BackupFile {
        name: SharedString,
        ssh: SshTarget,
        path_glob: String,
    },
}

impl PullSource {
    pub fn name(&self) -> &SharedString {
        match self {
            Self::Dump { name, .. } | Self::BackupFile { name, .. } => name,
        }
    }

    pub fn ssh(&self) -> &SshTarget {
        match self {
            Self::Dump { ssh, .. } | Self::BackupFile { ssh, .. } => ssh,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RemoteDatabaseCredentials {
    ServerDefault,
    Keychain {
        username: String,
    },
    Env {
        source: Option<String>,
        user_var: String,
        password_var: String,
        host_var: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PullTarget {
    Database(LocalDatabase),
    File {
        name: SharedString,
        path: PathBuf,
    },
    GoogleDrive {
        name: SharedString,
        folder: String,
        auth: GoogleDriveAuth,
    },
    Rclone {
        name: SharedString,
        dest: String,
        rclone_path: String,
    },
}

impl PullTarget {
    pub fn name(&self) -> &SharedString {
        match self {
            Self::Database(local) => &local.name,
            Self::File { name, .. }
            | Self::GoogleDrive { name, .. }
            | Self::Rclone { name, .. } => name,
        }
    }

    pub fn is_database(&self) -> bool {
        matches!(self, Self::Database(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GoogleDriveAuth {
    /// Credentials imported via `database_pull::ImportGoogleDriveCredentials`.
    Keychain,
    /// Path to a service-account JSON key file.
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalDatabase {
    pub name: SharedString,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub database: String,
    pub post_import: Vec<PostImportStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PostImportStep {
    pub name: SharedString,
    pub command: String,
}

fn validate_identifier(value: &str, what: &str) -> Result<()> {
    anyhow::ensure!(!value.is_empty(), "{what} must not be empty");
    anyhow::ensure!(
        !value.contains('`') && !value.contains('\'') && !value.contains('\n'),
        "{what} contains invalid characters: {value:?}"
    );
    Ok(())
}

fn expand_path(value: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(value).into_owned())
}

/// Env var names are interpolated unquoted into the remote shell script, so
/// they must be plain identifiers to avoid injection.
fn validate_env_var_name(value: &str, what: &str) -> Result<()> {
    anyhow::ensure!(!value.is_empty(), "`database_pull` env {what} must not be empty");
    let valid = value
        .bytes()
        .enumerate()
        .all(|(index, byte)| byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit()));
    anyhow::ensure!(
        valid,
        "`database_pull` env {what} must be a plain variable name: {value:?}"
    );
    Ok(())
}

fn ssh_from_content(content: &DatabasePullSshContent) -> Result<SshTarget> {
    anyhow::ensure!(!content.host.is_empty(), "`database_pull` ssh host must not be empty");
    Ok(SshTarget {
        host: content.host.clone(),
        username: content.username.clone(),
        port: content.port,
        args: content.args.clone(),
    })
}

impl DatabasePullConfig {
    pub fn from_content(content: &DatabasePullSettingsContent) -> Result<Self> {
        let environments = content
            .environments
            .iter()
            .flatten()
            .map(PullEnvironment::from_content)
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(
            !environments.is_empty(),
            "`database_pull.environments` must contain at least one environment"
        );

        let mut seen = HashSet::new();
        for environment in &environments {
            anyhow::ensure!(
                seen.insert(environment.name.clone()),
                "duplicate `database_pull` environment name: {:?}",
                environment.name
            );
        }

        for table in content.exclude_table_data.iter().flatten() {
            validate_identifier(table, "`database_pull.exclude_table_data` entry")?;
        }

        let shared_targets = content
            .shared_targets
            .iter()
            .flatten()
            .map(PullTarget::from_content)
            .collect::<Result<Vec<_>>>()?;

        for environment in &environments {
            anyhow::ensure!(
                !environment.targets.is_empty() || !shared_targets.is_empty(),
                "environment {:?} has no targets — add one to the environment or to `shared_targets`",
                environment.name
            );
        }

        Ok(Self {
            environments,
            shared_targets,
            exclude_table_data: content.exclude_table_data.clone().unwrap_or_default(),
        })
    }
}

impl PullEnvironment {
    fn from_content(content: &DatabasePullEnvironmentContent) -> Result<Self> {
        anyhow::ensure!(
            !content.name.trim().is_empty(),
            "each `database_pull` environment must have a non-empty `name`"
        );
        let ssh = ssh_from_content(&content.ssh)?;
        if let Some(database) = &content.database {
            validate_identifier(database, "`database_pull` environment database")?;
        }
        if let Some(glob) = &content.backup_glob {
            anyhow::ensure!(
                !glob.is_empty(),
                "environment {:?} `backup_glob` must not be empty",
                content.name
            );
        }
        anyhow::ensure!(
            content.database.is_some() || content.backup_glob.is_some(),
            "environment {:?} needs a `database` (for a live dump) or a `backup_glob` \
             (for a backup file)",
            content.name
        );
        let credentials = parse_credentials(content.credentials.clone())?;
        let targets = content
            .targets
            .iter()
            .flatten()
            .map(PullTarget::from_content)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            name: content.name.clone().into(),
            ssh,
            database: content.database.clone(),
            credentials,
            mysqldump_args: content.mysqldump_args.clone(),
            backup_glob: content.backup_glob.clone(),
            targets,
        })
    }
}

fn parse_post_import(steps: &Option<Vec<PostImportStepContent>>) -> Result<Vec<PostImportStep>> {
    steps
        .iter()
        .flatten()
        .map(|step| {
            if step.command.trim().is_empty() {
                return Err(anyhow!("`database_pull` post_import command is empty"));
            }
            Ok(PostImportStep {
                name: step
                    .name
                    .clone()
                    .unwrap_or_else(|| step.command.clone())
                    .into(),
                command: step.command.clone(),
            })
        })
        .collect()
}

/// Parses a remote environment's mysqldump credentials, applying defaults and
/// validating the env-var names (which are interpolated into a remote shell
/// script).
fn parse_credentials(
    content: Option<RemoteDatabaseCredentialsContent>,
) -> Result<RemoteDatabaseCredentials> {
    Ok(
        match content.unwrap_or(RemoteDatabaseCredentialsContent::ServerDefault) {
            RemoteDatabaseCredentialsContent::ServerDefault => {
                RemoteDatabaseCredentials::ServerDefault
            }
            RemoteDatabaseCredentialsContent::Keychain { username } => {
                validate_identifier(&username, "`database_pull` credentials keychain username")?;
                RemoteDatabaseCredentials::Keychain { username }
            }
            RemoteDatabaseCredentialsContent::Env {
                source,
                user_var,
                password_var,
                host_var,
            } => {
                let user_var = user_var.unwrap_or_else(|| "DB_USERNAME".into());
                let password_var = password_var.unwrap_or_else(|| "DB_PASSWORD".into());
                let host_var = host_var.unwrap_or_else(|| "DB_HOST".into());
                for (var, what) in [
                    (&user_var, "user_var"),
                    (&password_var, "password_var"),
                    (&host_var, "host_var"),
                ] {
                    validate_env_var_name(var, what)?;
                }
                RemoteDatabaseCredentials::Env {
                    source,
                    user_var,
                    password_var,
                    host_var,
                }
            }
        },
    )
}

impl PullTarget {
    fn from_content(content: &DatabasePullTargetContent) -> Result<Self> {
        match content {
            DatabasePullTargetContent::Database {
                name,
                host,
                port,
                username,
                password,
                database,
                post_import,
            } => {
                validate_identifier(database, "`database_pull` database target name")?;
                Ok(Self::Database(LocalDatabase {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("Local DB ({database})"))
                        .into(),
                    host: host.clone().unwrap_or_else(|| "127.0.0.1".into()),
                    port: port.unwrap_or(3306),
                    username: username.clone().unwrap_or_else(|| "root".into()),
                    password: password.clone(),
                    database: database.clone(),
                    post_import: parse_post_import(post_import)?,
                }))
            }
            DatabasePullTargetContent::File { name, path } => {
                anyhow::ensure!(
                    !path.is_empty(),
                    "`database_pull` file target path must not be empty"
                );
                Ok(Self::File {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("File ({path})"))
                        .into(),
                    path: expand_path(path),
                })
            }
            DatabasePullTargetContent::Gdrive {
                name,
                folder,
                auth,
            } => {
                anyhow::ensure!(
                    !folder.is_empty(),
                    "`database_pull` gdrive target folder must not be empty"
                );
                anyhow::ensure!(
                    !auth.is_empty(),
                    "`database_pull` gdrive target auth must not be empty"
                );
                let auth = if auth == "keychain" {
                    GoogleDriveAuth::Keychain
                } else {
                    GoogleDriveAuth::File(expand_path(auth))
                };
                Ok(Self::GoogleDrive {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("Google Drive ({folder})"))
                        .into(),
                    folder: folder.clone(),
                    auth,
                })
            }
            DatabasePullTargetContent::Rclone {
                name,
                dest,
                rclone_path,
            } => {
                anyhow::ensure!(
                    !dest.is_empty(),
                    "`database_pull` rclone target dest must not be empty"
                );
                Ok(Self::Rclone {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("rclone ({dest})"))
                        .into(),
                    dest: dest.clone(),
                    rclone_path: rclone_path
                        .clone()
                        .unwrap_or_else(|| "rclone".to_string()),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_example_json() {
        let json = r#"{
            "environments": [
                {
                    "name": "prod",
                    "ssh": { "host": "web1.example.com", "username": "deploy", "port": 22 },
                    "database": "example_live",
                    "credentials": "server_default",
                    "backup_glob": "/var/backups/mysql/example_*.sql.gz",
                    "targets": [
                        { "type": "database", "database": "example_test", "password": "root",
                          "post_import": [
                            { "name": "Search-replace URLs",
                              "command": "wp search-replace https://example.com https://example.test --all-tables" }
                          ] },
                        { "type": "gdrive", "folder": "Backups/Example", "auth": "keychain" }
                    ]
                }
            ],
            "shared_targets": [
                { "type": "file", "path": "~/Downloads" }
            ],
            "exclude_table_data": ["wp_actionscheduler_logs"]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("example settings should deserialize");
        let config =
            DatabasePullConfig::from_content(&content).expect("example settings should validate");

        assert_eq!(config.environments.len(), 1);
        let environment = &config.environments[0];
        assert_eq!(environment.name, SharedString::from("prod"));
        assert_eq!(environment.ssh.host, "web1.example.com");

        // Both a backup file and a live dump are offered, backup first.
        let sources = environment.sources();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].name(), &SharedString::from("Nightly backup"));
        assert!(matches!(sources[0], PullSource::BackupFile { .. }));
        assert_eq!(sources[1].name(), &SharedString::from("Live dump"));
        assert!(matches!(sources[1], PullSource::Dump { .. }));
        assert_eq!(sources[0].ssh().host, "web1.example.com");

        // Available targets = the environment's own, then the shared ones.
        let targets = environment.available_targets(&config.shared_targets);
        assert_eq!(targets.len(), 3);

        let PullTarget::Database(local) = &targets[0] else {
            panic!("expected database target");
        };
        assert_eq!(local.host, "127.0.0.1");
        assert_eq!(local.port, 3306);
        assert_eq!(local.username, "root");
        assert_eq!(local.password.as_deref(), Some("root"));
        assert_eq!(local.database, "example_test");
        assert_eq!(local.post_import.len(), 1);

        let PullTarget::GoogleDrive { folder, auth, .. } = &targets[1] else {
            panic!("expected gdrive target");
        };
        assert_eq!(folder, "Backups/Example");
        assert_eq!(auth, &GoogleDriveAuth::Keychain);

        let PullTarget::File { path, .. } = &targets[2] else {
            panic!("expected shared file target");
        };
        assert!(!path.to_string_lossy().contains('~'), "tilde should expand");
    }

    #[test]
    fn test_source_derivation() {
        // Backup-only, dump-only, and both.
        let json = r#"{
            "environments": [
                { "name": "backup-only", "ssh": { "host": "h" },
                  "backup_glob": "/b/*.gz", "targets": [{ "type": "file", "path": "~/x" }] },
                { "name": "dump-only", "ssh": { "host": "h" },
                  "database": "app", "targets": [{ "type": "file", "path": "~/x" }] }
            ]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let config = DatabasePullConfig::from_content(&content).expect("should validate");

        let backup_only = config.environments[0].sources();
        assert_eq!(backup_only.len(), 1);
        assert!(matches!(backup_only[0], PullSource::BackupFile { .. }));

        let dump_only = config.environments[1].sources();
        assert_eq!(dump_only.len(), 1);
        assert!(matches!(dump_only[0], PullSource::Dump { .. }));
    }

    #[test]
    fn test_multiple_environments_and_shared_targets() {
        let json = r#"{
            "environments": [
                { "name": "NZ", "ssh": { "host": "nz-host" }, "database": "nz_live",
                  "targets": [{ "type": "database", "name": "NZ local", "database": "nz" }] },
                { "name": "AU", "ssh": { "host": "au-host" }, "database": "au_live",
                  "targets": [{ "type": "database", "name": "AU local", "database": "au" }] }
            ],
            "shared_targets": [{ "type": "file", "name": "Download", "path": "~/Downloads" }]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let config = DatabasePullConfig::from_content(&content).expect("should validate");

        assert_eq!(config.environments.len(), 2);
        assert_eq!(config.environments[0].ssh.host, "nz-host");
        assert_eq!(config.environments[1].ssh.host, "au-host");

        // NZ only offers its own DB target plus the shared download — never AU's.
        let nz_targets = config.environments[0].available_targets(&config.shared_targets);
        let names = nz_targets
            .iter()
            .map(|target| target.name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["NZ local", "Download"]);
    }

    #[test]
    fn test_config_validation_errors() {
        let content: DatabasePullSettingsContent =
            serde_json::from_str("{}").expect("empty settings should deserialize");
        let error = DatabasePullConfig::from_content(&content)
            .expect_err("empty settings should not validate");
        assert!(error.to_string().contains("at least one environment"));

        // An environment with no ssh host is an error.
        let json = r#"{
            "environments": [{ "name": "e", "ssh": { "host": "" }, "database": "db",
                "targets": [{ "type": "file", "path": "~/x" }] }]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let error = DatabasePullConfig::from_content(&content)
            .expect_err("missing ssh host should not validate");
        assert!(error.to_string().contains("ssh host"));

        // An environment with neither database nor backup_glob is an error.
        let json = r#"{
            "environments": [{ "name": "e", "ssh": { "host": "h" },
                "targets": [{ "type": "file", "path": "~/x" }] }]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let error = DatabasePullConfig::from_content(&content)
            .expect_err("environment with no source should not validate");
        assert!(error.to_string().contains("needs a `database`"));

        // A backtick in the database name is rejected.
        let json = r#"{
            "environments": [{ "name": "e", "ssh": { "host": "h" }, "database": "bad`name",
                "targets": [{ "type": "file", "path": "~/x" }] }]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let error = DatabasePullConfig::from_content(&content)
            .expect_err("backtick database name should not validate");
        assert!(error.to_string().contains("invalid characters"));

        // An environment with no targets (and no shared targets) is an error.
        let json = r#"{
            "environments": [{ "name": "e", "ssh": { "host": "h" }, "database": "db" }]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let error = DatabasePullConfig::from_content(&content)
            .expect_err("no targets should not validate");
        assert!(error.to_string().contains("no targets"));

        // Duplicate environment names are rejected.
        let json = r#"{
            "environments": [
                { "name": "dup", "ssh": { "host": "h" }, "database": "a",
                  "targets": [{ "type": "file", "path": "~/x" }] },
                { "name": "dup", "ssh": { "host": "h" }, "database": "b",
                  "targets": [{ "type": "file", "path": "~/x" }] }
            ]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let error = DatabasePullConfig::from_content(&content)
            .expect_err("duplicate environment names should not validate");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn test_gdrive_auth_file_path_expands() {
        let json = r#"{
            "environments": [{ "name": "e", "ssh": { "host": "h" }, "database": "db",
                "targets": [{ "type": "gdrive", "folder": "Backups", "auth": "~/keys/sa.json" }] }]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let config = DatabasePullConfig::from_content(&content).expect("should validate");
        let PullTarget::GoogleDrive { auth, .. } = &config.environments[0].targets[0] else {
            panic!("expected gdrive target");
        };
        let GoogleDriveAuth::File(path) = auth else {
            panic!("expected file auth");
        };
        assert!(!path.to_string_lossy().contains('~'));
    }

    #[test]
    fn test_env_credentials_parse() {
        let json = r#"{
            "environments": [{ "name": "e", "ssh": { "host": "h" }, "database": "db",
                "credentials": { "env": { "source": "/app/.env" } },
                "targets": [{ "type": "file", "path": "~/x" }] }]
        }"#;
        let content: DatabasePullSettingsContent =
            serde_json::from_str(json).expect("settings should deserialize");
        let config = DatabasePullConfig::from_content(&content).expect("should validate");
        let RemoteDatabaseCredentials::Env {
            source,
            user_var,
            password_var,
            host_var,
        } = &config.environments[0].credentials
        else {
            panic!("expected env credentials");
        };
        assert_eq!(source.as_deref(), Some("/app/.env"));
        assert_eq!(user_var, "DB_USERNAME");
        assert_eq!(password_var, "DB_PASSWORD");
        assert_eq!(host_var, "DB_HOST");
    }
}
