use super::*;
use codex_protocol::openai_models::MultiAgentToolMessages;
use codex_protocol::openai_models::ToolMessages;
use pretty_assertions::assert_eq;

#[test]
fn catalog_tool_messages_fall_back_when_the_byte_limit_is_exceeded() {
    for (description, accepted) in [
        (String::new(), true),
        ("a".repeat(MAX_CATALOG_TOOL_MESSAGE_BYTES), true),
        ("a".repeat(MAX_CATALOG_TOOL_MESSAGE_BYTES + 1), false),
        ("界".repeat(MAX_CATALOG_TOOL_MESSAGE_BYTES / 3 + 1), false),
    ] {
        let tool = Some(ToolMessage {
            description: Some(description.clone()),
            parameters: Some(description.clone()),
        });
        let catalog = ModelMessages {
            tools: Some(ToolMessages {
                send_user_message_async: tool.clone(),
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
        .map(|name| {
            (
                resolved.multi_agent_tool_description_override(name),
                resolved.multi_agent_tool_parameters_override(name),
            )
        });
        let expected = accepted.then_some(description.as_str());
        assert_eq!(actual, [(expected, expected); 6]);
        assert_eq!(
            resolved.request_user_input_async_description(),
            expected.unwrap_or(REQUEST_USER_INPUT_ASYNC_DESCRIPTION)
        );
    }
}

#[test]
fn code_mode_fields_fall_back_independently_when_oversized() {
    let oversized = "界".repeat(MAX_CATALOG_TOOL_MESSAGE_BYTES / 3 + 1);
    let catalog = ModelMessages {
        tools: Some(ToolMessages {
            code_mode: Some(CodeModeToolMessages {
                exec: Some(ToolMessage {
                    description: Some(oversized.clone()),
                    parameters: Some(oversized.clone()),
                }),
                wait: Some(ToolMessage {
                    description: Some(String::new()),
                    parameters: Some(oversized.clone()),
                }),
                deferred_nested_tools_guidance: Some(oversized.clone()),
                mcp_typescript_preamble: Some(oversized),
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let resolved = ResolvedModelMessages {
        catalog_messages: Some(&catalog),
    };
    assert_eq!(
        resolved.code_mode(),
        Some(CodeModeToolMessages {
            exec: Some(ToolMessage::default()),
            wait: Some(ToolMessage {
                description: Some(String::new()),
                parameters: None,
            }),
            deferred_nested_tools_guidance: None,
            mcp_typescript_preamble: None,
        })
    );
    assert_eq!(resolved.code_mode_wait_description_override(), Some(""));
    assert_eq!(resolved.code_mode_wait_parameters_override(), None);
}
