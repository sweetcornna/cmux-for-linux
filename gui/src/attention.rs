//! Notification and agent attention derived from session resources.

use std::collections::HashMap;

use cmux::{AgentSnapshot, AgentState, NotificationLevel, NotificationSnapshot, TerminalId};

use crate::screen::TabContent;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttentionIndicator {
    pub notification: Option<NotificationLevel>,
    pub agent: Option<AgentState>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttentionState {
    terminals: HashMap<TerminalId, AttentionIndicator>,
}

impl AttentionState {
    pub fn from_resources<'a>(
        notifications: impl IntoIterator<Item = &'a NotificationSnapshot>,
        agents: impl IntoIterator<Item = &'a AgentSnapshot>,
    ) -> Self {
        let mut terminals = HashMap::<TerminalId, AttentionIndicator>::new();
        for notification in notifications {
            let Some(terminal) = notification
                .terminal_id
                .as_ref()
                .filter(|_| notification.unread)
            else {
                continue;
            };
            let indicator = terminals.entry(terminal.clone()).or_default();
            indicator.notification = highest_severity(indicator.notification, notification.level);
        }

        let mut latest_agents = HashMap::<TerminalId, (u64, &str, AgentState)>::new();
        for agent in agents {
            let next = (agent.updated_at_ms, agent.id.as_str(), agent.state);
            let latest = latest_agents
                .entry(agent.terminal_id.clone())
                .or_insert(next);
            if (next.0, next.1) >= (latest.0, latest.1) {
                *latest = next;
            }
        }
        for (terminal, (_, _, state)) in latest_agents {
            terminals.entry(terminal).or_default().agent = Some(state);
        }

        Self { terminals }
    }

    pub fn terminal(&self, terminal: &TerminalId) -> AttentionIndicator {
        self.terminals.get(terminal).copied().unwrap_or_default()
    }
}

pub fn tab_indicator(state: &AttentionState, content: &TabContent) -> AttentionIndicator {
    match content {
        TabContent::Terminal(terminal) => state.terminal(terminal),
        TabContent::Browser => AttentionIndicator::default(),
    }
}

pub fn screen_indicator(state: &AttentionState, terminals: &[TerminalId]) -> AttentionIndicator {
    AttentionIndicator {
        notification: rollup_notification(state, terminals),
        agent: None,
    }
}

pub fn workspace_indicator(state: &AttentionState, terminals: &[TerminalId]) -> AttentionIndicator {
    AttentionIndicator {
        notification: rollup_notification(state, terminals),
        agent: None,
    }
}

fn rollup_notification(
    state: &AttentionState,
    terminals: &[TerminalId],
) -> Option<NotificationLevel> {
    terminals.iter().fold(None, |current, terminal| {
        highest_severity(current, state.terminal(terminal).notification)
    })
}

fn highest_severity(
    current: Option<NotificationLevel>,
    candidate: impl Into<Option<NotificationLevel>>,
) -> Option<NotificationLevel> {
    let candidate = candidate.into();
    match (current, candidate) {
        (None, next) => next,
        (current, None) => current,
        (Some(current), Some(candidate)) => {
            Some(if severity_rank(candidate) > severity_rank(current) {
                candidate
            } else {
                current
            })
        }
    }
}

const fn severity_rank(level: NotificationLevel) -> u8 {
    match level {
        NotificationLevel::Info => 0,
        NotificationLevel::Warning => 1,
        NotificationLevel::Error => 2,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cmux::{AgentId, AgentSnapshotSource, NotificationId, SessionId};

    use super::*;

    fn terminal(value: u128) -> TerminalId {
        TerminalId::parse(format!("term_{value:032x}")).unwrap()
    }

    fn notification(
        value: u128,
        terminal_id: Option<TerminalId>,
        level: NotificationLevel,
        unread: bool,
    ) -> NotificationSnapshot {
        NotificationSnapshot {
            id: NotificationId::parse(format!("notification_{value:032x}")).unwrap(),
            session_id: SessionId::parse(format!("session_{:032x}", 1)).unwrap(),
            title: "title".to_string(),
            body: "body".to_string(),
            level,
            terminal_id,
            created_at_ms: value as u64,
            unread,
            extra: BTreeMap::new(),
        }
    }

    fn agent(
        value: u128,
        terminal_id: TerminalId,
        state: AgentState,
        updated: u64,
    ) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::parse(format!("agent_{value:032x}")).unwrap(),
            session_id: SessionId::parse(format!("session_{:032x}", 1)).unwrap(),
            terminal_id,
            state,
            source: AgentSnapshotSource::Socket,
            updated_at_ms: updated,
            source_session: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn only_unread_terminal_notifications_mark_a_tab() {
        let first = terminal(1);
        let resources = [
            notification(1, Some(first.clone()), NotificationLevel::Warning, true),
            notification(2, Some(first.clone()), NotificationLevel::Error, false),
            notification(3, None, NotificationLevel::Error, true),
        ];
        let state = AttentionState::from_resources(resources.iter(), std::iter::empty());

        assert_eq!(
            tab_indicator(&state, &TabContent::Terminal(first)),
            AttentionIndicator {
                notification: Some(NotificationLevel::Warning),
                agent: None,
            }
        );
        assert_eq!(
            tab_indicator(&state, &TabContent::Browser),
            AttentionIndicator::default()
        );
    }

    #[test]
    fn screen_and_workspace_rollups_use_the_highest_severity() {
        let first = terminal(1);
        let second = terminal(2);
        let third = terminal(3);
        let resources = [
            notification(1, Some(first.clone()), NotificationLevel::Info, true),
            notification(2, Some(second.clone()), NotificationLevel::Error, true),
            notification(3, Some(third.clone()), NotificationLevel::Warning, true),
        ];
        let state = AttentionState::from_resources(resources.iter(), std::iter::empty());

        assert_eq!(
            screen_indicator(&state, &[first.clone(), third]).notification,
            Some(NotificationLevel::Warning)
        );
        assert_eq!(
            workspace_indicator(&state, &[first, second]).notification,
            Some(NotificationLevel::Error)
        );
    }

    #[test]
    fn newest_agent_report_is_the_current_tab_state() {
        let first = terminal(1);
        let reports = [
            agent(1, first.clone(), AgentState::Working, 20),
            agent(2, first.clone(), AgentState::Blocked, 40),
            agent(3, first.clone(), AgentState::Done, 30),
        ];
        let state = AttentionState::from_resources(std::iter::empty(), reports.iter());

        assert_eq!(
            tab_indicator(&state, &TabContent::Terminal(first)).agent,
            Some(AgentState::Blocked)
        );
    }
}
