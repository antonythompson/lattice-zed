//! The Database Pull window: a dedicated modal with a dashboard (last run +
//! live progress + a Pull button) and a structured editor for this project's
//! `database_pull` settings, written back to the project's `.zed/settings.json`.

use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{
    Anchor, App, ClipboardItem, Context, DismissEvent, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, Render, ScrollHandle, Subscription, Task, WeakEntity, Window,
};
use project::{Project, ProjectGroupKey, ProjectPath, WorktreeId};
use settings::{
    DatabasePullSettingsContent, Settings as _, SettingsLocation, SettingsStore,
};
use ui::{
    Banner, ContextMenu, IconPosition, Modal, ModalFooter, ModalHeader, PopoverMenu, ProgressBar,
    Section, ToggleButtonGroup, ToggleButtonSimple, prelude::*,
};
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::{ModalView, Workspace};

use crate::begin_pull;
use crate::pull_form::{
    ConfigForm, CredentialsKind, EnvironmentForm, PostImportForm, TargetForm, TargetKind,
};
use crate::pull_settings::DatabasePullConfig;
use crate::pull_store::{DatabasePullStore, PullEvent};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModalTab {
    Dashboard,
    Configuration,
}

/// Identifies a target form so add/remove/kind handlers can address either an
/// environment's target list or the shared-targets list.
#[derive(Clone, Copy)]
enum TargetLoc {
    Env { env: usize, target: usize },
    Shared { target: usize },
}

impl TargetLoc {
    fn tag(&self) -> String {
        match self {
            TargetLoc::Env { env, target } => format!("env{env}-t{target}"),
            TargetLoc::Shared { target } => format!("shared-t{target}"),
        }
    }
}

pub struct DatabasePullModal {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    key: ProjectGroupKey,
    worktree_id: Option<WorktreeId>,
    store: Entity<DatabasePullStore>,
    tab: ModalTab,
    form: ConfigForm,
    saving: bool,
    last_error: Option<SharedString>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl DatabasePullModal {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let weak = cx.entity().downgrade();
        let project = workspace.project().clone();
        workspace.toggle_modal(window, cx, |window, cx| {
            Self::new(weak, project, window, cx)
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = DatabasePullStore::global(cx);
        let subscription = cx.subscribe(&store, |_, _, _: &PullEvent, cx| cx.notify());

        let worktree = project.read(cx).visible_worktrees(cx).next();
        let (worktree_id, content) = if let Some(worktree) = &worktree {
            let worktree_id = worktree.read(cx).id();
            let location = SettingsLocation {
                worktree_id,
                path: RelPath::empty(),
            };
            let content = crate::pull_settings::DatabasePullSettings::get(Some(location), cx)
                .content
                .clone();
            (Some(worktree_id), content)
        } else {
            (None, None)
        };

        let form = ConfigForm::from_content(content.as_ref(), window, cx);
        let key = ProjectGroupKey::from_project(project.read(cx), cx);
        let tab = if content.is_some() {
            ModalTab::Dashboard
        } else {
            ModalTab::Configuration
        };

        Self {
            workspace,
            project,
            key,
            worktree_id,
            store,
            tab,
            form,
            saving: false,
            last_error: None,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn on_tab(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn on_tab_prev(&mut self, _: &menu::SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
    }

    fn start_pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| begin_pull(workspace, window, cx))
                .ok();
        });
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let content = self.form.to_content(cx);
        if let Err(error) = DatabasePullConfig::from_content(&content) {
            self.last_error = Some(format!("{error:#}").into());
            cx.notify();
            return;
        }
        let Some(worktree_id) = self.worktree_id else {
            self.last_error = Some("Open a project folder to save settings.".into());
            cx.notify();
            return;
        };

        self.last_error = None;
        self.saving = true;
        cx.notify();

        let task = write_project_settings(self.project.downgrade(), worktree_id, content, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.tab = ModalTab::Dashboard;
                    }
                    Err(error) => {
                        this.last_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn target_mut(&mut self, loc: TargetLoc) -> &mut TargetForm {
        match loc {
            TargetLoc::Env { env, target } => &mut self.form.environments[env].targets[target],
            TargetLoc::Shared { target } => &mut self.form.shared_targets[target],
        }
    }

    fn target_ref(&self, loc: TargetLoc) -> &TargetForm {
        match loc {
            TargetLoc::Env { env, target } => &self.form.environments[env].targets[target],
            TargetLoc::Shared { target } => &self.form.shared_targets[target],
        }
    }

    fn remove_target(&mut self, loc: TargetLoc) {
        match loc {
            TargetLoc::Env { env, target } => {
                self.form.environments[env].targets.remove(target);
            }
            TargetLoc::Shared { target } => {
                self.form.shared_targets.remove(target);
            }
        }
    }

    fn render_dashboard(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let running = store
            .state(&self.key)
            .filter(|state| state.stage.is_running())
            .map(|state| (state.stage.status_message(), state.stage.progress()));
        let last = store.last_run(&self.key).cloned();

        let body = if let Some((message, progress)) = running {
            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(IconName::ArrowCircle).color(Color::Accent))
                        .child(Label::new(message)),
                )
                .when_some(progress, |this, (bytes, total)| {
                    this.child(ProgressBar::new(
                        "dashboard-progress",
                        bytes as f32,
                        total.max(1) as f32,
                        cx,
                    ))
                })
                .child(
                    Button::new("dashboard-cancel", "Cancel")
                        .style(ButtonStyle::Outlined)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let key = this.key.clone();
                            this.store.update(cx, |store, cx| store.cancel(&key, cx));
                        })),
                )
                .into_any_element()
        } else if let Some(last) = last {
            let (icon, color, message) = match &last.outcome {
                Ok(message) => (IconName::Check, Color::Success, message.to_string()),
                Err(error) => (IconName::Warning, Color::Error, error.clone()),
            };
            v_flex()
                .gap_2()
                .child(
                    Label::new(format!("{} → {} → {}", last.environment, last.source, last.target))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(Icon::new(icon).color(color))
                        .child(div().max_w(rems(28.)).child(Label::new(message.clone()))),
                )
                .when(last.outcome.is_err(), |this| {
                    this.child(
                        Button::new("dashboard-copy-error", "Copy error")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(message.clone()));
                            })),
                    )
                })
                .into_any_element()
        } else {
            Label::new("No pulls have run for this project yet.")
                .color(Color::Muted)
                .into_any_element()
        };

        v_flex()
            .p_3()
            .gap_3()
            .child(body)
            .child(
                Button::new("dashboard-pull", "Pull database…")
                    .style(ButtonStyle::Filled)
                    .full_width()
                    .on_click(cx.listener(|this, _, window, cx| this.start_pull(window, cx))),
            )
            .into_any_element()
    }

    fn render_config(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let environments = self.form.environments.len();
        let shared_targets = self.form.shared_targets.len();
        let excludes = self.form.exclude_table_data.len();

        v_flex()
            .id("database-pull-config")
            .tab_group()
            .p_3()
            .gap_3()
            .max_h(rems(30.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .child(section_header(
                "Environments",
                "add-environment",
                cx.listener(|this, _, window, cx| {
                    this.form.environments.push(EnvironmentForm::empty(window, cx));
                    cx.notify();
                }),
            ))
            .children((0..environments).map(|env_ix| self.render_environment(env_ix, window, cx)))
            .child(section_header(
                "Shared targets",
                "add-shared-target",
                cx.listener(|this, _, window, cx| {
                    this.form.shared_targets.push(TargetForm::empty(window, cx));
                    cx.notify();
                }),
            ))
            .children(
                (0..shared_targets)
                    .map(|ix| self.render_target(TargetLoc::Shared { target: ix }, window, cx)),
            )
            .child(section_header(
                "Excluded table data",
                "add-exclude",
                cx.listener(|this, _, window, cx| {
                    this.form
                        .exclude_table_data
                        .push(crate::pull_form::exclude_input(window, cx));
                    cx.notify();
                }),
            ))
            .children((0..excludes).map(|ix| {
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(self.form.exclude_table_data[ix].clone()))
                    .child(
                        IconButton::new(("rm-exclude", ix), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.form.exclude_table_data.remove(ix);
                                cx.notify();
                            })),
                    )
            }))
            .into_any_element()
    }

    fn render_environment(
        &self,
        env_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let environment = &self.form.environments[env_ix];
        let target_count = environment.targets.len();

        card(cx)
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new("Environment").size(LabelSize::Small).color(Color::Muted))
                    .child(
                        IconButton::new(("rm-env", env_ix), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.form.environments.remove(env_ix);
                                cx.notify();
                            })),
                    ),
            )
            .child(environment.name.clone())
            .child(environment.ssh_host.clone())
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(environment.ssh_username.clone()))
                    .child(div().flex_1().child(environment.ssh_port.clone())),
            )
            .child(environment.ssh_args.clone())
            .child(environment.database.clone())
            .child(environment.backup_glob.clone())
            .child(self.render_credentials(env_ix, cx))
            .child(environment.mysqldump_args.clone())
            .child(
                h_flex()
                    .mt_1()
                    .justify_between()
                    .child(Label::new("Targets").size(LabelSize::Small))
                    .child(
                        Button::new(("add-target", env_ix), "Add target")
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.form.environments[env_ix]
                                    .targets
                                    .push(TargetForm::empty(window, cx));
                                cx.notify();
                            })),
                    ),
            )
            .children((0..target_count).map(|target_ix| {
                self.render_target(
                    TargetLoc::Env {
                        env: env_ix,
                        target: target_ix,
                    },
                    window,
                    cx,
                )
            }))
            .into_any_element()
    }

    fn render_credentials(&self, env_ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let credentials = &self.form.environments[env_ix].credentials;
        let selected = CredentialsKind::ALL
            .iter()
            .position(|kind| *kind == credentials.kind)
            .unwrap_or(0);
        let labels = CredentialsKind::ALL
            .iter()
            .map(|kind| SharedString::from(kind.label()))
            .collect::<Vec<_>>();

        let mut container = v_flex().gap_2().child(
            v_flex()
                .gap_1()
                .child(Label::new("Credentials").size(LabelSize::Small).color(Color::Muted))
                .child(dropdown(
                    format!("cred-{env_ix}"),
                    labels,
                    selected,
                    Rc::new(cx.listener(move |this, index: &usize, _, cx| {
                        this.form.environments[env_ix].credentials.kind =
                            CredentialsKind::ALL[*index];
                        cx.notify();
                    })),
                )),
        );

        match credentials.kind {
            CredentialsKind::ServerDefault => {}
            CredentialsKind::Keychain => {
                container = container.child(credentials.keychain_username.clone());
            }
            CredentialsKind::Env => {
                container = container
                    .child(credentials.env_source.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(credentials.env_user_var.clone()))
                            .child(div().flex_1().child(credentials.env_password_var.clone()))
                            .child(div().flex_1().child(credentials.env_host_var.clone())),
                    );
            }
        }

        container.into_any_element()
    }

    fn render_target(
        &self,
        loc: TargetLoc,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = self.target_ref(loc);
        let tag = loc.tag();
        let selected = TargetKind::ALL
            .iter()
            .position(|kind| *kind == target.kind)
            .unwrap_or(0);
        let labels = TargetKind::ALL
            .iter()
            .map(|kind| SharedString::from(kind.label()))
            .collect::<Vec<_>>();

        let kind_dropdown = dropdown(
            format!("target-kind-{tag}"),
            labels,
            selected,
            Rc::new(cx.listener(move |this, index: &usize, _, cx| {
                this.target_mut(loc).kind = TargetKind::ALL[*index];
                cx.notify();
            })),
        );

        let mut card = card(cx).child(
            h_flex()
                .gap_2()
                .child(div().flex_1().child(kind_dropdown))
                .child(
                    IconButton::new(ElementId::from(format!("rm-target-{tag}")), IconName::Trash)
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_target(loc);
                            cx.notify();
                        })),
                ),
        );

        card = card.child(target.name.clone());

        card = match target.kind {
            TargetKind::Database => card
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(target.db_host.clone()))
                        .child(div().flex_1().child(target.db_port.clone())),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(target.db_username.clone()))
                        .child(div().flex_1().child(target.db_password.clone())),
                )
                .child(target.db_database.clone())
                .child(self.render_post_import(loc, window, cx)),
            TargetKind::File => card.child(target.file_path.clone()),
            TargetKind::Gdrive => card
                .child(target.gdrive_folder.clone())
                .child(target.gdrive_auth.clone()),
            TargetKind::Rclone => card
                .child(target.rclone_dest.clone())
                .child(target.rclone_path.clone()),
        };

        card.into_any_element()
    }

    fn render_post_import(
        &self,
        loc: TargetLoc,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = self.target_ref(loc);
        let tag = loc.tag();
        let step_count = target.post_import.len();

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new("Post-import steps")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new(ElementId::from(format!("add-step-{tag}")), "Add step")
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.target_mut(loc)
                                    .post_import
                                    .push(PostImportForm::empty(window, cx));
                                cx.notify();
                            })),
                    ),
            )
            .children((0..step_count).map(|step_ix| {
                let step = &self.target_ref(loc).post_import[step_ix];
                v_flex()
                    .gap_1()
                    .child(step.name.clone())
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(step.command.clone()))
                            .child(
                                IconButton::new(
                                    ElementId::from(format!("rm-step-{tag}-{step_ix}")),
                                    IconName::Trash,
                                )
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.target_mut(loc).post_import.remove(step_ix);
                                    cx.notify();
                                })),
                            ),
                    )
            }))
            .into_any_element()
    }
}

fn card(cx: &mut Context<DatabasePullModal>) -> Div {
    v_flex()
        .p_2()
        .gap_2()
        .rounded_sm()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .bg(cx.theme().colors().element_active.opacity(0.15))
}

fn section_header(
    title: &'static str,
    add_id: &'static str,
    on_add: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .mt_1()
        .justify_between()
        .child(Label::new(title))
        .child(
            Button::new(add_id, "Add")
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                .on_click(on_add),
        )
}

/// A labelled dropdown built from a `PopoverMenu`, so the menu is created lazily
/// on open (avoiding per-frame `ContextMenu` churn).
fn dropdown(
    base: String,
    labels: Vec<SharedString>,
    selected: usize,
    on_select: Rc<dyn Fn(&usize, &mut Window, &mut App)>,
) -> impl IntoElement {
    let current = labels.get(selected).cloned().unwrap_or_default();
    let trigger = Button::new(ElementId::from(format!("{base}-trigger")), current)
        .style(ButtonStyle::Outlined)
        .full_width()
        .end_icon(Icon::new(IconName::ChevronUpDown).size(IconSize::XSmall).color(Color::Muted));

    PopoverMenu::new(ElementId::from(format!("{base}-menu")))
        .trigger(trigger)
        .anchor(Anchor::BottomLeft)
        .menu(move |window, cx| {
            let labels = labels.clone();
            let on_select = on_select.clone();
            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                for (index, label) in labels.iter().enumerate() {
                    let on_select = on_select.clone();
                    menu = menu.toggleable_entry(
                        label.clone(),
                        index == selected,
                        IconPosition::End,
                        None,
                        move |window, cx| on_select(&index, window, cx),
                    );
                }
                menu
            }))
        })
}

fn write_project_settings(
    project: WeakEntity<Project>,
    worktree_id: WorktreeId,
    content: DatabasePullSettingsContent,
    cx: &mut App,
) -> Task<Result<()>> {
    let rel_path: Arc<RelPath> = paths::local_settings_file_relative_path().into_arc();
    let project_path = ProjectPath {
        worktree_id,
        path: rel_path.clone(),
    };

    cx.spawn(async move |cx| {
        let worktree = project
            .read_with(cx, |project, cx| project.worktree_for_id(worktree_id, cx))?
            .context("worktree not found for saving database_pull settings")?;

        let needs_creation =
            worktree.read_with(cx, |worktree, _| worktree.entry_for_path(&rel_path).is_none());
        if needs_creation {
            worktree
                .update(cx, |worktree, cx| {
                    worktree.create_entry(rel_path.clone(), false, None, cx)
                })
                .await?;
        }

        let buffer_store = project.read_with(cx, |project, _| project.buffer_store().clone())?;
        let buffer = buffer_store
            .update(cx, |store, cx| store.open_buffer(project_path.clone(), cx))
            .await?;

        buffer.update(cx, move |buffer, cx| {
            let current_text = buffer.text();
            if let Some(new_text) = cx
                .global::<SettingsStore>()
                .new_text_for_update(current_text, move |settings| {
                    settings.project.database_pull = Some(content);
                })
                .log_err()
            {
                buffer.edit([(0..buffer.len(), new_text)], None, cx);
            }
        });

        buffer_store
            .update(cx, |store, cx| store.save_buffer(buffer, cx))
            .await?;

        Ok(())
    })
}

impl EventEmitter<DismissEvent> for DatabasePullModal {}

impl Focusable for DatabasePullModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for DatabasePullModal {}

impl Render for DatabasePullModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.tab;
        let content = match tab {
            ModalTab::Dashboard => self.render_dashboard(cx),
            ModalTab::Configuration => self.render_config(window, cx),
        };

        v_flex()
            .id("database-pull-modal")
            .key_context("DatabasePullModal")
            .w(rems(44.))
            .elevation_3(cx)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_tab_prev))
            .child(
                Modal::new("database-pull", None)
                    .header(
                        ModalHeader::new()
                            .headline("Database Pull")
                            .description("Pull a remote database into a local DB, file, or cloud."),
                    )
                    .child(
                        div().px_3().pt_2().child(
                            ToggleButtonGroup::single_row(
                                "database-pull-view",
                                [
                                    ToggleButtonSimple::new(
                                        "Dashboard",
                                        cx.listener(|this, _, _, cx| {
                                            this.tab = ModalTab::Dashboard;
                                            cx.notify();
                                        }),
                                    )
                                    .selected(tab == ModalTab::Dashboard),
                                    ToggleButtonSimple::new(
                                        "Configuration",
                                        cx.listener(|this, _, _, cx| {
                                            this.tab = ModalTab::Configuration;
                                            cx.notify();
                                        }),
                                    )
                                    .selected(tab == ModalTab::Configuration),
                                ],
                            )
                            .selected_index(if tab == ModalTab::Dashboard { 0 } else { 1 }),
                        ),
                    )
                    .when_some(self.last_error.clone(), |this, error| {
                        this.section(
                            Section::new().child(
                                Banner::new()
                                    .severity(Severity::Warning)
                                    .child(div().text_xs().child(error)),
                            ),
                        )
                    })
                    .child(content)
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("database-pull-cancel", "Close").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.cancel(&menu::Cancel, window, cx)
                                        }),
                                    ),
                                )
                                .when(tab == ModalTab::Configuration, |this| {
                                    this.child(
                                        Button::new("database-pull-save", "Save")
                                            .style(ButtonStyle::Filled)
                                            .disabled(self.saving)
                                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                                    )
                                }),
                        ),
                    ),
            )
    }
}
