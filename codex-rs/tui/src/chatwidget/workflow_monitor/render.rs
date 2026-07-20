//! Compact ratatui renderer for the live workflow projection.

use super::MonitoredRun;
use super::SummarizedRun;
use super::SummarizedRunStatus;
use super::WorkflowMonitor;
use super::agent_control::WorkflowAgentControlRequestState;
use super::navigation::MAX_VISIBLE_NODES_PER_RUN;
use super::navigation::WorkflowMonitorSelection;
use super::render_controls::agent_control_status;
use super::render_controls::render_run_controls;
use crate::render::renderable::Renderable;
use crate::status::format_tokens_compact;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_core_workflows::WorkflowAgent;
use codex_core_workflows::WorkflowGroup;
use codex_core_workflows::WorkflowNodeState;
use codex_core_workflows::WorkflowPhaseState;
use codex_core_workflows::WorkflowRunState;
use codex_core_workflows::WorkflowTopologyNode;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_utils_elapsed::format_duration;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::collections::HashSet;
use std::time::Duration;

const MAX_VISIBLE_PHASES: usize = 8;

impl WorkflowMonitor {
    pub(super) fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let selected_indexes = self.visible_run_indexes();

        let omitted_runs = self.runs.len().saturating_sub(selected_indexes.len());
        if omitted_runs > 0 {
            let noun = if omitted_runs == 1 { "run" } else { "runs" };
            push_line(
                &mut lines,
                vec![format!("  … {omitted_runs} workflow {noun} omitted").dim()].into(),
                width,
                "  ",
            );
        }

        for (index, run_index) in selected_indexes.into_iter().enumerate() {
            if index > 0 || omitted_runs > 0 {
                lines.push(Line::default());
            }
            if let Some(run) = self.runs.get(run_index) {
                render_run(
                    &mut lines,
                    run,
                    self.selection(),
                    self.selected_run_id(),
                    width,
                );
            }
        }
        if !self.summarized_runs.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            for summary in &self.summarized_runs {
                render_summarized_run(&mut lines, summary, width);
            }
        }
        if self.saturated_run_count > 0 {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let noun = if self.saturated_run_count == 1 {
                "run"
            } else {
                "runs"
            };
            let latest = self
                .saturated_run_ids
                .back()
                .map(|run_id| format!(" · latest {}", compact_run_id(run_id)))
                .unwrap_or_default();
            push_line(
                &mut lines,
                vec![
                    format!(
                        "  … {} additional workflow {noun} omitted at monitor capacity{latest}",
                        self.saturated_run_count
                    )
                    .magenta(),
                ]
                .into(),
                width,
                "  ",
            );
        }
        if self.has_focus_targets() {
            lines.push(Line::default());
            push_line(
                &mut lines,
                vec!["  ".into(), self.focus_hint().dim()].into(),
                width,
                "  ",
            );
        }
        lines
    }
}

fn render_summarized_run(lines: &mut Vec<Line<'static>>, run: &SummarizedRun, width: u16) {
    let name = if run.name.trim().is_empty() {
        "unnamed".to_string()
    } else {
        run.name.clone()
    };
    let status = match &run.status {
        SummarizedRunStatus::Starting => "starting".dim(),
        SummarizedRunStatus::Running => "running".cyan(),
        SummarizedRunStatus::Reconciling => "unknown".red(),
        SummarizedRunStatus::Reconciled(status) => reconciled_status_label(*status),
        SummarizedRunStatus::Completed {
            status,
            terminal_reason,
        } => run_status_label(status, *terminal_reason),
    };
    push_line(
        lines,
        vec![
            "◇ ".magenta(),
            "Workflow ".bold(),
            name.into(),
            "  ".into(),
            compact_run_id(&run.run_id).dim(),
            "  compact · ".dim(),
            status,
        ]
        .into(),
        width,
        "  ",
    );
}

impl Renderable for WorkflowMonitor {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        Paragraph::new(Text::from(self.display_lines(area.width))).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.display_lines(width)
            .len()
            .try_into()
            .unwrap_or(u16::MAX)
    }
}

fn render_run(
    lines: &mut Vec<Line<'static>>,
    run: &MonitoredRun,
    selection: Option<&WorkflowMonitorSelection>,
    selected_run_id: Option<&str>,
    width: u16,
) {
    let model = &run.model;
    let name = if model.name.trim().is_empty() {
        "unnamed".to_string()
    } else {
        model.name.clone()
    };
    let mut header = vec![
        if selected_run_id == Some(model.run_id.as_str()) {
            "› ".cyan()
        } else {
            "".into()
        },
        "◆ ".magenta(),
        "Workflow ".bold(),
        name.into(),
        "  ".into(),
        compact_run_id(&model.run_id).dim(),
        "  ".into(),
        run.reconciled_status.map_or_else(
            || run_status_label(&model.status, model.terminal_reason),
            reconciled_status_label,
        ),
    ];
    if let Some(resumed_from_run_id) = &model.resumed_from_run_id {
        header.extend([
            " · resumed from ".dim(),
            compact_run_id(resumed_from_run_id).dim(),
        ]);
    }
    if let Some(timing) = run.timing.run_display(run.effective_state()) {
        header.extend([" · ".dim(), timing.dim()]);
    }
    push_line(lines, header.into(), width, "  ");
    render_run_controls(lines, run, width);

    if !model.topology.is_empty() {
        push_line(
            lines,
            vec![
                "  ".into(),
                format!(
                    "{} tokens · {} {}",
                    format_tokens_compact(model.aggregate.token_usage.total_tokens),
                    model.aggregate.tool_call_count,
                    plural(model.aggregate.tool_call_count, "tool", "tools")
                )
                .dim(),
            ]
            .into(),
            width,
            "  ",
        );
    }

    let phase_focus = model
        .phases
        .iter()
        .position(|phase| phase.state == WorkflowPhaseState::Active)
        .unwrap_or_else(|| model.phases.len().saturating_sub(1));
    let phase_start = phase_focus
        .saturating_sub(MAX_VISIBLE_PHASES / 2)
        .min(model.phases.len().saturating_sub(MAX_VISIBLE_PHASES));
    let phase_end = phase_start
        .saturating_add(MAX_VISIBLE_PHASES)
        .min(model.phases.len());
    let mut visited = HashSet::new();
    let mut visible_nodes = 0;
    if phase_start > 0 {
        push_line(
            lines,
            vec![format!("  … {phase_start} earlier phases").dim()].into(),
            width,
            "  ",
        );
    }
    for phase in &model.phases[phase_start..phase_end] {
        push_line(
            lines,
            vec![
                "  ".into(),
                phase_icon(phase.state),
                " ".into(),
                phase.title.clone().into(),
                phase_summary(run, phase).dim(),
            ]
            .into(),
            width,
            "    ",
        );

        let roots = phase
            .root_node_ids
            .iter()
            .filter(|node_id| {
                model
                    .topology
                    .get(node_id)
                    .is_some_and(|node| node.phase_index() == phase.index)
            })
            .copied()
            .collect::<Vec<_>>();
        let root_count = roots.len();
        let mut renderer = NodeRenderer {
            lines,
            run,
            selection,
            width,
            visited: &mut visited,
            visible_nodes: &mut visible_nodes,
        };
        for (index, node_id) in roots.into_iter().enumerate() {
            renderer.render(node_id, phase.index, &[], index + 1 == root_count);
        }
    }
    if phase_end < model.phases.len() {
        push_line(
            lines,
            vec![format!("  … {} later phases", model.phases.len() - phase_end).dim()].into(),
            width,
            "  ",
        );
    }
    let omitted_nodes = model.topology.len().saturating_sub(visited.len());
    if omitted_nodes > 0 {
        push_line(
            lines,
            vec![format!("    … {omitted_nodes} more nodes").dim()].into(),
            width,
            "    ",
        );
    }
    if let Some(selection) = selection
        && selection.run_id == model.run_id
        && !visited.contains(&selection.node_id)
        && let Some(WorkflowTopologyNode::Agent(agent)) = model.topology.get(&selection.node_id)
    {
        push_line(
            lines,
            agent_line(
                "    … ".to_string(),
                agent,
                run.agent_control_requests.get(&agent.id),
                /*selected*/ true,
            ),
            width,
            "    ",
        );
    }

    for log in &run.logs {
        push_line(
            lines,
            vec!["  ↳ ".dim(), log.clone().dim()].into(),
            width,
            "    ",
        );
    }

    if let Some(budget) = model.budget {
        let budget_text = match budget.total {
            Some(total) => format!(
                "{} / {} weighted tokens",
                format_tokens_compact(budget.spent),
                format_tokens_compact(total)
            ),
            None => format!(
                "{} weighted tokens · unmetered",
                format_tokens_compact(budget.spent)
            ),
        };
        push_line(
            lines,
            vec!["  budget  ".bold(), budget_text.dim()].into(),
            width,
            "    ",
        );
    }
    if model.state == WorkflowRunState::Completed
        && let Some(message) = status_message(&model.status)
        && !message.is_empty()
    {
        let detail = match &model.status {
            AgentStatus::Errored(_) => message.to_string().red(),
            AgentStatus::PendingInit
            | AgentStatus::Running
            | AgentStatus::Interrupted
            | AgentStatus::Completed(_)
            | AgentStatus::Shutdown
            | AgentStatus::NotFound => message.to_string().dim(),
        };
        push_line(lines, vec!["  ↳ ".dim(), detail].into(), width, "    ");
    }
}

struct NodeRenderer<'a> {
    lines: &'a mut Vec<Line<'static>>,
    run: &'a MonitoredRun,
    selection: Option<&'a WorkflowMonitorSelection>,
    width: u16,
    visited: &'a mut HashSet<u64>,
    visible_nodes: &'a mut usize,
}

impl NodeRenderer<'_> {
    fn render(
        &mut self,
        node_id: u64,
        phase_index: u64,
        ancestor_has_sibling: &[bool],
        is_last: bool,
    ) {
        if *self.visible_nodes >= MAX_VISIBLE_NODES_PER_RUN || !self.visited.insert(node_id) {
            return;
        }
        let Some(node) = self.run.model.topology.get(&node_id) else {
            return;
        };
        *self.visible_nodes += 1;

        let prefix = tree_prefix(ancestor_has_sibling, is_last);
        let subsequent_indent = " ".repeat(prefix.chars().count());
        match node {
            WorkflowTopologyNode::Group(group) => push_line(
                self.lines,
                group_line(prefix, group),
                self.width,
                &subsequent_indent,
            ),
            WorkflowTopologyNode::Agent(agent) => {
                let selected = self.selection.is_some_and(|selection| {
                    selection.run_id == self.run.model.run_id && selection.node_id == agent.id
                });
                push_line(
                    self.lines,
                    agent_line(
                        prefix,
                        agent,
                        self.run.agent_control_requests.get(&agent.id),
                        selected,
                    ),
                    self.width,
                    &subsequent_indent,
                );
                if let Some(message) = status_message(&agent.status)
                    && !message.is_empty()
                    && matches!(&agent.status, AgentStatus::Errored(_))
                {
                    push_line(
                        self.lines,
                        vec![subsequent_indent.clone().into(), message.to_string().red()].into(),
                        self.width,
                        &subsequent_indent,
                    );
                }
            }
        }

        let children = node
            .child_node_ids()
            .iter()
            .filter(|child_id| {
                self.run
                    .model
                    .topology
                    .get(child_id)
                    .is_some_and(|child| child.phase_index() == phase_index)
            })
            .copied()
            .collect::<Vec<_>>();
        let child_count = children.len();
        let mut child_ancestors = ancestor_has_sibling.to_vec();
        child_ancestors.push(!is_last);
        for (index, child_id) in children.into_iter().enumerate() {
            self.render(
                child_id,
                phase_index,
                &child_ancestors,
                index + 1 == child_count,
            );
        }
    }
}

fn group_line(prefix: String, group: &WorkflowGroup) -> Line<'static> {
    let kind = match group.kind {
        WorkflowGroupKind::Parallel => "parallel",
        WorkflowGroupKind::Pipeline => "pipeline",
    };
    let empty = if group.item_count == 0 {
        " · empty"
    } else {
        ""
    };
    vec![
        prefix.into(),
        node_state_icon(group.state),
        " ".into(),
        kind.bold(),
        format!(
            " · {} {}{empty}",
            group.item_count,
            plural(group.item_count, "item", "items")
        )
        .dim(),
    ]
    .into()
}

fn agent_line(
    prefix: String,
    agent: &WorkflowAgent,
    control: Option<&WorkflowAgentControlRequestState>,
    selected: bool,
) -> Line<'static> {
    let prefix = if selected {
        format!("  › {}", prefix.trim_start())
    } else {
        prefix
    };
    let mut spans = vec![
        prefix.into(),
        status_icon(&agent.status),
        " ".into(),
        agent.label.clone().into(),
        format!("  {} · {}", agent.model, agent.effort).dim(),
        "  ".into(),
        status_label(&agent.status),
        " · ".dim(),
        format!("attempt {}", agent.attempt.saturating_add(1)).dim(),
    ];
    if let Some(reason) = agent.last_attempt_reason {
        spans.extend([" · ".dim(), agent_attempt_reason_label(reason)]);
    }
    if agent.returned_null {
        spans.extend([" · ".dim(), "null".red()]);
    }
    let activity = match (
        agent.token_usage.total_tokens > 0,
        agent.tool_call_count > 0,
    ) {
        (true, true) => Some(format!(
            "{} tok · {} {}",
            format_tokens_compact(agent.token_usage.total_tokens),
            agent.tool_call_count,
            plural(agent.tool_call_count, "tool", "tools")
        )),
        (true, false) => Some(format!(
            "{} tok",
            format_tokens_compact(agent.token_usage.total_tokens)
        )),
        (false, true) => Some(format!(
            "{} {}",
            agent.tool_call_count,
            plural(agent.tool_call_count, "tool", "tools")
        )),
        (false, false) => None,
    };
    if let Some(activity) = activity {
        spans.extend([" · ".dim(), activity.dim()]);
    }
    let duration = format_duration(Duration::from_millis(agent.duration_ms));
    let duration = match &agent.status {
        AgentStatus::PendingInit | AgentStatus::Running => format!("≥{duration}"),
        AgentStatus::Interrupted
        | AgentStatus::Completed(_)
        | AgentStatus::Errored(_)
        | AgentStatus::Shutdown
        | AgentStatus::NotFound => duration,
    };
    spans.extend([" · ".dim(), duration.dim()]);
    if let Some(control) = control {
        spans.extend([" · ".dim(), agent_control_status(control)]);
    }
    spans.into()
}

fn agent_attempt_reason_label(reason: WorkflowAgentAttemptReason) -> Span<'static> {
    match reason {
        WorkflowAgentAttemptReason::UserSkip => "user skip".dim(),
        WorkflowAgentAttemptReason::UserRetry => "user retry".dim(),
        WorkflowAgentAttemptReason::RetryLimitReached => "retry limit".red(),
    }
}

fn phase_summary(run: &MonitoredRun, phase: &codex_core_workflows::WorkflowPhase) -> Span<'static> {
    let mut details = Vec::new();
    if phase.aggregate.agent_count > 0 {
        details.push(format!(
            "{}/{} agents",
            phase.aggregate.completed_agent_count, phase.aggregate.agent_count
        ));
    }
    if let Some(timing) = run.timing.phase_display(phase.index, phase.state) {
        details.push(timing);
    }
    if details.is_empty() {
        "".into()
    } else {
        format!(" · {}", details.join(" · ")).into()
    }
}

fn phase_icon(state: WorkflowPhaseState) -> Span<'static> {
    match state {
        WorkflowPhaseState::Pending => "○".dim(),
        WorkflowPhaseState::Active => "●".cyan(),
        WorkflowPhaseState::Completed => "✓".green(),
    }
}

fn node_state_icon(state: WorkflowNodeState) -> Span<'static> {
    match state {
        WorkflowNodeState::Active => "●".cyan(),
        WorkflowNodeState::Completed => "✓".green(),
    }
}

fn status_icon(status: &AgentStatus) -> Span<'static> {
    match status {
        AgentStatus::PendingInit => "○".dim(),
        AgentStatus::Running => "●".cyan(),
        AgentStatus::Interrupted => "■".red(),
        AgentStatus::Completed(_) => "✓".green(),
        AgentStatus::Errored(_) => "×".red(),
        AgentStatus::Shutdown => "■".dim(),
        AgentStatus::NotFound => "?".red(),
    }
}

fn status_label(status: &AgentStatus) -> Span<'static> {
    match status {
        AgentStatus::PendingInit => "pending".dim(),
        AgentStatus::Running => "running".cyan(),
        AgentStatus::Interrupted => "interrupted".red(),
        AgentStatus::Completed(_) => "completed".green(),
        AgentStatus::Errored(_) => "errored".red(),
        AgentStatus::Shutdown => "shutdown".dim(),
        AgentStatus::NotFound => "not found".red(),
    }
}

fn run_status_label(
    status: &AgentStatus,
    terminal_reason: Option<WorkflowRunTerminalReason>,
) -> Span<'static> {
    match terminal_reason {
        Some(WorkflowRunTerminalReason::Completed) => "completed".green(),
        Some(WorkflowRunTerminalReason::Failed) => "failed".red(),
        Some(WorkflowRunTerminalReason::Interrupted) => "interrupted".red(),
        Some(WorkflowRunTerminalReason::Stopped) => "stopped".dim(),
        Some(WorkflowRunTerminalReason::Paused) => "paused".cyan(),
        None => match status {
            AgentStatus::Shutdown => "stopped".dim(),
            AgentStatus::PendingInit
            | AgentStatus::Running
            | AgentStatus::Interrupted
            | AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::NotFound => status_label(status),
        },
    }
}

fn reconciled_status_label(status: WorkflowRunStatus) -> Span<'static> {
    match status {
        WorkflowRunStatus::Running => "running".cyan(),
        WorkflowRunStatus::Completed => "completed".green(),
        WorkflowRunStatus::Stopped => "stopped".dim(),
        WorkflowRunStatus::Paused => "paused".cyan(),
        WorkflowRunStatus::Failed => "failed".red(),
        WorkflowRunStatus::Unknown => "unknown".red(),
    }
}

fn status_message(status: &AgentStatus) -> Option<&str> {
    match status {
        AgentStatus::Completed(message) => message.as_deref(),
        AgentStatus::Errored(message) => Some(message),
        AgentStatus::PendingInit
        | AgentStatus::Running
        | AgentStatus::Interrupted
        | AgentStatus::Shutdown
        | AgentStatus::NotFound => None,
    }
}

fn tree_prefix(ancestor_has_sibling: &[bool], is_last: bool) -> String {
    let mut prefix = "    ".to_string();
    for has_sibling in ancestor_has_sibling {
        prefix.push_str(if *has_sibling { "│ " } else { "  " });
    }
    prefix.push_str(if is_last { "└─ " } else { "├─ " });
    prefix
}

pub(super) fn compact_run_id(run_id: &str) -> String {
    if let Ok(run_uuid) = uuid::Uuid::parse_str(run_id) {
        let simple = run_uuid.simple().to_string();
        return format!("…{}", &simple[simple.len() - 8..]);
    }
    let compact = run_id
        .chars()
        .filter(|ch| !ch.is_control() && !ch.is_whitespace())
        .take(8)
        .collect::<String>();
    if compact.is_empty() {
        "unknown".to_string()
    } else {
        compact
    }
}

fn plural<'a>(count: u64, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

pub(super) fn push_line(
    lines: &mut Vec<Line<'static>>,
    line: Line<'static>,
    width: u16,
    subsequent_indent: &str,
) {
    lines.extend(word_wrap_lines(
        [line],
        RtOptions::new(usize::from(width.max(1)))
            .subsequent_indent(subsequent_indent.to_string().into()),
    ));
}
