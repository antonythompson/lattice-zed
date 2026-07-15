use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, Focusable, Render, SharedString, Task,
    WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace, ui::HighlightedLabel};

/// Shows a modal list of `items`; `on_select` runs with the chosen item's
/// original index. Dismissing without choosing drops the callback.
pub fn pick(
    workspace: &mut Workspace,
    placeholder: SharedString,
    items: Vec<SharedString>,
    window: &mut Window,
    cx: &mut App,
    on_select: impl FnOnce(usize, &mut Window, &mut App) + 'static,
) {
    workspace.toggle_modal(window, cx, |window, cx| {
        let delegate = PullOptionDelegate {
            picker_entity: cx.entity().downgrade(),
            items,
            matches: Vec::new(),
            selected_index: 0,
            placeholder,
            on_select: Some(Box::new(on_select)),
        };
        PullOptionPicker {
            picker: cx.new(|cx| Picker::uniform_list(delegate, window, cx)),
        }
    });
}

pub struct PullOptionPicker {
    picker: Entity<Picker<PullOptionDelegate>>,
}

impl Focusable for PullOptionPicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for PullOptionPicker {}
impl ModalView for PullOptionPicker {}

impl Render for PullOptionPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

pub struct PullOptionDelegate {
    picker_entity: WeakEntity<PullOptionPicker>,
    items: Vec<SharedString>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    placeholder: SharedString,
    on_select: Option<Box<dyn FnOnce(usize, &mut Window, &mut App)>>,
}

impl PickerDelegate for PullOptionDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.to_string().into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self
            .items
            .iter()
            .enumerate()
            .map(|(id, name)| StringMatchCandidate::new(id, name))
            .collect::<Vec<_>>();

        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update(cx, |this, _| {
                this.delegate.matches = matches;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.matches.len().saturating_sub(1));
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let chosen = self
            .matches
            .get(self.selected_index)
            .map(|selection| selection.candidate_id);
        let on_select = self.on_select.take();
        self.picker_entity
            .update(cx, |_, cx| {
                cx.emit(DismissEvent);
            })
            .ok();
        // Run the callback only after this modal's dismiss has been processed,
        // otherwise the pending DismissEvent tears down the next modal (e.g.
        // the target picker) that the callback opens synchronously.
        if let (Some(index), Some(on_select)) = (chosen, on_select) {
            window.defer(cx, move |window, cx| on_select(index, window, cx));
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker_entity
            .update(cx, |_, cx| {
                cx.emit(DismissEvent);
            })
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidate = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    candidate.string.clone(),
                    candidate.positions.clone(),
                )),
        )
    }
}
