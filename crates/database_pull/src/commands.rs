//! Pure builders for the ssh/mysqldump/mysql invocations used by the pull
//! pipeline. Kept free of process spawning and GPUI so they can be unit
//! tested.

use crate::pull_settings::{LocalDatabase, SshTarget};
use project::ProjectGroupKey;
use sha2::{Digest, Sha256};

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub fn ssh_destination(ssh: &SshTarget) -> String {
    match ssh.username.as_deref() {
        Some(username) if !username.is_empty() => format!("{username}@{}", ssh.host),
        _ => ssh.host.clone(),
    }
}

/// Arguments for `ssh` running `remote_command` on the target host.
/// `BatchMode=yes` makes ssh fail fast instead of hanging on an interactive
/// password prompt, since the pipeline can't answer one.
pub fn ssh_args(ssh: &SshTarget, remote_command: &str) -> Vec<String> {
    let mut args = vec!["-o".to_string(), "BatchMode=yes".to_string()];
    if let Some(port) = ssh.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    args.extend(ssh.args.iter().cloned());
    args.push(ssh_destination(ssh));
    args.push(remote_command.to_string());
    args
}

/// A dump script executed on the server via `ssh <dest> sh -s` with the
/// script on stdin, so that credentials never appear on a command line or in
/// a process list. It writes the gzipped dump to a remote temp file and
/// How mysqldump authenticates on the server.
#[derive(Debug, Clone, PartialEq)]
pub enum DumpAuth {
    /// No explicit credentials — mysqldump uses `~/.my.cnf` / a login path.
    ServerDefault,
    /// An explicit username with a password (e.g. from the keychain).
    Explicit { username: String, password: String },
    /// Credentials read from environment variables on the server, after
    /// optionally sourcing a file (e.g. a Laravel `.env`).
    Env {
        source: Option<String>,
        user_var: String,
        password_var: String,
        host_var: String,
    },
}

impl DumpAuth {
    /// Shell run before the dump that sets up `MYSQL_PWD` (and sources a file
    /// for env mode), and the per-`mysqldump` argument string (`-u …`).
    fn prelude_and_args(&self) -> (String, String) {
        match self {
            Self::ServerDefault => (String::new(), String::new()),
            Self::Explicit { username, password } => (
                format!("MYSQL_PWD={}\nexport MYSQL_PWD\n", shell_quote(password)),
                format!(" -u {}", shell_quote(username)),
            ),
            Self::Env {
                source,
                user_var,
                password_var,
                host_var,
            } => {
                let mut prelude = String::new();
                if let Some(source) = source {
                    // set -a exports everything the file defines so it reaches
                    // mysqldump; restored afterwards.
                    prelude.push_str(&format!("set -a\n. {}\nset +a\n", shell_quote(source)));
                }
                // Double-quoted so the shell expands the variables; the names
                // are validated to be identifier-safe upstream.
                prelude.push_str(&format!("MYSQL_PWD=\"${password_var}\"\nexport MYSQL_PWD\n"));
                (prelude, format!(" -u \"${user_var}\" -h \"${host_var}\""))
            }
        }
    }
}

/// prints that file's path as its only stdout line.
pub fn remote_dump_script(
    database: &str,
    exclude_table_data: &[String],
    extra_args: &[String],
    auth: &DumpAuth,
    is_mariadb: bool,
) -> String {
    let (auth, auth_args) = auth.prelude_and_args();

    let mut common_flags =
        "--single-transaction --quick --no-tablespaces --routines --triggers".to_string();
    // MariaDB's mysqldump rejects --set-gtid-purged; only MySQL needs it to
    // keep the dump importable into a server without GTID state.
    if !is_mariadb {
        common_flags.push_str(" --set-gtid-purged=OFF");
    }
    for arg in extra_args {
        common_flags.push(' ');
        common_flags.push_str(&shell_quote(arg));
    }

    let quoted_database = shell_quote(database);
    // `&&` between the passes so a failure in the structure pass aborts before
    // the data pass and is reflected in the captured exit code.
    let dump_commands = if exclude_table_data.is_empty() {
        format!("mysqldump{auth_args} {common_flags} {quoted_database}")
    } else {
        let ignore_flags = exclude_table_data
            .iter()
            .map(|table| format!("--ignore-table={}", shell_quote(&format!("{database}.{table}"))))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "mysqldump{auth_args} {common_flags} --no-data {quoted_database} && \\\n  \
             mysqldump{auth_args} {common_flags} --no-create-info {ignore_flags} {quoted_database}"
        )
    };

    // The dump is piped to gzip, so mysqldump's exit code would normally be
    // hidden behind gzip's. `set -o pipefail` would expose it, but it is not
    // POSIX and some restricted server shells *fatally exit* on the invalid
    // option rather than returning a catchable error. So instead the dump's
    // exit code is written to a status file inside the pipe and checked after.
    format!(
        "set -e\n\
         {auth}\
         OUT=$(mktemp /tmp/lattice-pull-XXXXXX.sql.gz)\n\
         STATUS=$(mktemp /tmp/lattice-pull-XXXXXX.status)\n\
         {{ {dump_commands} ; echo $? > \"$STATUS\" ; }} | gzip -c > \"$OUT\"\n\
         code=$(cat \"$STATUS\")\n\
         rm -f \"$STATUS\"\n\
         if [ \"$code\" != 0 ]; then rm -f \"$OUT\"; exit \"$code\"; fi\n\
         printf '%s\\n' \"$OUT\"\n"
    )
}

/// Lists the newest file matching the glob. The glob is intentionally
/// unquoted so the remote shell expands it; backup paths containing spaces
/// are not supported.
pub fn newest_backup_command(path_glob: &str) -> String {
    format!("ls -1t {path_glob} 2>/dev/null | head -n 1")
}

/// `stat` differs between GNU (Linux) and BSD; try both.
pub fn file_size_command(path: &str) -> String {
    let quoted = shell_quote(path);
    format!("stat -c %s {quoted} 2>/dev/null || stat -f %z {quoted}")
}

pub fn stream_file_command(path: &str) -> String {
    format!("cat {}", shell_quote(path))
}

pub fn remove_file_command(path: &str) -> String {
    format!("rm -f {}", shell_quote(path))
}

pub fn mysql_args(local: &LocalDatabase, database: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--protocol=tcp".to_string(),
        "-h".to_string(),
        local.host.clone(),
        "-P".to_string(),
        local.port.to_string(),
        "-u".to_string(),
        local.username.clone(),
    ];
    if let Some(database) = database {
        args.push(database.to_string());
    }
    args
}

/// Builds the full rclone destination. A trailing "/" (or a bare "remote:")
/// means a directory, so the auto-named file is appended; otherwise `dest` is
/// used verbatim as the destination path.
pub fn rclone_dest(dest: &str, file_name: &str) -> String {
    if dest.ends_with('/') || dest.ends_with(':') {
        format!("{dest}{file_name}")
    } else {
        dest.to_string()
    }
}

pub fn recreate_database_sql(database: &str) -> String {
    format!("DROP DATABASE IF EXISTS `{database}`; CREATE DATABASE `{database}` CHARACTER SET utf8mb4;")
}

/// A short stable identifier for a project, derived from its main worktree
/// paths so it survives settings edits. Used to key keychain entries and the
/// local scratch directory.
pub fn project_hash(key: &ProjectGroupKey) -> String {
    let mut hasher = Sha256::new();
    for path in key.path_list().ordered_paths() {
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn keychain_url(key: &ProjectGroupKey, slot: &str) -> String {
    format!("lattice://database-pull/{}/{slot}", project_hash(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ssh_target() -> SshTarget {
        SshTarget {
            host: "web1.example.com".to_string(),
            username: Some("deploy".to_string()),
            port: Some(2222),
            args: vec!["-o".to_string(), "StrictHostKeyChecking=accept-new".to_string()],
        }
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("simple"), "'simple'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn test_ssh_args() {
        assert_eq!(
            ssh_args(&ssh_target(), "sh -s"),
            vec![
                "-o",
                "BatchMode=yes",
                "-p",
                "2222",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "deploy@web1.example.com",
                "sh -s",
            ]
        );

        let no_user = SshTarget {
            username: None,
            port: None,
            args: Vec::new(),
            ..ssh_target()
        };
        assert_eq!(
            ssh_args(&no_user, "true"),
            vec!["-o", "BatchMode=yes", "web1.example.com", "true"]
        );
    }

    #[test]
    fn test_dump_script_without_exclusions_is_single_pass() {
        let script =
            remote_dump_script("example_live", &[], &[], &DumpAuth::ServerDefault, false);
        assert_eq!(script.matches("mysqldump").count(), 1);
        assert!(script.contains("--set-gtid-purged=OFF"));
        assert!(script.contains("'example_live'"));
        assert!(script.contains("printf '%s\\n' \"$OUT\""));
        // server_default adds no auth prelude or -u.
        assert!(!script.contains("MYSQL_PWD"));
        assert!(!script.contains(" -u "));
        // pipefail is non-portable and must not appear; failure is detected
        // via the status file instead.
        assert!(!script.contains("pipefail"));
        assert!(script.contains("STATUS=$(mktemp"));
    }

    #[test]
    fn test_dump_script_with_exclusions_splits_structure_and_data() {
        let excluded = vec!["wp_logs".to_string(), "wp_sessions".to_string()];
        let script =
            remote_dump_script("example_live", &excluded, &[], &DumpAuth::ServerDefault, false);
        assert_eq!(script.matches("mysqldump").count(), 2);
        assert!(script.contains("--no-data 'example_live'"));
        assert!(script.contains("--no-create-info"));
        assert!(script.contains("--ignore-table='example_live.wp_logs'"));
        assert!(script.contains("--ignore-table='example_live.wp_sessions'"));
        // The two passes are chained with && so a first-pass failure aborts.
        assert!(script.contains("&&"));
    }

    #[test]
    fn test_dump_script_mariadb_drops_gtid_flag() {
        let script = remote_dump_script("db", &[], &[], &DumpAuth::ServerDefault, true);
        assert!(!script.contains("--set-gtid-purged"));
    }

    #[test]
    fn test_dump_script_keychain_credentials_stay_off_argv() {
        let auth = DumpAuth::Explicit {
            username: "readonly".to_string(),
            password: "s3cr'et".to_string(),
        };
        let script = remote_dump_script("db", &[], &[], &auth, false);
        assert!(script.contains(r"MYSQL_PWD='s3cr'\''et'"));
        assert!(script.contains("mysqldump -u 'readonly'"));
    }

    #[test]
    fn test_dump_script_env_mode_sources_and_uses_vars() {
        let auth = DumpAuth::Env {
            source: Some("/container/application/current/.env".to_string()),
            user_var: "DB_USERNAME".to_string(),
            password_var: "DB_PASSWORD".to_string(),
            host_var: "DB_HOST".to_string(),
        };
        let script = remote_dump_script("auctionlive", &[], &[], &auth, false);
        assert!(script.contains(". '/container/application/current/.env'"));
        assert!(script.contains("MYSQL_PWD=\"$DB_PASSWORD\""));
        assert!(script.contains("mysqldump -u \"$DB_USERNAME\" -h \"$DB_HOST\""));
        // The password goes through MYSQL_PWD, never `-p` on the argv.
        assert!(!script.contains("-p\"$DB_PASSWORD\""));
        assert!(!script.contains("-p$"));
    }

    #[test]
    fn test_dump_script_env_mode_without_source_uses_container_env() {
        let auth = DumpAuth::Env {
            source: None,
            user_var: "DB_USERNAME".to_string(),
            password_var: "DB_PASSWORD".to_string(),
            host_var: "DB_HOST".to_string(),
        };
        let script = remote_dump_script("db", &[], &[], &auth, false);
        assert!(!script.contains(". '"), "should not source a file");
        assert!(script.contains("MYSQL_PWD=\"$DB_PASSWORD\""));
    }

    #[test]
    fn test_rclone_dest() {
        assert_eq!(
            rclone_dest("gdrive_backups:Vendella/Vendella NZ/", "db-2026-07-15.sql.gz"),
            "gdrive_backups:Vendella/Vendella NZ/db-2026-07-15.sql.gz"
        );
        assert_eq!(
            rclone_dest("remote:", "db.sql.gz"),
            "remote:db.sql.gz"
        );
        assert_eq!(
            rclone_dest("remote:path/exact.sql.gz", "db.sql.gz"),
            "remote:path/exact.sql.gz"
        );
    }

    #[test]
    fn test_backup_and_stat_commands() {
        assert_eq!(
            newest_backup_command("/var/backups/mysql/example_*.sql.gz"),
            "ls -1t /var/backups/mysql/example_*.sql.gz 2>/dev/null | head -n 1"
        );
        assert_eq!(
            file_size_command("/tmp/dump.sql.gz"),
            "stat -c %s '/tmp/dump.sql.gz' 2>/dev/null || stat -f %z '/tmp/dump.sql.gz'"
        );
    }

    #[test]
    fn test_mysql_args() {
        let local = LocalDatabase {
            name: "Local DB".into(),
            host: "127.0.0.1".to_string(),
            port: 3306,
            username: "root".to_string(),
            password: None,
            database: "example_test".to_string(),
            post_import: Vec::new(),
        };
        assert_eq!(
            mysql_args(&local, Some("example_test")),
            vec![
                "--protocol=tcp",
                "-h",
                "127.0.0.1",
                "-P",
                "3306",
                "-u",
                "root",
                "example_test",
            ]
        );
    }
}
