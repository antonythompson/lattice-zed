use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use async_compression::futures::bufread::GzipDecoder;
use futures::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, io::BufReader};
use gpui::{AppContext as _, AsyncApp, WeakEntity};
use project::ProjectGroupKey;
use smol::channel::Sender;
use util::command::{new_command, Stdio};

use crate::commands;
use crate::pull_settings::{
    DatabasePullConfig, GoogleDriveAuth, LocalDatabase, PullSource, PullTarget,
    RemoteDatabaseCredentials, SshTarget,
};
use crate::pull_store::{DatabasePullStore, PullCleanup, PullStage};
use gpui::SharedString;

const DOWNLOAD_CHUNK_SIZE: usize = 128 * 1024;
const PROGRESS_UPDATE_INTERVAL: Duration = Duration::from_millis(200);

pub(crate) async fn run_pull(
    config: DatabasePullConfig,
    source: PullSource,
    target: PullTarget,
    project_dir: Option<PathBuf>,
    store: WeakEntity<DatabasePullStore>,
    key: &ProjectGroupKey,
    cx: &mut AsyncApp,
) -> Result<SharedString> {
    let dump_auth = match &source {
        PullSource::Dump { credentials, .. } => match credentials {
            RemoteDatabaseCredentials::ServerDefault => commands::DumpAuth::ServerDefault,
            RemoteDatabaseCredentials::Keychain { username } => {
                let password = read_password(&commands::keychain_url(key, "remote-db"), cx)
                    .await
                    .context(
                        "the remote database password is not in the keychain — \
                         run `database pull: set remote database password` first",
                    )?;
                commands::DumpAuth::Explicit {
                    username: username.clone(),
                    password,
                }
            }
            RemoteDatabaseCredentials::Env {
                source,
                user_var,
                password_var,
                host_var,
            } => commands::DumpAuth::Env {
                source: source.clone(),
                user_var: user_var.clone(),
                password_var: password_var.clone(),
                host_var: host_var.clone(),
            },
        },
        PullSource::BackupFile { .. } => commands::DumpAuth::ServerDefault,
    };

    let ssh = source.ssh().clone();
    set_stage(&store, key, PullStage::Connecting, cx);
    let connect = run_ssh(&ssh, "true".to_string(), cx).await;
    let (_, stderr) = check_output(connect, "connecting over ssh")
        .map_err(|error| anyhow!("{error}\nCheck that key/agent SSH auth works for this host."))?;
    push_stderr(&store, key, stderr, cx);

    // Locate or produce the remote dump file.
    let remote_dump_path = match &source {
        PullSource::Dump {
            database,
            mysqldump_args,
            ..
        } => {
            let version = run_ssh(&ssh, "mysqldump --version".to_string(), cx).await;
            let (version_output, stderr) =
                check_output(version, "checking mysqldump on the server")?;
            push_stderr(&store, key, stderr, cx);
            let is_mariadb = version_output.to_lowercase().contains("mariadb");

            set_stage(&store, key, PullStage::Dumping, cx);
            let script = commands::remote_dump_script(
                database,
                &config.exclude_table_data,
                mysqldump_args,
                &dump_auth,
                is_mariadb,
            );
            let dump = run_ssh_with_stdin(&ssh, "sh -s".to_string(), script, cx).await;
            let (stdout, stderr) = check_output(dump, "dumping the remote database")?;
            push_stderr(&store, key, stderr, cx);
            let path = stdout.trim().to_string();
            anyhow::ensure!(!path.is_empty(), "mysqldump did not produce a dump file");
            record_cleanup(&store, key, cx, {
                let ssh = ssh.clone();
                let path = path.clone();
                move |cleanup| {
                    cleanup.ssh = Some(ssh);
                    cleanup.remote_tmp_path = Some(path);
                }
            });
            path
        }
        PullSource::BackupFile { path_glob, .. } => {
            set_stage(&store, key, PullStage::LocatingBackup, cx);
            let list = run_ssh(&ssh, commands::newest_backup_command(path_glob), cx).await;
            let (stdout, stderr) = check_output(list, "locating the newest backup")?;
            push_stderr(&store, key, stderr, cx);
            let path = stdout.trim().to_string();
            anyhow::ensure!(
                !path.is_empty(),
                "no backup file matched the glob {path_glob:?} on the server"
            );
            path
        }
    };

    // Determine the dump size so download and import progress are exact.
    let stat = run_ssh(&ssh, commands::file_size_command(&remote_dump_path), cx).await;
    let (stdout, stderr) = check_output(stat, "reading the dump file size")?;
    push_stderr(&store, key, stderr, cx);
    let total_bytes = stdout
        .trim()
        .parse::<u64>()
        .with_context(|| format!("unexpected stat output: {stdout:?}"))?;

    set_stage(
        &store,
        key,
        PullStage::Downloading {
            bytes: 0,
            total: total_bytes,
        },
        cx,
    );
    // File targets download straight to their destination and keep the
    // result; other targets download to a scratch file that is cleaned up.
    let local_dump_path = match &target {
        PullTarget::File { path, .. } => {
            // Treat the path as an exact destination only when it names a dump
            // file (….sql.gz/.sql/.gz). Otherwise it is a directory to drop an
            // auto-named file into — created during the download — so pointing
            // at a not-yet-existing folder like ~/Backups/vendella works.
            if is_dump_file_path(path) {
                path.clone()
            } else {
                path.join(auto_file_name(&source, &remote_dump_path))
            }
        }
        _ => {
            let path = scratch_dump_path(key);
            record_cleanup(&store, key, cx, {
                let path = path.clone();
                move |cleanup| cleanup.local_dump_path = Some(path)
            });
            path
        }
    };
    let (progress_tx, progress_rx) = smol::channel::unbounded();
    let download = cx.background_spawn(download_dump(
        ssh.clone(),
        remote_dump_path.clone(),
        local_dump_path.clone(),
        progress_tx,
    ));
    let mut last_update = Instant::now();
    while let Ok(bytes) = progress_rx.recv().await {
        if last_update.elapsed() >= PROGRESS_UPDATE_INTERVAL || bytes == total_bytes {
            set_stage(
                &store,
                key,
                PullStage::Downloading {
                    bytes,
                    total: total_bytes,
                },
                cx,
            );
            last_update = Instant::now();
        }
    }
    let stderr = download.await.context("downloading the dump")?;
    push_stderr(&store, key, stderr, cx);

    // The remote temp file is no longer needed; remove it now rather than at
    // the very end so failed imports don't leave it behind any longer than
    // necessary.
    if let PullSource::Dump { .. } = &source {
        let remove = run_ssh(&ssh, commands::remove_file_command(&remote_dump_path), cx).await;
        if let Err(error) = check_output(remove, "removing the remote temp file") {
            log::warn!(target: "database_pull", "{error:#}");
        }
        record_cleanup(&store, key, cx, |cleanup| cleanup.remote_tmp_path = None);
    }

    match &target {
        PullTarget::File { .. } => Ok(format!("Saved to {}", local_dump_path.display()).into()),
        PullTarget::GoogleDrive { folder, auth, .. } => {
            set_stage(
                &store,
                key,
                PullStage::Uploading {
                    bytes: 0,
                    total: total_bytes,
                },
                cx,
            );
            let auth_json = load_gdrive_credentials(auth, cx).await?;
            let http_client = cx.update(|cx| cx.http_client());
            let file_name = auto_file_name(&source, &remote_dump_path);
            let (progress_tx, progress_rx) = smol::channel::unbounded();
            let upload = cx.background_spawn(crate::gdrive::upload(
                http_client,
                auth_json,
                folder.clone(),
                local_dump_path.clone(),
                file_name,
                progress_tx,
            ));
            let mut last_update = Instant::now();
            while let Ok(bytes) = progress_rx.recv().await {
                if last_update.elapsed() >= PROGRESS_UPDATE_INTERVAL || bytes == total_bytes {
                    set_stage(
                        &store,
                        key,
                        PullStage::Uploading {
                            bytes,
                            total: total_bytes,
                        },
                        cx,
                    );
                    last_update = Instant::now();
                }
            }
            upload.await.context("uploading to Google Drive")?;
            Ok(format!("Uploaded to Google Drive ({folder})").into())
        }
        PullTarget::Rclone {
            dest, rclone_path, ..
        } => {
            set_stage(
                &store,
                key,
                PullStage::Uploading {
                    bytes: 0,
                    total: 100,
                },
                cx,
            );
            let full_dest = commands::rclone_dest(dest, &auto_file_name(&source, &remote_dump_path));
            let (progress_tx, progress_rx) = smol::channel::unbounded();
            let upload = cx.background_spawn(upload_rclone(
                rclone_path.clone(),
                local_dump_path.clone(),
                full_dest.clone(),
                progress_tx,
            ));
            let mut last_update = Instant::now();
            // rclone reports a percentage, so progress is scaled to 100.
            while let Ok(percent) = progress_rx.recv().await {
                if last_update.elapsed() >= PROGRESS_UPDATE_INTERVAL || percent >= 100 {
                    set_stage(
                        &store,
                        key,
                        PullStage::Uploading {
                            bytes: percent,
                            total: 100,
                        },
                        cx,
                    );
                    last_update = Instant::now();
                }
            }
            upload.await.context("uploading with rclone")?;
            Ok(format!("Uploaded via rclone to {full_dest}").into())
        }
        PullTarget::Database(local) => {
            let local = local.clone();
            let local_password =
                match read_password(&commands::keychain_url(key, "local"), cx).await {
                    Some(password) => Some(password),
                    None => local.password.clone(),
                };

            set_stage(
                &store,
                key,
                PullStage::Importing {
                    bytes: 0,
                    total: total_bytes,
                },
                cx,
            );
            let recreate = recreate_database(&local, local_password.as_deref(), cx).await;
            let (_, stderr) = check_output(recreate, "recreating the local database")?;
            push_stderr(&store, key, stderr, cx);

            let (progress_tx, progress_rx) = smol::channel::unbounded();
            let import = cx.background_spawn(import_dump(
                local.clone(),
                local_password.clone(),
                local_dump_path.clone(),
                progress_tx,
            ));
            let mut last_update = Instant::now();
            while let Ok(bytes) = progress_rx.recv().await {
                if last_update.elapsed() >= PROGRESS_UPDATE_INTERVAL || bytes == total_bytes {
                    set_stage(
                        &store,
                        key,
                        PullStage::Importing {
                            bytes,
                            total: total_bytes,
                        },
                        cx,
                    );
                    last_update = Instant::now();
                }
            }
            let stderr = import.await.context("importing the dump")?;
            push_stderr(&store, key, stderr, cx);

            let step_count = local.post_import.len();
            for (index, step) in local.post_import.iter().enumerate() {
                set_stage(
                    &store,
                    key,
                    PullStage::PostStep {
                        index,
                        count: step_count,
                        name: step.name.clone(),
                    },
                    cx,
                );
                let output = run_post_import_step(&step.command, project_dir.as_deref(), cx).await;
                let (_, stderr) = check_output(output, "running the step").with_context(|| {
                    format!(
                        "the database imported successfully, but post-import step {} of {} ({:?}) failed",
                        index + 1,
                        step_count,
                        step.name
                    )
                })?;
                push_stderr(&store, key, stderr, cx);
            }

            Ok(format!("Database pull complete ({})", local.database).into())
        }
    }
}

/// Whether a file target path names an exact dump file (rather than a
/// directory to auto-name a file inside). An existing directory is never an
/// exact file; otherwise it is decided by a dump-file extension so a
/// not-yet-existing folder path is treated as a directory.
fn is_dump_file_path(path: &Path) -> bool {
    if path.is_dir() {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".sql.gz") || name.ends_with(".sql") || name.ends_with(".gz")
}

/// The uploaded/saved file name: dump sources get `{database}-{timestamp}.sql.gz`,
/// backup files keep their stem with the timestamp appended. The timestamp
/// includes the time so repeated pulls on the same day don't overwrite.
fn auto_file_name(source: &PullSource, remote_path: &str) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    match source {
        PullSource::Dump { database, .. } => format!("{database}-{stamp}.sql.gz"),
        PullSource::BackupFile { .. } => {
            let basename = remote_path.rsplit('/').next().unwrap_or("backup.sql.gz");
            match basename.split_once('.') {
                Some((stem, extension)) => format!("{stem}-{stamp}.{extension}"),
                None => format!("{basename}-{stamp}"),
            }
        }
    }
}

async fn load_gdrive_credentials(auth: &GoogleDriveAuth, cx: &mut AsyncApp) -> Result<Vec<u8>> {
    match auth {
        GoogleDriveAuth::Keychain => {
            let task = cx.update(|cx| cx.read_credentials(crate::GDRIVE_KEYCHAIN_URL));
            match task.await {
                Ok(Some((_, json))) => Ok(json),
                Ok(None) => Err(anyhow!(
                    "no Google Drive credentials in the keychain — \
                     run `database pull: import google drive credentials` first"
                )),
                Err(error) => Err(error.context("reading Google Drive credentials from keychain")),
            }
        }
        GoogleDriveAuth::File(path) => smol::fs::read(path)
            .await
            .with_context(|| format!("reading Google Drive credentials from {path:?}")),
    }
}

/// Removes temp files left over by a pull. Used on completion, failure, and
/// cancellation (where the pipeline task is dropped and can't clean up
/// itself).
pub(crate) async fn cleanup(cleanup: PullCleanup) {
    if let Some(path) = cleanup.local_dump_path {
        if let Err(error) = smol::fs::remove_file(&path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(target: "database_pull", "failed to remove {path:?}: {error}");
            }
        }
    }
    if let (Some(ssh), Some(remote_path)) = (cleanup.ssh, cleanup.remote_tmp_path) {
        let args = commands::ssh_args(&ssh, &commands::remove_file_command(&remote_path));
        match new_command("ssh")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
        {
            Ok(status) if status.success() => {}
            Ok(status) => log::warn!(
                target: "database_pull",
                "failed to remove remote temp file {remote_path:?}: ssh exited with {status}"
            ),
            Err(error) => log::warn!(
                target: "database_pull",
                "failed to remove remote temp file {remote_path:?}: {error}"
            ),
        }
    }
}

fn scratch_dump_path(key: &ProjectGroupKey) -> PathBuf {
    paths::data_dir()
        .join("database_pull")
        .join(commands::project_hash(key))
        .join("dump.sql.gz")
}

fn set_stage(
    store: &WeakEntity<DatabasePullStore>,
    key: &ProjectGroupKey,
    stage: PullStage,
    cx: &mut AsyncApp,
) {
    store
        .update(cx, |store, cx| store.set_stage(key, stage, cx))
        .ok();
}

fn push_stderr(
    store: &WeakEntity<DatabasePullStore>,
    key: &ProjectGroupKey,
    lines: Vec<String>,
    cx: &mut AsyncApp,
) {
    if lines.is_empty() {
        return;
    }
    store
        .update(cx, |store, cx| store.push_stderr(key, lines, cx))
        .ok();
}

fn record_cleanup(
    store: &WeakEntity<DatabasePullStore>,
    key: &ProjectGroupKey,
    cx: &mut AsyncApp,
    update: impl FnOnce(&mut PullCleanup) + 'static,
) {
    store
        .update(cx, |store, _| store.record_cleanup(key, update))
        .ok();
}

async fn read_password(url: &str, cx: &mut AsyncApp) -> Option<String> {
    let task = cx.update(|cx| cx.read_credentials(url));
    match task.await {
        Ok(Some((_, password))) => Some(String::from_utf8_lossy(&password).into_owned()),
        Ok(None) => None,
        Err(error) => {
            log::warn!(target: "database_pull", "failed to read keychain credentials: {error:#}");
            None
        }
    }
}

struct CommandResult {
    stdout: String,
    stderr_lines: Vec<String>,
    status: std::process::ExitStatus,
}

/// Converts a finished command into (stdout, stderr) or a descriptive error.
/// ssh reserves exit code 255 for transport failures, so those get a clearer
/// prefix than the remote command's own failures.
fn check_output(
    result: Result<CommandResult>,
    action: &str,
) -> Result<(String, Vec<String>)> {
    let result = result.with_context(|| format!("failed while {action}"))?;
    if result.status.success() {
        return Ok((result.stdout, result.stderr_lines));
    }
    let stderr = result.stderr_lines.join("\n");
    let prefix = if result.status.code() == Some(255) {
        "SSH connection failed"
    } else {
        "command failed"
    };
    bail!(
        "{prefix} while {action} ({}): {}",
        result.status,
        if stderr.is_empty() { "<no output>" } else { &stderr }
    )
}

async fn run_ssh(ssh: &SshTarget, remote_command: String, cx: &AsyncApp) -> Result<CommandResult> {
    let args = commands::ssh_args(ssh, &remote_command);
    cx.background_spawn(async move {
        let output = new_command("ssh")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to spawn ssh")?;
        Ok(CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr_lines: stderr_tail_lines(&String::from_utf8_lossy(&output.stderr)),
            status: output.status,
        })
    })
    .await
}

/// Runs a remote command feeding `stdin_content` to it over ssh's stdin.
/// Used for the dump script so credentials never appear in a process list.
async fn run_ssh_with_stdin(
    ssh: &SshTarget,
    remote_command: String,
    stdin_content: String,
    cx: &AsyncApp,
) -> Result<CommandResult> {
    let args = commands::ssh_args(ssh, &remote_command);
    cx.background_spawn(async move {
        let mut child = new_command("ssh")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn ssh")?;
        let mut stdin = child.stdin.take().context("failed to open ssh stdin")?;
        let mut stdout = child.stdout.take().context("failed to open ssh stdout")?;
        let stderr = child.stderr.take().context("failed to open ssh stderr")?;

        let write_input = async {
            stdin.write_all(stdin_content.as_bytes()).await?;
            stdin.close().await?;
            drop(stdin);
            anyhow::Ok(())
        };
        let read_output = async {
            let mut output = String::new();
            stdout.read_to_string(&mut output).await?;
            anyhow::Ok(output)
        };
        let (write_result, stdout_result, stderr_lines) =
            futures::join!(write_input, read_output, collect_stderr(stderr));
        let status = child.status().await.context("failed to wait for ssh")?;
        write_result?;
        Ok(CommandResult {
            stdout: stdout_result?,
            stderr_lines,
            status,
        })
    })
    .await
}

async fn download_dump(
    ssh: SshTarget,
    remote_path: String,
    local_path: PathBuf,
    progress: Sender<u64>,
) -> Result<Vec<String>> {
    if let Some(parent) = local_path.parent() {
        smol::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {parent:?}"))?;
    }

    let args = commands::ssh_args(&ssh, &commands::stream_file_command(&remote_path));
    let mut child = new_command("ssh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn ssh")?;
    let mut stdout = child.stdout.take().context("failed to open ssh stdout")?;
    let stderr = child.stderr.take().context("failed to open ssh stderr")?;

    let copy_to_file = async {
        let mut file = smol::fs::File::create(&local_path)
            .await
            .with_context(|| format!("failed to create {local_path:?}"))?;
        let mut buffer = vec![0u8; DOWNLOAD_CHUNK_SIZE];
        let mut bytes_downloaded = 0u64;
        loop {
            let read = stdout.read(&mut buffer).await.context("read from ssh")?;
            if read == 0 {
                break;
            }
            futures::AsyncWriteExt::write_all(&mut file, &buffer[..read])
                .await
                .context("write to dump file")?;
            bytes_downloaded += read as u64;
            // The receiver disappearing just means no one is watching progress
            // anymore (e.g. cancellation is in flight); keep downloading.
            progress.try_send(bytes_downloaded).ok();
        }
        futures::AsyncWriteExt::flush(&mut file).await?;
        anyhow::Ok(())
    };
    let (copy_result, stderr_lines) = futures::join!(copy_to_file, collect_stderr(stderr));
    let status = child.status().await.context("failed to wait for ssh")?;
    if !status.success() {
        bail!(
            "downloading the dump failed ({status}): {}",
            stderr_lines.join("\n")
        );
    }
    copy_result?;
    Ok(stderr_lines)
}

/// Uploads the local dump using a preconfigured rclone remote, streaming
/// `--stats` percentages back through `progress` (0-100).
async fn upload_rclone(
    rclone_path: String,
    local_path: PathBuf,
    dest: String,
    progress: Sender<u64>,
) -> Result<()> {
    let mut child = new_command(&rclone_path)
        .arg("copyto")
        .arg(&local_path)
        .arg(&dest)
        .arg("--stats=1s")
        .arg("--stats-one-line")
        .arg("--stats-log-level=NOTICE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn {rclone_path:?} — is rclone installed?"))?;
    let stdout = child.stdout.take().context("failed to open rclone stdout")?;
    let stderr = child.stderr.take().context("failed to open rclone stderr")?;

    // rclone writes stats to stderr; each line contains a "NN%" token.
    let watch_stats = async {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut tail = Vec::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(percent) = parse_rclone_percent(&line) {
                        progress.try_send(percent).ok();
                    }
                    let trimmed = line.trim_end();
                    if !trimmed.trim().is_empty() {
                        tail.push(trimmed.to_string());
                    }
                }
            }
        }
        let skip = tail.len().saturating_sub(STDERR_TAIL_LINES);
        tail.into_iter().skip(skip).collect::<Vec<_>>()
    };
    let drain_stdout = async {
        let mut stdout = stdout;
        let mut sink = Vec::new();
        stdout.read_to_end(&mut sink).await.ok();
    };
    let (stderr_tail, ()) = futures::join!(watch_stats, drain_stdout);
    let status = child.status().await.context("failed to wait for rclone")?;
    if !status.success() {
        bail!("rclone upload failed ({status}): {}", stderr_tail.join("\n"));
    }
    Ok(())
}

/// Extracts a percentage from an rclone `--stats-one-line` line such as
/// "Transferred: 1.2 MiB / 5.6 MiB, 22%, 500 KiB/s, ETA 8s".
fn parse_rclone_percent(line: &str) -> Option<u64> {
    let percent_index = line.find('%')?;
    let digits: String = line[..percent_index]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.chars().rev().collect::<String>().parse().ok()
}

async fn recreate_database(
    local: &LocalDatabase,
    password: Option<&str>,
    cx: &AsyncApp,
) -> Result<CommandResult> {
    let mut args = commands::mysql_args(local, None);
    args.push("-e".to_string());
    args.push(commands::recreate_database_sql(&local.database));
    let password = password.map(str::to_string);
    cx.background_spawn(async move {
        let mut command = new_command("mysql");
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(password) = password {
            command.env("MYSQL_PWD", password);
        }
        let output = command.output().await.context("failed to spawn mysql")?;
        Ok(CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr_lines: stderr_tail_lines(&String::from_utf8_lossy(&output.stderr)),
            status: output.status,
        })
    })
    .await
}

async fn import_dump(
    local: LocalDatabase,
    password: Option<String>,
    dump_path: PathBuf,
    progress: Sender<u64>,
) -> Result<Vec<String>> {
    let mut magic = [0u8; 2];
    let is_gzip = {
        let mut file = smol::fs::File::open(&dump_path)
            .await
            .with_context(|| format!("failed to open {dump_path:?}"))?;
        file.read_exact(&mut magic).await.is_ok() && magic == [0x1f, 0x8b]
    };

    let mut command = new_command("mysql");
    command
        .args(commands::mysql_args(&local, Some(&local.database)))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(password) = &password {
        command.env("MYSQL_PWD", password);
    }
    let mut child = command.spawn().context("failed to spawn mysql")?;
    let mut stdin = child.stdin.take().context("failed to open mysql stdin")?;
    let stderr = child.stderr.take().context("failed to open mysql stderr")?;

    let feed_dump = async {
        let file = smol::fs::File::open(&dump_path)
            .await
            .with_context(|| format!("failed to open {dump_path:?}"))?;
        // Progress counts compressed bytes read from disk against the known
        // file size, so it is exact without knowing the uncompressed size.
        let reader = BufReader::new(CountingReader::new(file, progress));
        if is_gzip {
            let mut decoder = GzipDecoder::new(reader);
            futures::io::copy(&mut decoder, &mut stdin).await?;
        } else {
            let mut reader = reader;
            futures::io::copy(&mut reader, &mut stdin).await?;
        }
        stdin.close().await?;
        drop(stdin);
        anyhow::Ok(())
    };
    let (feed_result, stderr_lines) = futures::join!(feed_dump, collect_stderr(stderr));
    let status = child.status().await.context("failed to wait for mysql")?;
    if !status.success() {
        // mysql exiting early also breaks the feeding pipe, so report the
        // mysql error rather than the resulting broken-pipe write error.
        bail!(
            "mysql import failed ({status}): {}",
            stderr_lines.join("\n")
        );
    }
    feed_result?;
    Ok(stderr_lines)
}

async fn run_post_import_step(
    command_text: &str,
    project_dir: Option<&Path>,
    cx: &AsyncApp,
) -> Result<CommandResult> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let command_text = command_text.to_string();
    let project_dir = project_dir.map(Path::to_path_buf);
    cx.background_spawn(async move {
        let mut command = new_command(shell);
        command
            .arg("-c")
            .arg(&command_text)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(project_dir) = project_dir {
            command.current_dir(project_dir);
        }
        let output = command
            .output()
            .await
            .with_context(|| format!("failed to spawn {command_text:?}"))?;
        Ok(CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr_lines: stderr_tail_lines(&String::from_utf8_lossy(&output.stderr)),
            status: output.status,
        })
    })
    .await
}

const STDERR_TAIL_LINES: usize = 30;

fn stderr_tail_lines(stderr: &str) -> Vec<String> {
    let lines = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let skip = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines.into_iter().skip(skip).collect()
}

async fn collect_stderr(stderr: impl AsyncRead + Unpin) -> Vec<String> {
    let mut lines = Vec::new();
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim_end();
                if !trimmed.trim().is_empty() {
                    lines.push(trimmed.to_string());
                }
            }
        }
    }
    let skip = lines.len().saturating_sub(STDERR_TAIL_LINES);
    lines.into_iter().skip(skip).collect()
}

/// Wraps a reader, reporting the cumulative number of bytes read through a
/// channel so the foreground can render progress.
struct CountingReader<R> {
    inner: R,
    bytes_read: u64,
    progress: Sender<u64>,
}

impl<R> CountingReader<R> {
    fn new(inner: R, progress: Sender<u64>) -> Self {
        Self {
            inner,
            bytes_read: 0,
            progress,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(read)) => {
                this.bytes_read += read as u64;
                // A closed receiver only means progress reporting stopped.
                this.progress.try_send(this.bytes_read).ok();
                Poll::Ready(Ok(read))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rclone_percent() {
        assert_eq!(
            parse_rclone_percent(
                "Transferred: 1.234 MiB / 5.678 MiB, 22%, 500 KiB/s, ETA 8s"
            ),
            Some(22)
        );
        assert_eq!(parse_rclone_percent("Transferred: 5 MiB / 5 MiB, 100%, -"), Some(100));
        assert_eq!(parse_rclone_percent("no percentage here"), None);
    }
}
