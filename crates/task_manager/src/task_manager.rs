//! A standalone, Activity-Monitor-style window listing every running process owned by an open
//! project (terminals, Claude terminals, task runs, language servers, debug sessions), grouped by
//! project, with live CPU/memory usage, per-project subtotals, a grand total, and actions to end a
//! single process or force-close an entire project.
//!
//! The window holds no cross-window entities directly: a background poll loop re-derives the whole
//! snapshot each tick from [`gpui::App::windows`], so it stays correct as projects and processes
//! come and go.

use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{
    App, Entity, EntityId, FocusHandle, Focusable, IntoElement, Pixels, PromptLevel, Render, Size,
    Task, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, actions, div, point, px,
};
use project::debugger::dap_store::DapStore;
use project::{LspStore, ProjectGroupKey};
use release_channel::ReleaseChannel;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use terminal::Terminal;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::TerminalPanel;
use ui::Tooltip;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{MultiWorkspace, Pane, SaveIntent, Workspace};

actions!(
    task_manager,
    [
        /// Opens the Task Manager window listing all running project processes.
        OpenTaskManager
    ]
);

/// How often the process list and its metrics are refreshed. Comfortably above
/// `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` (200ms) so per-process CPU deltas are valid.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

// Fixed column widths so the header labels line up with every row's metrics and action button.
const CPU_COLUMN_WIDTH: Pixels = px(110.);
const MEMORY_COLUMN_WIDTH: Pixels = px(140.);
const ACTION_COLUMN_WIDTH: Pixels = px(28.);

fn column_header_label(text: &'static str) -> Label {
    Label::new(text)
        .size(LabelSize::Small)
        .color(Color::Muted)
}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &OpenTaskManager, cx| open_task_manager_window(cx));
}

/// Opens the Task Manager window, or focuses it if it is already open (singleton).
pub fn open_task_manager_window(cx: &mut App) {
    if let Some(existing) = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<TaskManager>())
    {
        existing
            .update(cx, |_, window, _| window.activate_window())
            .log_err();
        return;
    }

    let size = Size {
        width: px(760.),
        height: px(560.),
    };

    cx.open_window(
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Task Manager".into()),
                appears_transparent: false,
                traffic_light_position: Some(point(px(9.), px(9.))),
            }),
            window_bounds: Some(WindowBounds::centered(size, cx)),
            kind: WindowKind::Normal,
            app_id: Some(ReleaseChannel::global(cx).app_id().to_owned()),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| TaskManager::new(window, cx)),
    )
    .log_err();
}

#[derive(Clone, Copy, PartialEq)]
enum RowKind {
    Terminal,
    Claude,
    Task,
    LanguageServer,
    DebugSession,
}

impl RowKind {
    fn icon(self) -> IconName {
        match self {
            RowKind::Terminal => IconName::Terminal,
            RowKind::Claude => IconName::Sparkle,
            RowKind::Task => IconName::PlayFilled,
            RowKind::LanguageServer => IconName::Server,
            RowKind::DebugSession => IconName::Debug,
        }
    }

    /// Whether the user may end this process directly. Only processes backed by a closeable
    /// terminal tab are killable; editor-managed processes (language servers, debug adapters) are
    /// restarted automatically and so are shown for visibility but not killable here.
    fn is_killable(self) -> bool {
        matches!(self, RowKind::Terminal | RowKind::Claude | RowKind::Task)
    }
}

/// Handles for ending a terminal-backed row by closing its tab. Closing the item drops the
/// `Terminal` entity, whose `Drop` impl terminates the child process — so this both stops the
/// process and removes the (now-dead) tab, uniformly for plain, Claude, and task terminals.
#[derive(Clone)]
struct TerminalRef {
    workspace: WeakEntity<Workspace>,
    terminal: WeakEntity<Terminal>,
}

struct ProcessRow {
    kind: RowKind,
    label: SharedString,
    cpu_raw: f32,
    memory: u64,
    /// False when no local PID is available (debug sessions, remote processes); metrics show "—".
    has_metrics: bool,
    /// Present only for killable terminal-backed rows; used to close the terminal's tab.
    terminal_ref: Option<TerminalRef>,
}

struct ProjectSnapshot {
    key: ProjectGroupKey,
    name: SharedString,
    /// The window that owns this project group, used to force-close it.
    window: WindowHandle<MultiWorkspace>,
    /// Stores for every workspace in the group, so a forced close can shut them all down.
    stores: Vec<(WeakEntity<LspStore>, WeakEntity<DapStore>)>,
    rows: Vec<ProcessRow>,
    cpu_raw: f32,
    memory: u64,
    /// Union of every row's subtree PIDs, for the forced close.
    all_pids: Vec<Pid>,
}

pub struct TaskManager {
    focus_handle: FocusHandle,
    system: System,
    logical_cpus: f32,
    total_memory: u64,
    projects: Vec<ProjectSnapshot>,
    _poll: Task<()>,
}

impl TaskManager {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut system = System::new();
        system.refresh_memory();
        let total_memory = system.total_memory();
        let logical_cpus = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1) as f32;

        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let still_open = this
                    .update(cx, |this, cx| {
                        this.refresh(cx);
                        cx.notify();
                    })
                    .is_ok();
                if !still_open {
                    break;
                }
            }
        });

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            system,
            logical_cpus,
            total_memory,
            projects: Vec::new(),
            _poll: poll,
        };
        this.refresh(cx);
        this
    }

    /// Rebuilds the snapshot: enumerate every window's projects and their processes, refresh
    /// process metrics, then attribute each process subtree's CPU/RAM to its row.
    fn refresh(&mut self, cx: &mut App) {
        let mut raw = self.gather(cx);

        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );

        // parent -> children, built once per tick for subtree walks.
        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::default();
        for (pid, process) in self.system.processes() {
            if let Some(parent) = process.parent() {
                children.entry(parent).or_default().push(*pid);
            }
        }

        let mut projects = Vec::with_capacity(raw.len());
        for raw_project in raw.drain(..) {
            let mut rows = Vec::with_capacity(raw_project.rows.len());
            let mut project_cpu = 0.0;
            let mut project_memory = 0u64;
            let mut all_pids = Vec::new();

            for raw_row in raw_project.rows {
                let mut kind = raw_row.kind;
                let mut cpu = 0.0;
                let mut memory = 0u64;
                let mut pids = Vec::new();
                let mut has_claude = false;

                if let Some(root) = raw_row.root_pid {
                    self.sum_subtree(
                        root,
                        &children,
                        &mut pids,
                        &mut cpu,
                        &mut memory,
                        &mut has_claude,
                    );
                }
                let has_metrics = raw_row.root_pid.is_some();

                // A plain terminal currently running `claude` is shown as a Claude row.
                if kind == RowKind::Terminal && has_claude {
                    kind = RowKind::Claude;
                }

                let label = match kind {
                    RowKind::Claude if raw_row.label.is_empty() => "Claude".into(),
                    _ if raw_row.label.is_empty() => "Terminal".into(),
                    _ => raw_row.label,
                };

                project_cpu += cpu;
                project_memory += memory;
                all_pids.extend(pids.iter().copied());

                rows.push(ProcessRow {
                    kind,
                    label,
                    cpu_raw: cpu,
                    memory,
                    has_metrics,
                    terminal_ref: raw_row.terminal_ref,
                });
            }

            // Terminals first, then language servers, then debug sessions; stable within a kind.
            rows.sort_by_key(|row| match row.kind {
                RowKind::Terminal | RowKind::Claude | RowKind::Task => 0,
                RowKind::LanguageServer => 1,
                RowKind::DebugSession => 2,
            });

            projects.push(ProjectSnapshot {
                key: raw_project.key,
                name: raw_project.name,
                window: raw_project.window,
                stores: raw_project.stores,
                rows,
                cpu_raw: project_cpu,
                memory: project_memory,
                all_pids,
            });
        }

        projects.sort_by(|a, b| a.name.cmp(&b.name));
        self.projects = projects;
    }

    /// Walks the process subtree rooted at `root`, accumulating PIDs, CPU, memory, and whether any
    /// process in the subtree is the Claude CLI.
    fn sum_subtree(
        &self,
        root: Pid,
        children: &HashMap<Pid, Vec<Pid>>,
        pids: &mut Vec<Pid>,
        cpu: &mut f32,
        memory: &mut u64,
        has_claude: &mut bool,
    ) {
        let mut seen = HashSet::default();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(process) = self.system.process(pid) {
                pids.push(pid);
                *cpu += process.cpu_usage();
                *memory += process.memory();
                if process
                    .name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("claude")
                {
                    *has_claude = true;
                }
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
    }

    /// Reads every window's `MultiWorkspace` and collects each project group's processes (read-only).
    fn gather(&self, cx: &App) -> Vec<RawProject> {
        let mut projects: Vec<RawProject> = Vec::new();

        for window in cx.windows() {
            let Some(window) = window.downcast::<MultiWorkspace>() else {
                continue;
            };
            let Ok(multi_workspace) = window.read(cx) else {
                continue;
            };

            for workspace_entity in multi_workspace.workspaces() {
                let workspace = workspace_entity.read(cx);
                let key = workspace.project_group_key(cx);
                let project = workspace.project().read(cx);

                let path_detail_map = std::collections::HashMap::new();
                let name = key.display_name(&path_detail_map);

                let lsp_store = project.lsp_store();
                let dap_store = project.dap_store();

                let mut rows: Vec<RawRow> = Vec::new();

                // Terminals (plain, Claude, and task runs all live here).
                for handle in project.local_terminal_handles() {
                    let Some(terminal) = handle.upgrade() else {
                        continue;
                    };
                    let terminal_read = terminal.read(cx);
                    let root_pid = terminal_read
                        .pid_getter()
                        .map(|getter| getter.fallback_pid())
                        .or_else(|| terminal_read.pid());

                    let (kind, label): (RowKind, SharedString) = match terminal_read.task() {
                        Some(task) => {
                            (RowKind::Task, task.spawned_task.label.clone().into())
                        }
                        None => (RowKind::Terminal, SharedString::default()),
                    };

                    rows.push(RawRow {
                        kind,
                        label,
                        root_pid,
                        terminal_ref: Some(TerminalRef {
                            workspace: workspace_entity.downgrade(),
                            terminal: handle.clone(),
                        }),
                    });
                }

                // Language servers (shown for visibility/metrics; editor-managed, not killable).
                {
                    let lsp_store_read = lsp_store.read(cx);
                    for (_id, status) in lsp_store_read.language_server_statuses() {
                        rows.push(RawRow {
                            kind: RowKind::LanguageServer,
                            label: status.name.0.clone(),
                            root_pid: status.process_id.map(Pid::from_u32),
                            terminal_ref: None,
                        });
                    }
                }

                // Debug sessions (shown for visibility; editor-managed, not killable here).
                {
                    let dap_store_read = dap_store.read(cx);
                    for session in dap_store_read.sessions() {
                        let session = session.read(cx);
                        let label = session.label().unwrap_or_else(|| session.adapter().0);
                        rows.push(RawRow {
                            kind: RowKind::DebugSession,
                            label,
                            root_pid: None,
                            terminal_ref: None,
                        });
                    }
                }

                // Merge into the group (multiple workspaces can share one project group).
                if let Some(existing) = projects.iter_mut().find(|p| p.key.matches(&key)) {
                    existing.rows.extend(rows);
                    existing
                        .stores
                        .push((lsp_store.downgrade(), dap_store.downgrade()));
                } else {
                    projects.push(RawProject {
                        key,
                        name,
                        window,
                        stores: vec![(lsp_store.downgrade(), dap_store.downgrade())],
                        rows,
                    });
                }
            }
        }

        projects
    }

    fn confirm_kill_row(
        &mut self,
        project_index: usize,
        row_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(project_index) else {
            return;
        };
        let Some(row) = project.rows.get(row_index) else {
            return;
        };
        let Some(terminal_ref) = row.terminal_ref.clone() else {
            return;
        };
        let label = row.label.clone();
        let window_handle = project.window;

        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("End “{label}”?"),
            Some("This closes the terminal and stops its process."),
            &["Cancel", "End"],
            cx,
        );

        cx.spawn(async move |_, cx| {
            if answer.await != Ok(1) {
                return;
            }
            let Some(workspace) = terminal_ref.workspace.upgrade() else {
                return;
            };
            let Some(terminal) = terminal_ref.terminal.upgrade() else {
                return;
            };
            let terminal_id = terminal.entity_id();
            window_handle
                .update(cx, move |_, window, cx| {
                    close_terminal_tab(&workspace, terminal_id, window, cx);
                })
                .log_err();
        })
        .detach();
    }

    fn confirm_close_project(
        &mut self,
        project_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(project_index) else {
            return;
        };
        let name = project.name.clone();
        let key = project.key.clone();
        let window_handle = project.window;
        let stores = project.stores.clone();
        let pids = project.all_pids.clone();

        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Close project “{name}”?"),
            Some("This kills all of the project's running processes and closes it."),
            &["Cancel", "Close Project"],
            cx,
        );

        cx.spawn(async move |_, cx| {
            if answer.await != Ok(1) {
                return;
            }

            for pid in pids {
                kill_pid(pid);
            }

            window_handle
                .update(cx, |multi_workspace, window, cx| {
                    for (lsp_store, dap_store) in &stores {
                        lsp_store
                            .update(cx, |store, cx| {
                                store.shutdown_all_language_servers(cx).detach();
                            })
                            .log_err();
                        dap_store
                            .update(cx, |store, cx| store.shutdown_sessions(cx).detach())
                            .log_err();
                    }
                    multi_workspace
                        .remove_project_group(&key, window, cx)
                        .detach_and_log_err(cx);
                })
                .log_err();
        })
        .detach();
    }

    fn render_metrics(&self, cpu_raw: f32, memory: u64, has_metrics: bool) -> impl IntoElement {
        let (cpu, mem) = if has_metrics {
            (
                format!(
                    "{:.0}% ({:.0}%)",
                    cpu_raw,
                    cpu_raw / self.logical_cpus.max(1.0)
                ),
                format!(
                    "{} ({:.1}%)",
                    format_memory(memory),
                    if self.total_memory > 0 {
                        memory as f64 / self.total_memory as f64 * 100.0
                    } else {
                        0.0
                    }
                ),
            )
        } else {
            ("—".to_string(), "—".to_string())
        };

        h_flex()
            .gap_4()
            .child(
                div().w(CPU_COLUMN_WIDTH).child(
                    Label::new(cpu).size(LabelSize::Small).color(Color::Muted),
                ),
            )
            .child(
                div().w(MEMORY_COLUMN_WIDTH).child(
                    Label::new(mem).size(LabelSize::Small).color(Color::Muted),
                ),
            )
    }

    fn render_metrics_header(&self) -> impl IntoElement {
        h_flex()
            .gap_4()
            .child(div().w(CPU_COLUMN_WIDTH).child(column_header_label("CPU")))
            .child(div().w(MEMORY_COLUMN_WIDTH).child(column_header_label("Memory")))
    }

    fn render_row(
        &self,
        project_index: usize,
        row_index: usize,
        row: &ProcessRow,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let _ = cx;
        h_flex()
            .pl_8()
            .pr_3()
            .py_1()
            .gap_2()
            .justify_between()
            .child(
                h_flex()
                    .gap_2()
                    .flex_1()
                    .min_w_0()
                    .child(Icon::new(row.kind.icon()).size(IconSize::Small).color(Color::Muted))
                    .child(Label::new(row.label.clone()).size(LabelSize::Small).truncate()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(self.render_metrics(row.cpu_raw, row.memory, row.has_metrics))
                    .child(
                        h_flex()
                            .w(ACTION_COLUMN_WIDTH)
                            .flex_none()
                            .justify_center()
                            .when(row.kind.is_killable(), |this| {
                                this.child(
                                    IconButton::new(
                                        SharedString::from(format!(
                                            "kill-{project_index}-{row_index}"
                                        )),
                                        IconName::XCircle,
                                    )
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("End process"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_kill_row(project_index, row_index, window, cx);
                                    })),
                                )
                            }),
                    ),
            )
    }

    fn render_project(
        &self,
        project_index: usize,
        project: &ProjectSnapshot,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .child(
                h_flex()
                    .pl_2()
                    .pr_3()
                    .py_1p5()
                    .gap_2()
                    .justify_between()
                    .bg(cx.theme().colors().element_background)
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Icon::new(IconName::FolderOpen)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(project.name.clone())
                                    .weight(gpui::FontWeight::SEMIBOLD)
                                    .truncate(),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(self.render_metrics(project.cpu_raw, project.memory, true))
                            .child(
                                h_flex().w(ACTION_COLUMN_WIDTH).flex_none().justify_center().child(
                                    IconButton::new(
                                        ("close-project", project_index),
                                        IconName::Power,
                                    )
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Error)
                                        .tooltip(Tooltip::text("Force-close project"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.confirm_close_project(project_index, window, cx);
                                        })),
                                ),
                            ),
                    ),
            )
            .children(if project.rows.is_empty() {
                Some(
                    div().pl_8().py_1().child(
                        Label::new("No running processes")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            } else {
                None
            })
            .children(
                project
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(row_index, row)| self.render_row(project_index, row_index, row, cx)),
            )
    }
}

impl Focusable for TaskManager {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TaskManager {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total_cpu: f32 = self.projects.iter().map(|p| p.cpu_raw).sum();
        let total_memory: u64 = self.projects.iter().map(|p| p.memory).sum();
        let process_count: usize = self.projects.iter().map(|p| p.rows.len()).sum();

        v_flex()
            .size_full()
            .bg(cx.theme().colors().background)
            .text_color(cx.theme().colors().text)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        v_flex()
                            .child(Label::new("All Projects").weight(gpui::FontWeight::BOLD))
                            .child(
                                Label::new(format!(
                                    "{} process{}",
                                    process_count,
                                    if process_count == 1 { "" } else { "es" }
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(self.render_metrics(total_cpu, total_memory, true))
                            .child(div().w(ACTION_COLUMN_WIDTH).flex_none()),
                    ),
            )
            .child(
                h_flex()
                    .pl_2()
                    .pr_3()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(column_header_label("Process")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(self.render_metrics_header())
                            .child(div().w(ACTION_COLUMN_WIDTH).flex_none()),
                    ),
            )
            .child(
                v_flex()
                    .id("task-manager-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(if self.projects.is_empty() {
                        Some(
                            div().p_4().child(
                                Label::new("No open projects")
                                    .color(Color::Muted),
                            ),
                        )
                    } else {
                        None
                    })
                    .children(
                        self.projects
                            .iter()
                            .enumerate()
                            .map(|(index, project)| self.render_project(index, project, cx)),
                    ),
            )
    }
}

/// Read-only data captured during the gather phase, before metrics are computed.
struct RawProject {
    key: ProjectGroupKey,
    name: SharedString,
    window: WindowHandle<MultiWorkspace>,
    stores: Vec<(WeakEntity<LspStore>, WeakEntity<DapStore>)>,
    rows: Vec<RawRow>,
}

struct RawRow {
    kind: RowKind,
    label: SharedString,
    root_pid: Option<Pid>,
    terminal_ref: Option<TerminalRef>,
}

/// Finds the terminal whose entity matches `terminal_id` in the workspace (terminal panel first,
/// then the center) and closes its item. Closing drops the `Terminal`, which kills the child.
fn close_terminal_tab(
    workspace: &Entity<Workspace>,
    terminal_id: EntityId,
    window: &mut Window,
    cx: &mut App,
) {
    let target = {
        let workspace = workspace.read(cx);
        let mut panes: Vec<Entity<Pane>> = Vec::new();
        if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
            panes.extend(panel.read(cx).panes().into_iter().cloned());
        }
        panes.extend(workspace.panes().iter().cloned());

        let mut found = None;
        'search: for pane in &panes {
            for item in pane.read(cx).items() {
                if let Some(view) = item.downcast::<TerminalView>() {
                    if view.read(cx).terminal().entity_id() == terminal_id {
                        found = Some((pane.clone(), item.item_id()));
                        break 'search;
                    }
                }
            }
        }
        found
    };

    if let Some((pane, item_id)) = target {
        pane.update(cx, |pane, cx| {
            pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
        })
        .detach_and_log_err(cx);
    }
}

fn format_memory(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.2} GB", bytes / GB)
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

#[cfg(unix)]
fn kill_pid(pid: Pid) {
    let raw = pid.as_u32() as i32;
    // Never signal the whole process group, init, or the kernel.
    if raw <= 1 {
        return;
    }
    if unsafe { libc::kill(raw, libc::SIGKILL) } != 0 {
        log::warn!("task_manager: failed to SIGKILL pid {raw}");
    }
}

#[cfg(not(unix))]
fn kill_pid(_pid: Pid) {}
