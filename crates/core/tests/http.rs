//! The Streamable HTTP layer: methods, headers and status codes.

mod common;

use common::*;
use pcloud_mcp_core::{HttpRequest, ServerConfig};
use serde_json::{Value, json};

fn send(req: HttpRequest) -> pcloud_mcp_core::HttpResponse {
    pollster::block_on(server(sample_storage()).handle_http(req))
}

fn ping() -> Value {
    request(7, "ping", json!({}))
}

fn error_code(resp: &pcloud_mcp_core::HttpResponse) -> i64 {
    body_json(resp)["error"]["code"]
        .as_i64()
        .expect("an error code")
}

#[test]
fn request_gets_200_json() {
    let resp = send(post(&ping()));
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-type"), Some("application/json"));
    assert_eq!(
        body_json(&resp),
        json!({"jsonrpc": "2.0", "id": 7, "result": {}})
    );
}

#[test]
fn no_session_id_is_issued() {
    let init = request(
        1,
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    let resp = send(post(&init));
    assert_eq!(resp.status, 200);
    assert!(resp.header("mcp-session-id").is_none());
}

#[test]
fn string_ids_are_echoed() {
    let resp = send(post(
        &json!({"jsonrpc": "2.0", "id": "abc", "method": "ping"}),
    ));
    assert_eq!(body_json(&resp)["id"], json!("abc"));
}

#[test]
fn notification_gets_202_without_body() {
    let resp = send(post(
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    ));
    assert_eq!(resp.status, 202);
    assert!(resp.body.is_empty());
}

#[test]
fn response_message_gets_202() {
    let resp = send(post(&json!({"jsonrpc": "2.0", "id": 3, "result": {}})));
    assert_eq!(resp.status, 202);
    assert!(resp.body.is_empty());
}

#[test]
fn get_and_delete_are_405_with_allow() {
    for method in ["GET", "DELETE", "PUT", "PATCH", "HEAD", "OPTIONS"] {
        let mut req = post(&ping());
        req.method = method.into();
        let resp = send(req);
        assert_eq!(resp.status, 405, "{method}");
        assert_eq!(resp.header("allow"), Some("POST"), "{method}");
    }
}

#[test]
fn wrong_content_type_is_415() {
    let mut req = post(&ping());
    req.headers
        .retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
    assert_eq!(send(req.clone()).status, 415);
    let req = with_header(req, "content-type", "text/plain");
    assert_eq!(send(req).status, 415);
}

#[test]
fn content_type_parameters_are_fine() {
    let mut req = post(&ping());
    req.headers
        .retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
    let req = with_header(req, "CONTENT-TYPE", "Application/JSON; charset=utf-8");
    assert_eq!(send(req).status, 200);
}

#[test]
fn accept_header_is_not_enforced() {
    let mut req = post(&ping());
    req.headers
        .retain(|(k, _)| !k.eq_ignore_ascii_case("accept"));
    assert_eq!(send(req.clone()).status, 200);
    assert_eq!(send(with_header(req, "Accept", "*/*")).status, 200);
}

#[test]
fn bad_json_is_400_parse_error() {
    for body in [&b"{not json"[..], b"", b"\xff\xfe"] {
        let resp = send(post_raw(body));
        assert_eq!(resp.status, 400);
        let body = body_json(&resp);
        assert_eq!(body["error"]["code"], json!(-32700));
        assert!(body.get("id").is_none(), "no id on a parse error: {body}");
    }
}

#[test]
fn batch_is_400_invalid_request() {
    let resp = send(post(&json!([ping(), ping()])));
    assert_eq!(resp.status, 400);
    assert_eq!(error_code(&resp), -32600);
}

#[test]
fn non_message_json_is_400_invalid_request() {
    for body in [
        json!(42),
        json!("ping"),
        json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 1, "method": 5}),
        json!({"jsonrpc": "2.0", "id": 1}),
    ] {
        let resp = send(post(&body));
        assert_eq!(resp.status, 400, "{body}");
        assert_eq!(error_code(&resp), -32600, "{body}");
    }
}

#[test]
fn supported_protocol_version_header_is_accepted() {
    for version in ["2025-03-26", "2025-06-18", "2025-11-25"] {
        let resp = send(with_header(post(&ping()), "MCP-Protocol-Version", version));
        assert_eq!(resp.status, 200, "{version}");
    }
}

#[test]
fn unsupported_protocol_version_header_is_400() {
    for version in ["2024-11-05", "1999-01-01", "garbage"] {
        let resp = send(with_header(post(&ping()), "mcp-protocol-version", version));
        assert_eq!(resp.status, 400, "{version}");
        let body = body_json(&resp);
        assert_eq!(body["id"], json!(7));
        assert_eq!(body["error"]["code"], json!(-32022));
        assert_eq!(body["error"]["data"]["requested"], json!(version));
        assert!(
            body["error"]["data"]["supported"]
                .as_array()
                .unwrap()
                .contains(&json!("2025-11-25"))
        );
    }
}

#[test]
fn unsupported_version_header_rejects_notifications_too() {
    let req = with_header(
        post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
        "MCP-Protocol-Version",
        "1999-01-01",
    );
    assert_eq!(send(req).status, 400);
}

#[test]
fn foreign_origin_is_403() {
    let resp = send(with_header(post(&ping()), "Origin", "https://evil.example"));
    assert_eq!(resp.status, 403);
    assert!(body_json(&resp).get("id").is_none());
    // Even before the method check.
    let mut req = with_header(post(&ping()), "Origin", "https://evil.example");
    req.method = "GET".into();
    assert_eq!(send(req).status, 403);
}

#[test]
fn claude_origins_are_allowed_by_default() {
    for origin in [
        "https://claude.ai",
        "https://claude.com/",
        "https://Claude.AI",
    ] {
        let resp = send(with_header(post(&ping()), "Origin", origin));
        assert_eq!(resp.status, 200, "{origin}");
    }
    for origin in ["https://claude.ai.evil.example", "http://claude.ai", "null"] {
        let resp = send(with_header(post(&ping()), "Origin", origin));
        assert_eq!(resp.status, 403, "{origin}");
    }
}

#[test]
fn allowed_origin_passes() {
    let config = ServerConfig {
        allowed_origins: vec!["https://claude.ai".into(), "http://localhost:6274/".into()],
        ..ServerConfig::default()
    };
    let server = server_with(sample_storage(), config);
    for origin in [
        "https://claude.ai",
        "HTTPS://CLAUDE.AI",
        "http://localhost:6274",
    ] {
        let resp =
            pollster::block_on(server.handle_http(with_header(post(&ping()), "Origin", origin)));
        assert_eq!(resp.status, 200, "{origin}");
    }
    let resp = pollster::block_on(server.handle_http(with_header(
        post(&ping()),
        "Origin",
        "https://claude.ai.evil.example",
    )));
    assert_eq!(resp.status, 403);
}

#[test]
fn unknown_method_is_200_with_32601_for_older_clients() {
    let resp = send(post(&request(1, "resources/list", json!({}))));
    assert_eq!(resp.status, 200);
    assert_eq!(error_code(&resp), -32601);
}

// --- 2026-07-28 stateless requests -------------------------------------------------------

fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"}
    })
}

/// A 2026-07-28 request with the headers that revision requires.
fn stateless(method: &str, mut params: Value) -> HttpRequest {
    params["_meta"] = meta();
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut req = with_header(
        with_header(
            post(&request(9, method, params)),
            "MCP-Protocol-Version",
            "2026-07-28",
        ),
        "Mcp-Method",
        method,
    );
    if let Some(name) = name {
        req = with_header(req, "Mcp-Name", &name);
    }
    req
}

fn without(mut req: HttpRequest, header: &str) -> HttpRequest {
    req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(header));
    req
}

#[test]
fn stateless_tools_list_carries_result_type_cache_hints_and_server_info() {
    let resp = send(stateless("tools/list", json!({})));
    assert_eq!(resp.status, 200);
    let result = &body_json(&resp)["result"];
    assert_eq!(result["resultType"], json!("complete"));
    assert_eq!(result["cacheScope"], json!("private"));
    assert!(result["ttlMs"].as_u64().unwrap() > 0);
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("pcloud-mcp")
    );
    assert_eq!(result["tools"].as_array().unwrap().len(), 5);
}

#[test]
fn stateless_tools_call_works() {
    let resp = send(stateless(
        "tools/call",
        json!({"name": "get_file_info", "arguments": {"path": "/readme.txt"}}),
    ));
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
    let result = &body_json(&resp)["result"];
    assert_eq!(result["resultType"], json!("complete"));
    assert_eq!(result["isError"], json!(false));
}

#[test]
fn stateless_mcp_name_may_be_base64() {
    let req = without(
        stateless(
            "tools/call",
            json!({"name": "list_folder", "arguments": {}}),
        ),
        "mcp-name",
    );
    // "list_folder" in base64.
    let req = with_header(req, "Mcp-Name", "=?base64?bGlzdF9mb2xkZXI=?=");
    assert_eq!(send(req).status, 200);
}

#[test]
fn stateless_header_mismatches_are_400_32020() {
    let call = || {
        stateless(
            "tools/call",
            json!({"name": "list_folder", "arguments": {}}),
        )
    };
    let cases = [
        without(call(), "mcp-protocol-version"),
        without(call(), "mcp-method"),
        without(call(), "mcp-name"),
        with_header(without(call(), "mcp-method"), "Mcp-Method", "tools/list"),
        with_header(without(call(), "mcp-name"), "Mcp-Name", "read_image"),
        with_header(without(call(), "mcp-name"), "Mcp-Name", "=?base64?!!!?="),
        with_header(
            without(call(), "mcp-protocol-version"),
            "MCP-Protocol-Version",
            "2025-11-25",
        ),
    ];
    for (i, req) in cases.into_iter().enumerate() {
        let resp = send(req);
        assert_eq!(resp.status, 400, "case {i}");
        assert_eq!(error_code(&resp), -32020, "case {i}");
        assert_eq!(body_json(&resp)["id"], json!(9), "case {i}");
    }
}

#[test]
fn stateless_header_without_meta_is_a_mismatch() {
    let req = with_header(
        with_header(post(&ping()), "MCP-Protocol-Version", "2026-07-28"),
        "Mcp-Method",
        "ping",
    );
    let resp = send(req);
    assert_eq!(resp.status, 400);
    assert_eq!(error_code(&resp), -32020);
}

#[test]
fn stateless_unknown_method_is_404_32601() {
    let resp = send(stateless("subscriptions/listen", json!({})));
    assert_eq!(resp.status, 404);
    assert_eq!(error_code(&resp), -32601);
}

#[test]
fn stateless_missing_client_capabilities_is_400_32602() {
    let mut params = json!({});
    params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28"});
    let req = with_header(
        with_header(
            post(&request(9, "tools/list", params)),
            "MCP-Protocol-Version",
            "2026-07-28",
        ),
        "Mcp-Method",
        "tools/list",
    );
    let resp = send(req);
    assert_eq!(resp.status, 400);
    assert_eq!(error_code(&resp), -32602);
}

#[test]
fn unsupported_meta_version_is_400_32022() {
    let mut params = meta();
    params["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let resp = send(post(&request(9, "tools/list", json!({"_meta": params}))));
    assert_eq!(resp.status, 400);
    let body = body_json(&resp);
    assert_eq!(body["error"]["code"], json!(-32022));
    assert_eq!(body["error"]["data"]["requested"], json!("2099-01-01"));
    assert_eq!(body["error"]["data"]["supported"][0], json!("2026-07-28"));
}

#[test]
fn server_discover_lists_versions() {
    let resp = send(stateless("server/discover", json!({})));
    assert_eq!(resp.status, 200);
    let result = &body_json(&resp)["result"];
    assert_eq!(result["resultType"], json!("complete"));
    assert_eq!(
        result["supportedVersions"],
        json!(["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"])
    );
    assert_eq!(result["capabilities"]["tools"]["listChanged"], json!(false));
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("pcloud-mcp")
    );
    assert!(
        result["instructions"]
            .as_str()
            .unwrap()
            .contains("search_files")
    );
}

#[test]
fn stateless_notifications_need_no_headers() {
    let req = with_header(
        post(
            &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"_meta": meta()}}),
        ),
        "MCP-Protocol-Version",
        "2026-07-28",
    );
    assert_eq!(send(req).status, 202);
}
