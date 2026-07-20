//! Keyboard navigation for full workflow runs and inspectable agent leaves.

use super::WorkflowMonitor;
use super::WorkflowTopologyNode;
use crate::app_event::AppEvent;
use crate::chatwidget::ChatWidget;
use codex_app_server_protocol::WorkflowAgentControlAction;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;

pub(super) const MAX_VISIBLE_RUNS: usize = 3;
pub(super) const MAX_VISIBLE_NODES_PER_RUN: usize = 24;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkflowMonitorSelection {
    pub(super) run_id: String,
    pub(super) node_id: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkflowDrillTarget {
    selection: WorkflowMonitorSelection,
    child_thread_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkflowMonitorTarget {
    Run(String),
    Agent(WorkflowMonitorSelection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetActivity {
    ActiveOnly,
    Any,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionDirection {
    Previous,
    Next,
}

impl WorkflowMonitor {
    pub(super) fn visible_run_indexes(&self) -> Vec<usize> {
        let mut selected_indexes = self
            .runs
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, run)| run.is_effectively_running())
            .take(MAX_VISIBLE_RUNS)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let remaining_slots = MAX_VISIBLE_RUNS.saturating_sub(selected_indexes.len());
        selected_indexes.extend(
            self.runs
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, run)| !run.is_effectively_running())
                .take(remaining_slots)
                .map(|(index, _)| index),
        );
        let focused_run_id = self
            .selected_run_id
            .as_ref()
            .or_else(|| self.selection.as_ref().map(|selection| &selection.run_id));
        if let Some(focused_run_id) = focused_run_id
            && let Some(focused_index) = self
                .runs
                .iter()
                .position(|run| &run.model.run_id == focused_run_id)
            && !selected_indexes.contains(&focused_index)
        {
            if selected_indexes.len() >= MAX_VISIBLE_RUNS {
                selected_indexes.remove(/*index*/ 0);
            }
            selected_indexes.push(focused_index);
        }
        selected_indexes.sort_unstable();
        selected_indexes
    }

    pub(super) fn has_focus_targets(&self) -> bool {
        !self.runs.is_empty()
    }

    pub(super) fn selection(&self) -> Option<&WorkflowMonitorSelection> {
        self.selection.as_ref()
    }

    pub(super) fn selected_run_id(&self) -> Option<&str> {
        self.selected_run_id.as_deref()
    }

    pub(super) fn focus_hint(&self) -> &'static str {
        if self.selected_run_id.is_some() {
            if self.selected_run_can_stop() && self.selected_run_can_pause() {
                "↑↓ choose run/agent · x stop workflow · p pause · Enter inspect agents · Esc exit"
            } else if self.selected_run_can_resume() {
                "↑↓ choose run/agent · r resume · Enter inspect agents · Esc exit"
            } else if self.selected_run_can_save() {
                "↑↓ choose run/agent · s save · Enter inspect agents · Esc exit"
            } else {
                "↑↓ choose run/agent · Enter inspect agents · Esc exit"
            }
        } else if self.selection.is_some() {
            if self.selected_agent_can_control() {
                "↑↓ choose · x skip · r retry · Enter inspect agent · Esc exit"
            } else {
                "↑↓ choose · Enter inspect agent · Esc exit"
            }
        } else {
            "Enter inspect agents"
        }
    }

    fn focus_preferred_target(&mut self) -> bool {
        let visible_run_indexes = self.visible_run_indexes();
        let preferred = visible_run_indexes
            .iter()
            .rev()
            .find_map(|run_index| self.first_target_for_run(*run_index, TargetActivity::ActiveOnly))
            .or_else(|| {
                visible_run_indexes.iter().rev().find_map(|run_index| {
                    self.first_target_for_run(*run_index, TargetActivity::Any)
                })
            });
        if let Some(preferred) = preferred {
            self.selection = Some(preferred);
            self.selected_run_id = None;
            return true;
        }
        self.selection = None;
        self.selected_run_id = visible_run_indexes
            .iter()
            .rev()
            .find_map(|index| {
                self.runs
                    .get(*index)
                    .filter(|run| run.is_effectively_running())
            })
            .or_else(|| {
                visible_run_indexes
                    .iter()
                    .rev()
                    .find_map(|index| self.runs.get(*index))
            })
            .map(|run| run.model.run_id.clone());
        self.selected_run_id.is_some()
    }

    fn first_target_for_run(
        &self,
        run_index: usize,
        target_activity: TargetActivity,
    ) -> Option<WorkflowMonitorSelection> {
        let run = self.runs.get(run_index)?;
        if target_activity == TargetActivity::ActiveOnly && !run.is_effectively_running() {
            return None;
        }
        run.model
            .topology_order
            .iter()
            .filter_map(|node_id| {
                let WorkflowTopologyNode::Agent(agent) = run.model.topology.get(node_id)? else {
                    return None;
                };
                if target_activity == TargetActivity::ActiveOnly
                    && !matches!(
                        agent.status,
                        AgentStatus::PendingInit | AgentStatus::Running
                    )
                {
                    return None;
                }
                Some(WorkflowMonitorSelection {
                    run_id: run.model.run_id.clone(),
                    node_id: agent.id,
                })
            })
            .next()
    }

    fn agent_targets(&self) -> impl Iterator<Item = WorkflowMonitorSelection> + '_ {
        self.runs.iter().flat_map(|run| {
            run.model.topology_order.iter().filter_map(|node_id| {
                let WorkflowTopologyNode::Agent(agent) = run.model.topology.get(node_id)? else {
                    return None;
                };
                Some(WorkflowMonitorSelection {
                    run_id: run.model.run_id.clone(),
                    node_id: agent.id,
                })
            })
        })
    }

    fn monitor_targets(&self) -> Vec<WorkflowMonitorTarget> {
        self.runs
            .iter()
            .flat_map(|run| {
                std::iter::once(WorkflowMonitorTarget::Run(run.model.run_id.clone())).chain(
                    run.model.topology_order.iter().filter_map(|node_id| {
                        let WorkflowTopologyNode::Agent(agent) = run.model.topology.get(node_id)?
                        else {
                            return None;
                        };
                        Some(WorkflowMonitorTarget::Agent(WorkflowMonitorSelection {
                            run_id: run.model.run_id.clone(),
                            node_id: agent.id,
                        }))
                    }),
                )
            })
            .collect()
    }

    fn selected_monitor_target(&self) -> Option<WorkflowMonitorTarget> {
        if let Some(run_id) = self.selected_run_id.as_ref() {
            return Some(WorkflowMonitorTarget::Run(run_id.clone()));
        }
        self.selection.clone().map(WorkflowMonitorTarget::Agent)
    }

    fn set_monitor_target(&mut self, target: WorkflowMonitorTarget) {
        match target {
            WorkflowMonitorTarget::Run(run_id) => {
                self.selection = None;
                self.selected_run_id = Some(run_id);
            }
            WorkflowMonitorTarget::Agent(selection) => {
                self.selection = Some(selection);
                self.selected_run_id = None;
            }
        }
    }

    fn move_selection(&mut self, direction: SelectionDirection) {
        let targets = self.monitor_targets();
        let Some(selected) = self.selected_monitor_target() else {
            return;
        };
        let Some(index) = targets.iter().position(|target| target == &selected) else {
            self.focus_preferred_target();
            return;
        };
        let next_index = match direction {
            SelectionDirection::Previous => index.saturating_sub(1),
            SelectionDirection::Next => {
                index.saturating_add(1).min(targets.len().saturating_sub(1))
            }
        };
        if let Some(target) = targets.get(next_index) {
            self.set_monitor_target(target.clone());
        }
    }

    fn selected_drill_target(&self) -> Option<WorkflowDrillTarget> {
        let selected = self.selection.as_ref()?;
        let run = self
            .runs
            .iter()
            .find(|run| run.model.run_id == selected.run_id)?;
        let WorkflowTopologyNode::Agent(agent) = run.model.topology.get(&selected.node_id)? else {
            return None;
        };
        Some(WorkflowDrillTarget {
            selection: selected.clone(),
            child_thread_id: agent.child_thread_id.clone()?,
        })
    }

    fn focus_selected_run_agent(&mut self) -> bool {
        let Some(run_id) = self.selected_run_id.as_deref() else {
            return false;
        };
        let Some(run_index) = self.runs.iter().position(|run| run.model.run_id == run_id) else {
            return false;
        };
        let target = self
            .first_target_for_run(run_index, TargetActivity::ActiveOnly)
            .or_else(|| self.first_target_for_run(run_index, TargetActivity::Any));
        let Some(target) = target else {
            return false;
        };
        self.selection = Some(target);
        self.selected_run_id = None;
        true
    }

    fn is_focused(&self) -> bool {
        self.selection.is_some() || self.selected_run_id.is_some()
    }

    fn clear_focus(&mut self) {
        self.selection = None;
        self.selected_run_id = None;
    }

    pub(super) fn restore_focus(&mut self, run_id: &str, node_id: u64) -> bool {
        let selection = WorkflowMonitorSelection {
            run_id: run_id.to_string(),
            node_id,
        };
        if self.agent_targets().any(|target| target == selection) {
            self.selection = Some(selection);
            self.selected_run_id = None;
            true
        } else {
            false
        }
    }

    pub(super) fn reconcile_selection(&mut self) {
        if let Some(selected_run_id) = self.selected_run_id.as_ref() {
            if self
                .runs
                .iter()
                .any(|run| &run.model.run_id == selected_run_id)
            {
                return;
            }
            self.selected_run_id = None;
        }
        let Some(selection) = self.selection.clone() else {
            return;
        };
        if !self.agent_targets().any(|target| target == selection) {
            self.focus_preferred_target();
        }
    }
}

impl ChatWidget {
    pub(crate) fn handle_workflow_monitor_key_event(&mut self, key_event: KeyEvent) -> bool {
        if !self.bottom_pane.no_modal_or_popup_active()
            || !self.bottom_pane.composer_is_empty()
            || !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            return false;
        }

        if !self.workflow_monitor.is_focused() {
            if key_event.code == KeyCode::Enter
                && key_event.kind == KeyEventKind::Press
                && self.workflow_monitor.focus_preferred_target()
            {
                self.request_redraw();
                return true;
            }
            return false;
        }

        match key_event.code {
            KeyCode::Up => self
                .workflow_monitor
                .move_selection(SelectionDirection::Previous),
            KeyCode::Down => self
                .workflow_monitor
                .move_selection(SelectionDirection::Next),
            KeyCode::Char('s') => {
                if self.workflow_monitor.selected_run_id().is_some() {
                    if key_event.kind == KeyEventKind::Press {
                        self.open_workflow_save_dialog();
                    }
                    return true;
                }
                self.workflow_monitor.clear_focus();
                self.request_redraw();
                return false;
            }
            KeyCode::Char('x') => {
                if self.workflow_monitor.selected_run_id().is_some() {
                    if key_event.kind == KeyEventKind::Press {
                        self.open_workflow_stop_confirmation();
                    }
                    return true;
                }
                if self.workflow_monitor.selection().is_some() {
                    if key_event.kind == KeyEventKind::Press {
                        self.open_workflow_agent_control_confirmation(
                            WorkflowAgentControlAction::Skip,
                        );
                    }
                    return true;
                }
            }
            KeyCode::Char('p') => {
                if self.workflow_monitor.selected_run_id().is_some() {
                    if key_event.kind == KeyEventKind::Press {
                        self.open_workflow_pause_confirmation();
                    }
                    return true;
                }
            }
            KeyCode::Char('r') => {
                if self.workflow_monitor.selected_run_id().is_some() {
                    if key_event.kind == KeyEventKind::Press {
                        self.request_selected_workflow_resume();
                    }
                    return true;
                }
                if self.workflow_monitor.selection().is_some() {
                    if key_event.kind == KeyEventKind::Press {
                        self.open_workflow_agent_control_confirmation(
                            WorkflowAgentControlAction::Retry,
                        );
                    }
                    return true;
                }
            }
            KeyCode::Enter if key_event.kind == KeyEventKind::Press => {
                if self.workflow_monitor.selected_run_id().is_some() {
                    self.workflow_monitor.focus_selected_run_agent();
                    self.request_redraw();
                    return true;
                }
                let Some(target) = self.workflow_monitor.selected_drill_target() else {
                    return true;
                };
                match ThreadId::from_string(&target.child_thread_id) {
                    Ok(thread_id) => self.app_event_tx.send(AppEvent::SelectWorkflowAgentThread {
                        thread_id,
                        run_id: target.selection.run_id,
                        node_id: target.selection.node_id,
                    }),
                    Err(error) => {
                        tracing::warn!(
                            child_thread_id = target.child_thread_id,
                            %error,
                            "ignoring invalid workflow child thread id"
                        );
                        self.add_error_message(
                            "This workflow agent has an invalid child thread id.".to_string(),
                        );
                    }
                }
                return true;
            }
            KeyCode::Esc if key_event.kind == KeyEventKind::Press => {
                self.workflow_monitor.clear_focus();
            }
            _ => {
                self.workflow_monitor.clear_focus();
                self.request_redraw();
                return false;
            }
        }
        self.request_redraw();
        true
    }

    pub(crate) fn restore_workflow_monitor_focus(&mut self, run_id: &str, node_id: u64) -> bool {
        let restored = self.workflow_monitor.restore_focus(run_id, node_id);
        if restored {
            self.request_redraw();
        }
        restored
    }

    #[cfg(test)]
    pub(crate) fn workflow_monitor_focus(&self) -> Option<(&str, u64)> {
        self.workflow_monitor
            .selection()
            .map(|selection| (selection.run_id.as_str(), selection.node_id))
    }

    #[cfg(test)]
    pub(crate) fn workflow_monitor_text(&self, width: u16) -> String {
        self.workflow_monitor
            .display_lines(width)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
