use crate::adapter::codex::{CodexAdapter, parse_codex_event};
use crate::event::{AgentEvent, EventAdapter};
use crate::tmux::TRAEX_AGENT;
use serde_json::Value;

pub struct TraexAdapter;

impl TraexAdapter {
    pub const HOOK_REGISTRATIONS: &'static [super::HookRegistration] =
        CodexAdapter::HOOK_REGISTRATIONS;
}

impl EventAdapter for TraexAdapter {
    fn parse(&self, event_name: &str, input: &Value) -> Option<AgentEvent> {
        parse_codex_event(TRAEX_AGENT, event_name, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sets_traex_agent_name() {
        let event = TraexAdapter
            .parse("user-prompt-submit", &json!({"prompt": "hi"}))
            .unwrap();
        match event {
            AgentEvent::UserPromptSubmit { agent, .. } => assert_eq!(agent, TRAEX_AGENT),
            other => panic!("expected UserPromptSubmit, got {other:?}"),
        }
    }
}
