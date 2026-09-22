//! Wait descriptions and parameter schemas resolve independently without changing other tools.

use super::rmcp_client::remote_aware_environment_id;
use super::rmcp_client::remote_aware_stdio_server_bin;
use anyhow::Result;
use codex_protocol::openai_models::ToolMessages;
use codex_protocol::openai_models::ToolMode;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse_completed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;

const PARAMETERS: &str = r#"{"type":"object","properties":{"cell_id":{"type":"string","description":"Catalog cell identifier."}},"required":["cell_id"],"additionalProperties":false}"#;

#[test_case(ToolMode::CodeMode, json!({"description":"x".repeat(1_001),"parameters":PARAMETERS}), true; "oversized_description_preserves_parameters")]
#[test_case(ToolMode::CodeModeOnly, json!({"description":"Catalog wait.","parameters":format!("{PARAMETERS}{}", " ".repeat(1_001))}), false; "oversized_parameters_preserve_description")]
#[test_case(ToolMode::CodeMode, json!({"description":"  Catalog wait. {{ literal }}\n"}), false; "description_only")]
#[test_case(ToolMode::CodeMode, json!({"parameters":PARAMETERS}), true; "parameters_only")]
#[test_case(ToolMode::CodeModeOnly, json!({"description":"Catalog wait.","parameters":PARAMETERS}), true; "both_in_code_mode_only")]
#[test_case(ToolMode::CodeModeOnly, json!({"description":"","parameters":""}), false; "empty_description_and_invalid_schema")]
#[test_case(ToolMode::CodeMode, json!({"description":"Catalog wait.","parameters":"{"}), false; "invalid_parameters_preserve_description")]
#[test_case(ToolMode::CodeMode, json!({"description":null,"parameters":null}), false; "null_fields_fall_back")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_wait_overrides_are_independent(
    tool_mode: ToolMode,
    overrides: Value,
    valid_parameters: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![sse_completed("bundled"), sse_completed("catalog")],
    )
    .await;
    for messages in [
        None,
        Some(serde_json::from_value::<ToolMessages>(
            json!({"code_mode": {"wait": overrides}}),
        )?),
    ] {
        let test = test_codex()
            .with_model_info_override("gpt-5.5", move |model| {
                model.tool_mode = Some(tool_mode);
                model.use_responses_lite = false;
                model.model_messages.as_mut().expect("model messages").tools = messages;
            })
            .with_config(|config| {
                config.code_mode.disable_in_process_fallback = true;
            })
            .build_with_auto_env(&server)
            .await?;
        test.submit_turn("Inspect the available tools.").await?;
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let mut expected = requests[0].body_json()["tools"].clone();
    let wait = expected
        .as_array_mut()
        .expect("tools")
        .iter_mut()
        .find(|tool| tool["name"] == "wait")
        .expect("wait");
    if let Some(description) = overrides["description"]
        .as_str()
        .filter(|text| text.len() <= 1_000)
    {
        wait["description"] = json!(description);
    }
    if valid_parameters {
        wait["parameters"] = serde_json::from_str(PARAMETERS)?;
    }
    assert_eq!(requests[1].body_json()["tools"], expected);
    Ok(())
}

#[test_case(json!({"exec":{"description":"x".repeat(1_001)}}); "exec_description")]
#[test_case(json!({"deferred_nested_tools_guidance":"界".repeat(334)}); "deferred_guidance")]
#[test_case(json!({"mcp_typescript_preamble":"x".repeat(1_001)}); "mcp_preamble")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_oversized_exec_fields_use_bundled_tools(overrides: Value) -> Result<()> {
    skip_if_wine_exec!(
        Ok(()),
        "requires a Windows test_stdio_server in the Wine-exec environment"
    );
    skip_if_no_network!(Ok(()));
    let mcp_command = remote_aware_stdio_server_bin()?;
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![sse_completed("bundled"), sse_completed("catalog")],
    )
    .await;
    for messages in [
        None,
        Some(serde_json::from_value::<ToolMessages>(
            json!({"code_mode": overrides}),
        )?),
    ] {
        let mcp_command = mcp_command.clone();
        let test = test_codex()
            .with_model_info_override("gpt-5.5", move |model| {
                model.tool_mode = Some(ToolMode::CodeModeOnly);
                model.use_responses_lite = false;
                model.supports_search_tool = true;
                model.model_messages.as_mut().expect("model messages").tools = messages;
            })
            .with_config(move |config| {
                config.code_mode.disable_in_process_fallback = true;
                let mut servers = config.mcp_servers.get().clone();
                servers.insert(
                    "rmcp".to_string(),
                    serde_json::from_value(json!({
                        "command": mcp_command,
                        "environment_id": remote_aware_environment_id(),
                        "cwd": config.cwd,
                        "enabled_tools": ["echo"],
                    }))
                    .expect("valid MCP test server configuration"),
                );
                config
                    .mcp_servers
                    .set(servers)
                    .expect("configure MCP server");
            })
            .build_with_auto_env(&server)
            .await?;
        wait_for_mcp_server(&test.codex, "rmcp").await?;
        test.submit_turn("Inspect the available tools.").await?;
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body_json()["tools"],
        requests[0].body_json()["tools"]
    );
    let tools = requests[1].body_json()["tools"].clone();
    let description = tools
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "exec")
        .expect("exec tool")["description"]
        .as_str()
        .expect("exec description");
    assert!(description.contains("Some deferred nested tools may be omitted"));
    assert!(description.contains("Shared MCP Types:"));
    assert!(description.contains("type CallToolResult"));
    assert!(!description.contains(&"x".repeat(1_001)));
    assert!(!description.contains(&"界".repeat(334)));
    Ok(())
}
