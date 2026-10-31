//! Hidden paths never reach storage and never show; scopes filter and guard the tools.

mod common;

use chance_of_files_core::{HttpRequest, Policy, Scopes};
use common::*;
use serde_json::{Value, json};

fn hiding() -> Policy {
    Policy::new(
        &["/Crypto Folder", "/Notes/Secret"],
        &[] as &[&str],
        &["/Big"],
    )
    .unwrap()
}

fn drive() -> MemoryStorage {
    sample_storage()
        .file("/Crypto Folder/keys.txt", "secret")
        .file("/Notes/Secret/plan.md", "secret")
        .file("/Notes/secretive.md", "fine")
        .file("/Big/inside.txt", "x")
        .file("/Other/crypto-notes.txt", "x")
}

#[test]
fn hidden_paths_are_not_found_and_never_reach_storage() {
    let server = server_with_policy(drive(), hiding());
    for (tool, args) in [
        ("list_folder", json!({"path": "/Crypto Folder"})),
        ("list_folder", json!({"path": "/crypto folder/"})),
        ("get_file_info", json!({"path": "/CRYPTO FOLDER/keys.txt"})),
        (
            "read_text_file",
            json!({"path": "//Crypto Folder//./keys.txt"}),
        ),
        ("read_image", json!({"path": "/Crypto Folder/keys.txt"})),
        (
            "search_files",
            json!({"query": "keys", "path": "/Crypto Folder"}),
        ),
        ("read_text_file", json!({"path": "/notes/SECRET/plan.md"})),
    ] {
        let result = call(&server, tool, args.clone());
        assert!(is_error(&result), "{tool} {args}");
        assert!(
            text(&result).starts_with("not found: /"),
            "{tool} {args}: {result}"
        );
        assert!(server.storage().take_calls().is_empty(), "{tool} {args}");
    }
}

#[test]
fn listings_and_searches_drop_hidden_entries() {
    let server = server_with_policy(drive(), hiding());
    let listing = text(&call(&server, "list_folder", json!({})));
    assert!(!listing.contains("Crypto"), "{listing}");
    let listing = text(&call(&server, "list_folder", json!({"path": "/Notes"})));
    assert!(!listing.contains("Secret/"), "{listing}");
    assert!(listing.contains("secretive.md"), "{listing}");

    let found = text(&call(&server, "search_files", json!({"query": "secret"})));
    assert!(found.contains("/Notes/secretive.md"), "{found}");
    assert!(!found.contains("/Notes/Secret"), "{found}");
    let found = text(&call(&server, "search_files", json!({"query": "crypto"})));
    assert!(found.contains("/Other/crypto-notes.txt"), "{found}");
    assert!(!found.contains("Crypto Folder"), "{found}");
    // Hidden folders are never walked or listed, only their parents.
    let calls = server.storage().take_calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("Crypto") || c.contains("Secret")),
        "{calls:?}"
    );
}

#[test]
fn the_guide_is_not_read_from_a_hidden_path() {
    let config = chance_of_files_core::ServerConfig {
        guide_path: Some("/Crypto Folder/keys.txt".into()),
        ..Default::default()
    };
    let server = server_with(drive(), config).with_policy(hiding());
    let response = rpc(
        &server,
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    let instructions = response["result"]["instructions"].as_str().unwrap();
    assert!(!instructions.contains("secret"), "{instructions}");
    assert!(server.storage().take_calls().is_empty());
}

fn tool_names(
    server: &chance_of_files_core::McpServer<MemoryStorage>,
    scopes: Scopes,
) -> Vec<String> {
    let response =
        block_on(server.handle_jsonrpc_with(request(1, "tools/list", json!({})), scopes)).unwrap();
    response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn tools_list_follows_the_scopes() {
    let server = server(sample_storage());
    assert_eq!(tool_names(&server, Scopes::read_only()).len(), 5);
    assert_eq!(tool_names(&server, Scopes::all()).len(), 5);
    assert!(tool_names(&server, Scopes::none()).is_empty());
    assert!(tool_names(&server, Scopes::from_header(Some("files:write"))).is_empty());
}

#[test]
fn calls_without_the_scope_are_refused() {
    let server = server(sample_storage());
    let message = request(
        1,
        "tools/call",
        json!({"name": "list_folder", "arguments": {}}),
    );
    let response = block_on(server.handle_jsonrpc_with(message, Scopes::none())).unwrap();
    let result = &response["result"];
    assert!(is_error(result), "{response}");
    assert!(
        text(result).starts_with("list_folder needs the files:read permission"),
        "{response}"
    );
    assert!(server.storage().take_calls().is_empty());
    // Unknown tools stay a protocol error whatever the scopes.
    let message = request(2, "tools/call", json!({"name": "nope"}));
    let response = block_on(server.handle_jsonrpc_with(message, Scopes::none())).unwrap();
    assert_eq!(response["error"]["code"], json!(-32602));
}

#[test]
fn http_requests_carry_their_scopes() {
    let server = server(sample_storage());
    let mut req: HttpRequest = post(&request(1, "tools/list", json!({})));
    req.scopes = Scopes::none();
    let body: Value = body_json(&block_on(server.handle_http(req)));
    assert_eq!(body["result"]["tools"], json!([]));
    // A scopes header in the request itself means nothing to the core.
    let req = with_header(
        post(&request(1, "tools/list", json!({}))),
        "X-CoF-Scopes",
        "",
    );
    let body = body_json(&block_on(server.handle_http(req)));
    assert_eq!(body["result"]["tools"].as_array().unwrap().len(), 5);
}

#[test]
fn list_folder_pages_with_a_cursor() {
    let mut storage = MemoryStorage::new();
    for i in 0..2005 {
        storage = storage.file(&format!("/big/file-{i:04}.txt"), "x");
    }
    let server = server(storage);
    let first = text(&call(&server, "list_folder", json!({"path": "/big"})));
    let cursor = first
        .split("cursor=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a cursor")
        .to_owned();
    let second = text(&call(
        &server,
        "list_folder",
        json!({"path": "/big", "cursor": cursor}),
    ));
    assert!(
        second.contains("Showing entries 1001-2000 of 2005 entries"),
        "{second}"
    );
    assert!(second.contains("- file-1000.txt"), "{second}");
    assert!(!second.contains("- file-0999.txt"), "{second}");
    let cursor = second
        .split("cursor=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();
    let last = text(&call(
        &server,
        "list_folder",
        json!({"path": "/big", "cursor": cursor}),
    ));
    assert!(
        last.ends_with("Showing entries 2001-2005 of 2005.\n"),
        "{last}"
    );
    let bad = call(
        &server,
        "list_folder",
        json!({"path": "/big", "cursor": "zzz"}),
    );
    assert!(is_error(&bad));
}

#[test]
fn split_walks_never_fetch_excluded_folders() {
    use chance_of_files_core::Storage;
    let storage = drive();
    let lenient = Policy::default();
    let exclude = [
        lenient.check("/crypto folder").unwrap(),
        lenient.check("/Notes/Secret").unwrap(),
    ];
    let entries = block_on(storage.walk(&lenient.check("/").unwrap(), &exclude)).unwrap();
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"/Crypto Folder"), "{paths:?}");
    assert!(paths.contains(&"/Notes/Secret"), "{paths:?}");
    assert!(paths.contains(&"/Notes/secretive.md"), "{paths:?}");
    assert!(paths.contains(&"/Photos/dot.png"), "{paths:?}");
    assert!(
        !paths
            .iter()
            .any(|p| p.starts_with("/Crypto Folder/") || p.starts_with("/Notes/Secret/"))
    );
    let calls = storage.take_calls();
    assert_eq!(&calls[..2], ["list /", "walk /Big"], "{calls:?}");
    assert!(calls.contains(&"list /Notes".to_owned()), "{calls:?}");
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("Crypto") || c.contains("Secret")),
        "{calls:?}"
    );
}
