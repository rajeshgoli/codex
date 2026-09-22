use super::*;
use codex_protocol::openai_models::MultiAgentToolMessages;
use codex_protocol::openai_models::ToolMessages;
use pretty_assertions::assert_eq;

#[test]
fn catalog_tool_descriptions_fall_back_when_the_byte_limit_is_exceeded() {
    for (description, accepted) in [
        (String::new(), true),
        ("a".repeat(MAX_TOOL_DESCRIPTION_BYTES), true),
        ("a".repeat(MAX_TOOL_DESCRIPTION_BYTES + 1), false),
        ("界".repeat(MAX_TOOL_DESCRIPTION_BYTES / 3 + 1), false),
    ] {
        let tool = Some(ToolMessage {
            description: Some(description.clone()),
            ..Default::default()
        });
        let catalog = ModelMessages {
            tools: Some(ToolMessages {
                multi_agent: Some(MultiAgentToolMessages {
                    spawn_agent: tool.clone(),
                    send_message: tool.clone(),
                    followup_task: tool.clone(),
                    wait_agent: tool.clone(),
                    interrupt_agent: tool.clone(),
                    list_agents: tool,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = ResolvedModelMessages {
            catalog_messages: Some(&catalog),
        };
        let actual = [
            "spawn_agent",
            "send_message",
            "followup_task",
            "wait_agent",
            "interrupt_agent",
            "list_agents",
        ]
        .map(|name| resolved.multi_agent_tool_description_override(name));
        assert_eq!(actual, [accepted.then_some(description.as_str()); 6]);
    }
}
