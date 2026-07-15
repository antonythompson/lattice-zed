use gpui::{App, ClipboardItem, Context, Entity, Render, Subscription, Window};
use project::{Project, ProjectGroupKey};
use ui::{ContextMenu, PopoverMenu, ProgressBar, Tooltip, prelude::*};
use workspace::{StatusItemView, Workspace, item::ItemHandle};

use crate::pull_store::{DatabasePullStore, PullEvent, PullStage};

/// Status-bar item showing the state of this project's database pull.
/// Renders nothing while no pull is active.
pub struct PullIndicator {
    project: Entity<Project>,
    store: Entity<DatabasePullStore>,
    _subscription: Subscription,
}

impl PullIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Workspace>) -> Entity<Self> {
        let project = workspace.project().clone();
        cx.new(|cx| {
            let store = DatabasePullStore::global(cx);
            let _subscription =
                cx.subscribe(&store, |_, _, _: &PullEvent, cx| cx.notify());
            Self {
                project,
                store,
                _subscription,
            }
        })
    }
}

impl Render for PullIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = h_flex().id("database-pull-indicator");
        let key = ProjectGroupKey::from_project(self.project.read(cx), cx);
        let Some(state) = self.store.read(cx).state(&key) else {
            return container;
        };

        let stage = state.stage.clone();
        let message = stage.status_message();
        let full_error = state.full_error();
        let progress = stage.progress();
        let is_running = stage.is_running();

        let icon = match &stage {
            PullStage::Done { .. } => Some((IconName::Check, Color::Success)),
            PullStage::Failed { .. } => Some((IconName::Warning, Color::Error)),
            PullStage::Cancelled => Some((IconName::Close, Color::Muted)),
            _ => None,
        };

        let button = Button::new("database-pull-status", message)
            .label_size(LabelSize::Small)
            .map(|button| match icon {
                Some((icon, color)) => {
                    button.start_icon(Icon::new(icon).size(IconSize::Small).color(color))
                }
                None => button.loading(true),
            })
            .when_some(full_error.clone(), |button, error| {
                button.tooltip(Tooltip::text(error))
            });

        let store = self.store.clone();
        container.gap_1().child(
            PopoverMenu::new("database-pull-menu")
                .trigger(button)
                .anchor(gpui::Anchor::BottomLeft)
                .menu(move |window, cx| {
                    let store = store.clone();
                    let key = key.clone();
                    let full_error = full_error.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        if is_running {
                            menu = menu.entry("Cancel Pull", None, move |_, cx| {
                                store.update(cx, |store, cx| store.cancel(&key, cx));
                            });
                        } else {
                            if let Some(error) = full_error {
                                menu = menu.entry("Copy Error", None, move |_, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(error.clone()));
                                });
                            }
                            menu = menu.entry("Dismiss", None, move |_, cx| {
                                store.update(cx, |store, cx| store.dismiss(&key, cx));
                            });
                        }
                        menu
                    }))
                }),
        )
        .when_some(progress, |this, (bytes, total)| {
            this.child(
                div().w_24().child(ProgressBar::new(
                    "database-pull-progress",
                    bytes as f32,
                    total.max(1) as f32,
                    cx,
                )),
            )
        })
    }
}

impl StatusItemView for PullIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // Hides itself by rendering nothing when no pull is active.
        None
    }
}
