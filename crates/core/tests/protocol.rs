//! JSON-RPC methods: initialize, ping, tools/list and protocol errors.

mod common;

use common::*;
use pcloud_mcp_core::ServerConfig;
use serde_json::{Value, json};

fn initialize(version: &str) -> Value {
    let response = rpc(
        &server(sample_storage()),
        "initialize",
        json!({
            "protocolVersion": version,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }),
    );
    response["result"].clone()
}

#[test]
fn initialize_echoes_supported_versions() {
    for version in ["2025-03-26", "2025-06-18", "2025-11-25"] {
        assert_eq!(initialize(version)["protocolVersion"], json!(version));
    }
}

#[test]
fn initialize_offers_2025_11_25_otherwise() {
    // 2026-07-28 has no handshake, so a handshake can't settle on it.
    for version in ["2026-07-28", "2024-11-05", "1999-01-01", "2099-12-31"] {
        assert_eq!(
            initialize(version)["protocolVersion"],
            json!("2025-11-25"),
            "{version}"
        );
    }
}

#[test]
fn initialize_describes_the_server() {
    let result = initialize("2025-11-25");
    assert_eq!(
        result["capabilities"],
        json!({"tools": {"listChanged": false}})
    );
    assert_eq!(result["serverInfo"]["name"], json!("pcloud-mcp"));
    assert_eq!(
        result["serverInfo"]["version"],
        json!(env!("CARGO_PKG_VERSION"))
    );
    let instructions = result["instructions"].as_str().unwrap();
    assert!(instructions.contains("\"/\""));
    assert!(instructions.contains("search_files"));
    assert!(instructions.contains("list_folder"));
    // Older clients never heard of resultType.
    assert!(result.get("resultType").is_none());
}

#[test]
fn initialize_uses_configured_identity() {
    let config = ServerConfig {
        name: "my-files".into(),
        version: "9.9.9".into(),
        ..ServerConfig::default()
    };
    let response = rpc(
        &server_with(sample_storage(), config),
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    assert_eq!(
        response["result"]["serverInfo"],
        json!({"name": "my-files", "version": "9.9.9"})
    );
}

#[test]
fn initialize_without_version_is_invalid_params() {
    let response = rpc(&server(sample_storage()), "initialize", json!({}));
    assert_eq!(response["error"]["code"], json!(-32602));
}

#[test]
fn requests_work_without_initialize() {
    let response = rpc(&server(sample_storage()), "tools/list", json!({}));
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 5);
}

#[test]
fn ping_returns_empty_object() {
    let response = rpc(&server(sample_storage()), "ping", Value::Null);
    assert_eq!(response, json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
}

#[test]
fn notifications_get_no_response() {
    let server = server(sample_storage());
    for method in [
        "notifications/initialized",
        "notifications/cancelled",
        "notifications/whatever",
    ] {
        let message = json!({"jsonrpc": "2.0", "method": method});
        assert!(pollster::block_on(server.handle_jsonrpc(message)).is_none());
    }
}

#[test]
fn unknown_method_is_32601() {
    let response = rpc(&server(sample_storage()), "prompts/list", json!({}));
    assert_eq!(response["error"]["code"], json!(-32601));
    assert_eq!(response["id"], json!(1));
}

#[test]
fn non_object_params_are_32602() {
    let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": [1]});
    let response = pollster::block_on(server(sample_storage()).handle_jsonrpc(message)).unwrap();
    assert_eq!(response["error"]["code"], json!(-32602));
}

#[test]
fn tools_list_has_five_valid_tools() {
    let response = rpc(&server(sample_storage()), "tools/list", json!({}));
    let result = &response["result"];
    assert!(result.get("resultType").is_none());
    assert!(result.get("nextCursor").is_none());
    let tools = result["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "list_folder",
            "get_file_info",
            "read_text_file",
            "read_image",
            "search_files"
        ]
    );
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["title"].as_str().is_some_and(|t| !t.is_empty()),
            "{name}"
        );
        assert!(
            tool["description"].as_str().is_some_and(|d| d.len() > 20),
            "{name}"
        );
        assert_eq!(
            tool["annotations"],
            json!({
                "title": tool["title"],
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }),
            "{name}"
        );
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], json!("object"), "{name}");
        assert_eq!(schema["additionalProperties"], json!(false), "{name}");
        let properties = schema["properties"].as_object().unwrap();
        for property in properties.values() {
            let kind = property["type"].as_str().unwrap();
            assert!(["string", "integer"].contains(&kind), "{name}");
            assert!(property["description"].is_string(), "{name}");
        }
        for required in schema["required"].as_array().into_iter().flatten() {
            assert!(
                properties.contains_key(required.as_str().unwrap()),
                "{name}"
            );
        }
    }
}

#[test]
fn tool_schemas_follow_the_configured_limits() {
    let config = ServerConfig {
        max_read_bytes: 1234,
        search_limit: 7,
        ..ServerConfig::default()
    };
    let response = rpc(
        &server_with(sample_storage(), config),
        "tools/list",
        json!({}),
    );
    let tools = response["result"]["tools"].as_array().unwrap().clone();
    let tool = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap()
            .clone()
    };
    assert_eq!(
        tool("read_text_file")["inputSchema"]["properties"]["max_bytes"]["maximum"],
        json!(1234)
    );
    assert_eq!(
        tool("search_files")["inputSchema"]["properties"]["limit"]["maximum"],
        json!(7)
    );
    assert_eq!(
        tool("search_files")["inputSchema"]["required"],
        json!(["query"])
    );
}

#[test]
fn unknown_tool_is_32602() {
    let response = rpc(
        &server(sample_storage()),
        "tools/call",
        json!({"name": "delete_everything", "arguments": {}}),
    );
    assert_eq!(response["error"]["code"], json!(-32602));
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("delete_everything")
    );
}

#[test]
fn malformed_tools_call_is_32602() {
    let server = server(sample_storage());
    for params in [
        json!({}),
        json!({"name": 3}),
        json!({"name": "list_folder", "arguments": "/"}),
        json!({"name": "list_folder", "arguments": [1]}),
    ] {
        let response = rpc(&server, "tools/call", params.clone());
        assert_eq!(response["error"]["code"], json!(-32602), "{params}");
    }
}

#[test]
fn tools_call_without_arguments_uses_defaults() {
    let response = rpc(
        &server(sample_storage()),
        "tools/call",
        json!({"name": "list_folder"}),
    );
    assert_eq!(response["result"]["isError"], json!(false));
}
