use std::sync::Arc;

use crate::TaskContexts;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter,
    Focusable, InteractiveElement, ParentElement, Render, SharedString, Styled, Subscription, Task,
    WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate, highlighted_match_with_paths::HighlightedMatch};
use project::{TaskSourceKind, task_store::TaskStore};
use task::{
    DebugRequestType, DebugTaskDefinition, ResolvedTask, RevealTarget, TaskContext, TaskModal,
    TaskTemplate, TaskType,
};
use ui::{
    ActiveTheme, Button, ButtonCommon, ButtonSize, Clickable, Color, FluentBuilder as _, Icon,
    IconButton, IconButtonShape, IconName, IconSize, IntoElement, KeyBinding, Label, LabelSize,
    ListItem, ListItemSpacing, RenderOnce, Toggleable, Tooltip, div, h_flex, v_flex,
};

use util::{ResultExt, truncate_and_trailoff};
use workspace::{ModalView, Workspace, tasks::schedule_resolved_task};
pub use zed_actions::{Rerun, Spawn};

/// A modal used to spawn new tasks.
pub(crate) struct TasksModalDelegate {
    task_store: Entity<TaskStore>,
    candidates: Option<Vec<(TaskSourceKind, ResolvedTask)>>,
    task_overrides: Option<TaskOverrides>,
    last_used_candidate_index: Option<usize>,
    divider_index: Option<usize>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    workspace: WeakEntity<Workspace>,
    prompt: String,
    task_contexts: TaskContexts,
    placeholder_text: Arc<str>,
    /// If this delegate is responsible for running a scripting task or a debugger
    task_modal_type: TaskModal,
}

/// Task template amendments to do before resolving the context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TaskOverrides {
    /// See [`RevealTarget`].
    pub(crate) reveal_target: Option<RevealTarget>,
}

impl TasksModalDelegate {
    fn new(
        task_store: Entity<TaskStore>,
        task_contexts: TaskContexts,
        task_overrides: Option<TaskOverrides>,
        task_modal_type: TaskModal,
        workspace: WeakEntity<Workspace>,
    ) -> Self {
        let placeholder_text = if let Some(TaskOverrides {
            reveal_target: Some(RevealTarget::Center),
        }) = &task_overrides
        {
            Arc::from("Find a task, or run a command in the central pane")
        } else {
            Arc::from("Find a task, or run a command")
        };
        Self {
            task_store,
            workspace,
            candidates: None,
            matches: Vec::new(),
            last_used_candidate_index: None,
            divider_index: None,
            selected_index: 0,
            prompt: String::default(),
            task_contexts,
            task_modal_type,
            task_overrides,
            placeholder_text,
        }
    }

    fn spawn_oneshot(&mut self) -> Option<(TaskSourceKind, ResolvedTask)> {
        if self.prompt.trim().is_empty() {
            return None;
        }

        let default_context = TaskContext::default();
        let active_context = self
            .task_contexts
            .active_context()
            .unwrap_or(&default_context);
        let source_kind = TaskSourceKind::UserInput;
        let id_base = source_kind.to_id_base();
        let mut new_oneshot = TaskTemplate {
            label: self.prompt.clone(),
            command: self.prompt.clone(),
            ..TaskTemplate::default()
        };
        if let Some(TaskOverrides {
            reveal_target: Some(reveal_target),
        }) = &self.task_overrides
        {
            new_oneshot.reveal_target = *reveal_target;
        }
        Some((
            source_kind,
            new_oneshot.resolve_task(&id_base, active_context)?,
        ))
    }

    fn delete_previously_used(&mut self, ix: usize, cx: &mut App) {
        let Some(candidates) = self.candidates.as_mut() else {
            return;
        };
        let Some(task) = candidates.get(ix).map(|(_, task)| task.clone()) else {
            return;
        };
        // We remove this candidate manually instead of .taking() the candidates, as we already know the index;
        // it doesn't make sense to requery the inventory for new candidates, as that's potentially costly and more often than not it should just return back
        // the original list without a removed entry.
        candidates.remove(ix);
        if let Some(inventory) = self.task_store.read(cx).task_inventory().cloned() {
            inventory.update(cx, |inventory, _| {
                inventory.delete_previously_used(&task.id);
            })
        };
    }
}

pub(crate) struct TasksModal {
    picker: Entity<Picker<TasksModalDelegate>>,
    _subscription: Subscription,
}

impl TasksModal {
    pub(crate) fn new(
        task_store: Entity<TaskStore>,
        task_contexts: TaskContexts,
        task_overrides: Option<TaskOverrides>,
        workspace: WeakEntity<Workspace>,
        task_modal_type: TaskModal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker = cx.new(|cx| {
            Picker::uniform_list(
                TasksModalDelegate::new(
                    task_store,
                    task_contexts,
                    task_overrides,
                    task_modal_type,
                    workspace,
                ),
                window,
                cx,
            )
        });
        let _subscription = cx.subscribe(&picker, |_, _, _, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription,
        }
    }
}

impl Render for TasksModal {
    fn render(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> impl gpui::prelude::IntoElement {
        v_flex()
            .key_context("TasksModal")
            .w(rems(34.))
            .child(self.picker.clone())
    }
}

impl EventEmitter<DismissEvent> for TasksModal {}

impl Focusable for TasksModal {
    fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        self.picker.read(cx).focus_handle(cx)
    }
}

impl ModalView for TasksModal {}

const MAX_TAG_LEN: usize = 10;

impl PickerDelegate for TasksModalDelegate {
    type ListItem = ListItem;

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
        _cx: &mut Context<picker::Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, _: &mut App) -> Arc<str> {
        self.placeholder_text.clone()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<picker::Picker<Self>>,
    ) -> Task<()> {
        let task_type = self.task_modal_type.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let Some(candidates) = picker
                .update(cx, |picker, cx| match &mut picker.delegate.candidates {
                    Some(candidates) => string_match_candidates(candidates.iter(), task_type),
                    None => {
                        let Some(task_inventory) = picker
                            .delegate
                            .task_store
                            .read(cx)
                            .task_inventory()
                            .cloned()
                        else {
                            return Vec::new();
                        };

                        let (used, current) = task_inventory
                            .read(cx)
                            .used_and_current_resolved_tasks(&picker.delegate.task_contexts, cx);
                        picker.delegate.last_used_candidate_index = if used.is_empty() {
                            None
                        } else {
                            Some(used.len() - 1)
                        };

                        let mut new_candidates = used;
                        new_candidates.extend(current);
                        let match_candidates =
                            string_match_candidates(new_candidates.iter(), task_type);
                        let _ = picker.delegate.candidates.insert(new_candidates);
                        match_candidates
                    }
                })
                .ok()
            else {
                return;
            };
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                true,
                1000,
                &Default::default(),
                cx.background_executor().clone(),
            )
            .await;
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches;
                    if let Some(index) = delegate.last_used_candidate_index {
                        delegate.matches.sort_by_key(|m| m.candidate_id > index);
                    }

                    delegate.prompt = query;
                    delegate.divider_index = delegate.last_used_candidate_index.and_then(|index| {
                        let index = delegate
                            .matches
                            .partition_point(|matching_task| matching_task.candidate_id <= index);
                        Some(index).and_then(|index| (index != 0).then(|| index - 1))
                    });

                    if delegate.matches.is_empty() {
                        delegate.selected_index = 0;
                    } else {
                        delegate.selected_index =
                            delegate.selected_index.min(delegate.matches.len() - 1);
                    }
                })
                .log_err();
        })
    }

    fn confirm(
        &mut self,
        omit_history_entry: bool,
        window: &mut Window,
        cx: &mut Context<picker::Picker<Self>>,
    ) {
        let current_match_index = self.selected_index();
        let task = self
            .matches
            .get(current_match_index)
            .and_then(|current_match| {
                let ix = current_match.candidate_id;
                self.candidates
                    .as_ref()
                    .map(|candidates| candidates[ix].clone())
            });
        let Some((task_source_kind, mut task)) = task else {
            return;
        };
        if let Some(TaskOverrides {
            reveal_target: Some(reveal_target),
        }) = &self.task_overrides
        {
            if let Some(resolved_task) = &mut task.resolved {
                resolved_task.reveal_target = *reveal_target;
            }
        }

        self.workspace
            .update(cx, |workspace, cx| {
                match task.task_type() {
                    TaskType::Debug(config) if config.locator.is_none() => {
                        let Some(config): Option<DebugTaskDefinition> = task
                            .resolved_debug_adapter_config()
                            .and_then(|config| config.try_into().ok())
                        else {
                            return;
                        };
                        let project = workspace.project().clone();

                        match &config.request {
                            DebugRequestType::Attach(attach_config)
                                if attach_config.process_id.is_none() =>
                            {
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    debugger_ui::attach_modal::AttachModal::new(
                                        project,
                                        config.clone(),
                                        window,
                                        cx,
                                    )
                                });
                            }
                            _ => {
                                project.update(cx, |project, cx| {
                                    project
                                        .start_debug_session(config.into(), cx)
                                        .detach_and_log_err(cx);
                                });
                            }
                        }
                    }
                    _ => schedule_resolved_task(
                        workspace,
                        task_source_kind,
                        task,
                        omit_history_entry,
                        cx,
                    ),
                };
            })
            .ok();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<picker::Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<picker::Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let candidates = self.candidates.as_ref()?;
        let hit = &self.matches[ix];
        let (source_kind, resolved_task) = &candidates.get(hit.candidate_id)?;
        let template = resolved_task.original_task();
        let display_label = resolved_task.display_label();

        let mut tooltip_label_text = if display_label != &template.label {
            resolved_task.resolved_label.clone()
        } else {
            String::new()
        };
        if let Some(resolved) = resolved_task.resolved.as_ref() {
            if resolved.command_label != display_label
                && resolved.command_label != resolved_task.resolved_label
            {
                if !tooltip_label_text.trim().is_empty() {
                    tooltip_label_text.push('\n');
                }
                tooltip_label_text.push_str(&resolved.command_label);
            }
        }
        let tooltip_label = if tooltip_label_text.trim().is_empty() {
            None
        } else {
            Some(Tooltip::simple(tooltip_label_text, cx))
        };

        let highlighted_location = HighlightedMatch {
            text: hit.string.clone(),
            highlight_positions: hit.positions.clone(),
            char_count: hit.string.chars().count(),
            color: Color::Default,
        };
        let icon = match source_kind {
            TaskSourceKind::UserInput => Some(Icon::new(IconName::Terminal)),
            TaskSourceKind::AbsPath { .. } => Some(Icon::new(IconName::Settings)),
            TaskSourceKind::Worktree { .. } => Some(Icon::new(IconName::FileTree)),
            TaskSourceKind::Language { name } => file_icons::FileIcons::get(cx)
                .get_icon_for_type(&name.to_lowercase(), cx)
                .map(Icon::from_path),
        }
        .map(|icon| icon.color(Color::Muted).size(IconSize::Small));
        let history_run_icon = if Some(ix) <= self.divider_index {
            Some(
                Icon::new(IconName::HistoryRerun)
                    .color(Color::Muted)
                    .size(IconSize::Small)
                    .into_any_element(),
            )
        } else {
            Some(
                v_flex()
                    .flex_none()
                    .size(IconSize::Small.rems())
                    .into_any_element(),
            )
        };

        Some(
            ListItem::new(SharedString::from(format!("tasks-modal-{ix}")))
                .inset(true)
                .start_slot::<Icon>(icon)
                .end_slot::<AnyElement>(
                    h_flex()
                        .gap_1()
                        .children(
                            template
                                .tags
                                .iter()
                                .map(|tag| {
                                    Label::new(format!(
                                        "#{}",
                                        truncate_and_trailoff(tag, MAX_TAG_LEN)
                                    ))
                                })
                                .collect::<Vec<_>>(),
                        )
                        .flex_none()
                        .child(history_run_icon.unwrap())
                        .into_any_element(),
                )
                .spacing(ListItemSpacing::Sparse)
                .when_some(tooltip_label, |list_item, item_label| {
                    list_item.tooltip(move |_, _| item_label.clone())
                })
                .map(|item| {
                    let item = if matches!(source_kind, TaskSourceKind::UserInput)
                        || Some(ix) <= self.divider_index
                    {
                        let task_index = hit.candidate_id;
                        let delete_button = div().child(
                            IconButton::new("delete", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .icon_color(Color::Muted)
                                .size(ButtonSize::None)
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(move |picker, _event, window, cx| {
                                    cx.stop_propagation();
                                    window.prevent_default();

                                    picker.delegate.delete_previously_used(task_index, cx);
                                    picker.delegate.last_used_candidate_index = picker
                                        .delegate
                                        .last_used_candidate_index
                                        .unwrap_or(0)
                                        .checked_sub(1);
                                    picker.refresh(window, cx);
                                }))
                                .tooltip(|_, cx| {
                                    Tooltip::simple("Delete Previously Scheduled Task", cx)
                                }),
                        );
                        item.end_hover_slot(
                            h_flex()
                                .gap_1()
                                .children(
                                    template
                                        .tags
                                        .iter()
                                        .map(|tag| Label::new(format!("#{}", tag)))
                                        .collect::<Vec<_>>(),
                                )
                                .flex_none()
                                .child(delete_button)
                                .into_any_element(),
                        )
                    } else {
                        item
                    };
                    item
                })
                .toggle_state(selected)
                .child(highlighted_location.render(window, cx)),
        )
    }

    fn confirm_completion(
        &mut self,
        _: String,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<String> {
        let task_index = self.matches.get(self.selected_index())?.candidate_id;
        let tasks = self.candidates.as_ref()?;
        let (_, task) = tasks.get(task_index)?;
        Some(task.resolved.as_ref()?.command_label.clone())
    }

    fn confirm_input(
        &mut self,
        omit_history_entry: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let Some((task_source_kind, mut task)) = self.spawn_oneshot() else {
            return;
        };

        if let Some(TaskOverrides {
            reveal_target: Some(reveal_target),
        }) = self.task_overrides
        {
            if let Some(resolved_task) = &mut task.resolved {
                resolved_task.reveal_target = reveal_target;
            }
        }
        self.workspace
            .update(cx, |workspace, cx| {
                match task.task_type() {
                    TaskType::Script => schedule_resolved_task(
                        workspace,
                        task_source_kind,
                        task,
                        omit_history_entry,
                        cx,
                    ),
                    // todo(debugger): Should create a schedule_resolved_debug_task function
                    // This would allow users to access to debug history and other issues
                    TaskType::Debug(debug_args) => {
                        let Some(debug_config) = task.resolved_debug_adapter_config() else {
                            // todo(debugger) log an error, this should never happen
                            return;
                        };

                        if debug_args.locator.is_some() {
                            schedule_resolved_task(
                                workspace,
                                task_source_kind,
                                task,
                                omit_history_entry,
                                cx,
                            );
                        } else {
                            workspace.project().update(cx, |project, cx| {
                                project
                                    .start_debug_session(debug_config, cx)
                                    .detach_and_log_err(cx);
                            });
                        }
                    }
                };
            })
            .ok();
        cx.emit(DismissEvent);
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        if let Some(i) = self.divider_index {
            vec![i]
        } else {
            Vec::new()
        }
    }
    fn render_footer(
        &self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<gpui::AnyElement> {
        let is_recent_selected = self.divider_index >= Some(self.selected_index);
        let current_modifiers = window.modifiers();
        let left_button = if self
            .task_store
            .read(cx)
            .task_inventory()?
            .read(cx)
            .last_scheduled_task(None)
            .is_some()
        {
            Some(("Rerun Last Task", Rerun::default().boxed_clone()))
        } else {
            None
        };
        Some(
            h_flex()
                .w_full()
                .h_8()
                .p_2()
                .justify_between()
                .rounded_b_sm()
                .bg(cx.theme().colors().ghost_element_selected)
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    left_button
                        .map(|(label, action)| {
                            let keybind = KeyBinding::for_action(&*action, window, cx);

                            Button::new("edit-current-task", label)
                                .label_size(LabelSize::Small)
                                .when_some(keybind, |this, keybind| this.key_binding(keybind))
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(action.boxed_clone(), cx);
                                })
                                .into_any_element()
                        })
                        .unwrap_or_else(|| h_flex().into_any_element()),
                )
                .map(|this| {
                    if (current_modifiers.alt || self.matches.is_empty()) && !self.prompt.is_empty()
                    {
                        let action = picker::ConfirmInput {
                            secondary: current_modifiers.secondary(),
                        }
                        .boxed_clone();
                        this.children(KeyBinding::for_action(&*action, window, cx).map(|keybind| {
                            let spawn_oneshot_label = if current_modifiers.secondary() {
                                "Spawn Oneshot Without History"
                            } else {
                                "Spawn Oneshot"
                            };

                            Button::new("spawn-onehshot", spawn_oneshot_label)
                                .label_size(LabelSize::Small)
                                .key_binding(keybind)
                                .on_click(move |_, window, cx| {
                                    window.dispatch_action(action.boxed_clone(), cx)
                                })
                        }))
                    } else if current_modifiers.secondary() {
                        this.children(
                            KeyBinding::for_action(&menu::SecondaryConfirm, window, cx).map(
                                |keybind| {
                                    let label = if is_recent_selected {
                                        "Rerun Without History"
                                    } else {
                                        "Spawn Without History"
                                    };
                                    Button::new("spawn", label)
                                        .label_size(LabelSize::Small)
                                        .key_binding(keybind)
                                        .on_click(move |_, window, cx| {
                                            window.dispatch_action(
                                                menu::SecondaryConfirm.boxed_clone(),
                                                cx,
                                            )
                                        })
                                },
                            ),
                        )
                    } else {
                        this.children(KeyBinding::for_action(&menu::Confirm, window, cx).map(
                            |keybind| {
                                let run_entry_label =
                                    if is_recent_selected { "Rerun" } else { "Spawn" };

                                Button::new("spawn", run_entry_label)
                                    .label_size(LabelSize::Small)
                                    .key_binding(keybind)
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(menu::Confirm.boxed_clone(), cx);
                                    })
                            },
                        ))
                    }
                })
                .into_any_element(),
        )
    }
}

fn string_match_candidates<'a>(
    candidates: impl Iterator<Item = &'a (TaskSourceKind, ResolvedTask)> + 'a,
    task_modal_type: TaskModal,
) -> Vec<StringMatchCandidate> {
    candidates
        .enumerate()
        .filter(|(_, (_, candidate))| match candidate.task_type() {
            TaskType::Script => task_modal_type == TaskModal::ScriptModal,
            TaskType::Debug(_) => task_modal_type == TaskModal::DebugModal,
        })
        .map(|(index, (_, candidate))| StringMatchCandidate::new(index, candidate.display_label()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use editor::Editor;
    use gpui::{TestAppContext, VisualTestContext};
    use language::{Language, LanguageConfig, LanguageMatcher, Point};
    use project::{ContextProviderWithTasks, FakeFs, Project};
    use serde_json::json;
    use task::TaskTemplates;
    use util::path;
    use workspace::{CloseInactiveTabsAndPanes, OpenOptions, OpenVisible};

    use crate::{modal::Spawn, tests::init_test};

    use super::*;

    #[gpui::test]
    async fn test_spawn_tasks_modal_query_reuse(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                ".zed": {
                    "tasks.json": r#"[
                        {
                            "label": "example task",
                            "command": "echo",
                            "args": ["4"]
                        },
                        {
                            "label": "another one",
                            "command": "echo",
                            "args": ["55"]
                        },
                    ]"#,
                },
                "a.ts": "a"
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            query(&tasks_picker, cx),
            "",
            "Initial query should be empty"
        );
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["another one", "example task"],
            "With no global tasks and no open item, a single worktree should be used and its tasks listed"
        );
        drop(tasks_picker);

        let _ = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path!("/dir/a.ts")),
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["another one", "example task"],
            "Initial tasks should be listed in alphabetical order"
        );

        let query_str = "tas";
        cx.simulate_input(query_str);
        assert_eq!(query(&tasks_picker, cx), query_str);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["example task"],
            "Only one task should match the query {query_str}"
        );

        cx.dispatch_action(picker::ConfirmCompletion);
        assert_eq!(
            query(&tasks_picker, cx),
            "echo 4",
            "Query should be set to the selected task's command"
        );
        assert_eq!(
            task_names(&tasks_picker, cx),
            Vec::<String>::new(),
            "No task should be listed"
        );
        cx.dispatch_action(picker::ConfirmInput { secondary: false });

        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            query(&tasks_picker, cx),
            "",
            "Query should be reset after confirming"
        );
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["echo 4", "another one", "example task"],
            "New oneshot task should be listed first"
        );

        let query_str = "echo 4";
        cx.simulate_input(query_str);
        assert_eq!(query(&tasks_picker, cx), query_str);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["echo 4"],
            "New oneshot should match custom command query"
        );

        cx.dispatch_action(picker::ConfirmInput { secondary: false });
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            query(&tasks_picker, cx),
            "",
            "Query should be reset after confirming"
        );
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![query_str, "another one", "example task"],
            "Last recently used one show task should be listed first"
        );

        cx.dispatch_action(picker::ConfirmCompletion);
        assert_eq!(
            query(&tasks_picker, cx),
            query_str,
            "Query should be set to the custom task's name"
        );
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![query_str],
            "Only custom task should be listed"
        );

        let query_str = "0";
        cx.simulate_input(query_str);
        assert_eq!(query(&tasks_picker, cx), "echo 40");
        assert_eq!(
            task_names(&tasks_picker, cx),
            Vec::<String>::new(),
            "New oneshot should not match any command query"
        );

        cx.dispatch_action(picker::ConfirmInput { secondary: true });
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            query(&tasks_picker, cx),
            "",
            "Query should be reset after confirming"
        );
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["echo 4", "another one", "example task"],
            "No query should be added to the list, as it was submitted with secondary action (that maps to omit_history = true)"
        );

        cx.dispatch_action(Spawn::ByName {
            task_name: "example task".to_string(),
            reveal_target: None,
        });
        let tasks_picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<TasksModal>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["echo 4", "another one", "example task"],
        );
    }

    #[gpui::test]
    async fn test_basic_context_for_simple_files(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                ".zed": {
                    "tasks.json": r#"[
                        {
                            "label": "hello from $ZED_FILE:$ZED_ROW:$ZED_COLUMN",
                            "command": "echo",
                            "args": ["hello", "from", "$ZED_FILE", ":", "$ZED_ROW", ":", "$ZED_COLUMN"]
                        },
                        {
                            "label": "opened now: $ZED_WORKTREE_ROOT",
                            "command": "echo",
                            "args": ["opened", "now:", "$ZED_WORKTREE_ROOT"]
                        }
                    ]"#,
                },
                "file_without_extension": "aaaaaaaaaaaaaaaaaaaa\naaaaaaaaaaaaaaaaaa",
                "file_with.odd_extension": "b",
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![concat!("opened now: ", path!("/dir")).to_string()],
            "When no file is open for a single worktree, should autodetect all worktree-related tasks"
        );
        tasks_picker.update(cx, |_, cx| {
            cx.emit(DismissEvent);
        });
        drop(tasks_picker);
        cx.executor().run_until_parked();

        let _ = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path!("/dir/file_with.odd_extension")),
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        cx.executor().run_until_parked();
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![
                concat!("hello from ", path!("/dir/file_with.odd_extension:1:1")).to_string(),
                concat!("opened now: ", path!("/dir")).to_string(),
            ],
            "Second opened buffer should fill the context, labels should be trimmed if long enough"
        );
        tasks_picker.update(cx, |_, cx| {
            cx.emit(DismissEvent);
        });
        drop(tasks_picker);
        cx.executor().run_until_parked();

        let second_item = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path!("/dir/file_without_extension")),
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .unwrap();

        let editor = cx
            .update(|_window, cx| second_item.act_as::<Editor>(cx))
            .unwrap();
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(None, window, cx, |s| {
                s.select_ranges(Some(Point::new(1, 2)..Point::new(1, 5)))
            })
        });
        cx.executor().run_until_parked();
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![
                concat!("hello from ", path!("/dir/file_without_extension:2:3")).to_string(),
                concat!("opened now: ", path!("/dir")).to_string(),
            ],
            "Opened buffer should fill the context, labels should be trimmed if long enough"
        );
        tasks_picker.update(cx, |_, cx| {
            cx.emit(DismissEvent);
        });
        drop(tasks_picker);
        cx.executor().run_until_parked();
    }

    #[gpui::test]
    async fn test_language_task_filtering(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "a1.ts": "// a1",
                "a2.ts": "// a2",
                "b.rs": "// b",
            }),
        )
        .await;

        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            let language_registry = project.languages();
            language_registry.add(Arc::new(
                Language::new(
                    LanguageConfig {
                        name: "TypeScript".into(),
                        matcher: LanguageMatcher {
                            path_suffixes: vec!["ts".to_string()],
                            ..LanguageMatcher::default()
                        },
                        ..LanguageConfig::default()
                    },
                    None,
                )
                .with_context_provider(Some(Arc::new(
                    ContextProviderWithTasks::new(TaskTemplates(vec![
                        TaskTemplate {
                            label: "Task without variables".to_string(),
                            command: "npm run clean".to_string(),
                            ..TaskTemplate::default()
                        },
                        TaskTemplate {
                            label: "TypeScript task from file $ZED_FILE".to_string(),
                            command: "npm run build".to_string(),
                            ..TaskTemplate::default()
                        },
                        TaskTemplate {
                            label: "Another task from file $ZED_FILE".to_string(),
                            command: "npm run lint".to_string(),
                            ..TaskTemplate::default()
                        },
                    ])),
                ))),
            ));
            language_registry.add(Arc::new(
                Language::new(
                    LanguageConfig {
                        name: "Rust".into(),
                        matcher: LanguageMatcher {
                            path_suffixes: vec!["rs".to_string()],
                            ..LanguageMatcher::default()
                        },
                        ..LanguageConfig::default()
                    },
                    None,
                )
                .with_context_provider(Some(Arc::new(
                    ContextProviderWithTasks::new(TaskTemplates(vec![TaskTemplate {
                        label: "Rust task".to_string(),
                        command: "cargo check".into(),
                        ..TaskTemplate::default()
                    }])),
                ))),
            ));
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let _ts_file_1 = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path!("/dir/a1.ts")),
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![
                concat!("Another task from file ", path!("/dir/a1.ts")),
                concat!("TypeScript task from file ", path!("/dir/a1.ts")),
                "Task without variables",
            ],
            "Should open spawn TypeScript tasks for the opened file, tasks with most template variables above, all groups sorted alphanumerically"
        );

        emulate_task_schedule(
            tasks_picker,
            &project,
            concat!("TypeScript task from file ", path!("/dir/a1.ts")),
            cx,
        );

        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![
                concat!("TypeScript task from file ", path!("/dir/a1.ts")),
                concat!("Another task from file ", path!("/dir/a1.ts")),
                "Task without variables",
            ],
            "After spawning the task and getting it into the history, it should be up in the sort as recently used.
            Tasks with the same labels and context are deduplicated."
        );
        tasks_picker.update(cx, |_, cx| {
            cx.emit(DismissEvent);
        });
        drop(tasks_picker);
        cx.executor().run_until_parked();

        let _ts_file_2 = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path!("/dir/a2.ts")),
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![
                concat!("TypeScript task from file ", path!("/dir/a1.ts")),
                concat!("Another task from file ", path!("/dir/a2.ts")),
                concat!("TypeScript task from file ", path!("/dir/a2.ts")),
                "Task without variables",
            ],
            "Even when both TS files are open, should only show the history (on the top), and tasks, resolved for the current file"
        );
        tasks_picker.update(cx, |_, cx| {
            cx.emit(DismissEvent);
        });
        drop(tasks_picker);
        cx.executor().run_until_parked();

        let _rs_file = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path!("/dir/b.rs")),
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec!["Rust task"],
            "Even when both TS files are open and one TS task spawned, opened file's language tasks should be displayed only"
        );

        cx.dispatch_action(CloseInactiveTabsAndPanes::default());
        emulate_task_schedule(tasks_picker, &project, "Rust task", cx);
        let _ts_file_2 = workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    PathBuf::from(path!("/dir/a2.ts")),
                    OpenOptions {
                        visible: Some(OpenVisible::All),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .await
            .unwrap();
        let tasks_picker = open_spawn_tasks(&workspace, cx);
        assert_eq!(
            task_names(&tasks_picker, cx),
            vec![
                concat!("TypeScript task from file ", path!("/dir/a1.ts")),
                concat!("Another task from file ", path!("/dir/a2.ts")),
                concat!("TypeScript task from file ", path!("/dir/a2.ts")),
                "Task without variables",
            ],
            "After closing all but *.rs tabs, running a Rust task and switching back to TS tasks, \
            same TS spawn history should be restored"
        );
    }

    fn emulate_task_schedule(
        tasks_picker: Entity<Picker<TasksModalDelegate>>,
        project: &Entity<Project>,
        scheduled_task_label: &str,
        cx: &mut VisualTestContext,
    ) {
        let scheduled_task = tasks_picker.update(cx, |tasks_picker, _| {
            tasks_picker
                .delegate
                .candidates
                .iter()
                .flatten()
                .find(|(_, task)| task.resolved_label == scheduled_task_label)
                .cloned()
                .unwrap()
        });
        project.update(cx, |project, cx| {
            if let Some(task_inventory) = project.task_store().read(cx).task_inventory().cloned() {
                task_inventory.update(cx, |inventory, _| {
                    let (kind, task) = scheduled_task;
                    inventory.task_scheduled(kind, task);
                });
            }
        });
        tasks_picker.update(cx, |_, cx| {
            cx.emit(DismissEvent);
        });
        drop(tasks_picker);
        cx.executor().run_until_parked()
    }

    fn open_spawn_tasks(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<TasksModalDelegate>> {
        cx.dispatch_action(Spawn::modal());
        workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<TasksModal>(cx)
                .expect("no task modal after `Spawn` action was dispatched")
                .read(cx)
                .picker
                .clone()
        })
    }

    fn query(
        spawn_tasks: &Entity<Picker<TasksModalDelegate>>,
        cx: &mut VisualTestContext,
    ) -> String {
        spawn_tasks.update(cx, |spawn_tasks, cx| spawn_tasks.query(cx))
    }

    fn task_names(
        spawn_tasks: &Entity<Picker<TasksModalDelegate>>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        spawn_tasks.update(cx, |spawn_tasks, _| {
            spawn_tasks
                .delegate
                .matches
                .iter()
                .map(|hit| hit.string.clone())
                .collect::<Vec<_>>()
        })
    }
}
