//! The tools over a real index: headers, paging, budgets, policy, health, duplicates.

mod common;

use std::collections::HashSet;

use chance_of_files_core::Policy;
use common::{Tree, call, is_error, next_cursor, open, result_size, text};
use serde_json::{Value, json};

const CAP: usize = 120_000;

fn tree() -> std::sync::Arc<Tree> {
    let t = Tree::new();
    t.file("/Docs/report.pdf", 2_000_000)
        .file("/Docs/notes.md", 300)
        .file("/Docs/Cafe\u{301} menu.txt", 40)
        .file_with("/Media/a.mov", 5_000_000, "2025-05-01T00:00:00Z", "same")
        .file_with(
            "/Media/copy of a.mov",
            5_000_000,
            "2025-06-01T00:00:00Z",
            "same",
        )
        .file_with(
            "/Media/b.mov",
            5_000_000,
            "2025-06-01T00:00:00Z",
            "different",
        )
        .file("/Media/clip.mp4", 3_000_000)
        .file("/Huge/inside.txt", 1)
        .file("/Secret/s.txt", 1);
    t
}

fn policy() -> Policy {
    Policy::new(&["/Secret"], &[] as &[&str], &["/Huge"]).unwrap()
}

#[tokio::test]
async fn tools_list_adds_the_audit_tools_only_with_an_index() {
    let f = open(tree(), &["/"], policy());
    let server = f.server();
    let listed = server
        .handle_jsonrpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await
        .unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "list_folder",
            "get_file_info",
            "read_text_file",
            "read_image",
            "search_files",
            "folder_stats",
            "query_files",
            "find_duplicates",
            "recent_changes",
            "type_breakdown",
            "index_status",
            "refresh_index"
        ]
    );
    for tool in listed["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], json!(true), "{name}");
        let schema = &tool["inputSchema"];
        assert_eq!(schema["additionalProperties"], json!(false), "{name}");
        for (key, property) in schema["properties"].as_object().unwrap() {
            assert!(property["description"].is_string(), "{name}.{key}");
        }
    }
    let plain = chance_of_files_core::McpServer::new(f.tree.clone(), Default::default());
    let response = plain
        .handle_jsonrpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                               "params": {"name": "folder_stats", "arguments": {}}}))
        .await
        .unwrap();
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown tool"),
        "{response}"
    );
}

#[tokio::test]
async fn before_the_first_walk_listing_is_live_and_audits_say_so() {
    let f = open(tree(), &["/"], policy());
    let server = f.server();
    let listed = call(&server, "list_folder", json!({"path": "/"})).await;
    assert!(!is_error(&listed));
    assert!(
        !text(&listed).contains("as of"),
        "live listing has no index header"
    );
    assert!(f.tree.take_calls().contains(&"list /".to_owned()));
    let stats = call(&server, "folder_stats", json!({})).await;
    assert!(is_error(&stats));
    assert!(text(&stats).contains("isn't ready yet"), "{}", text(&stats));
}

#[tokio::test]
async fn list_folder_answers_from_the_index_with_totals_and_sorts() {
    let f = open(tree(), &["/"], policy());
    f.walk("/").await;
    let server = f.server();
    f.tree.take_calls();
    let listed = call(
        &server,
        "list_folder",
        json!({"path": "/", "sort": "largest"}),
    )
    .await;
    let body = text(&listed);
    assert!(f.tree.take_calls().is_empty(), "no backend calls");
    let first = body.lines().next().unwrap();
    assert!(first.starts_with("[rows 1-3 of 3 · / as of "), "{body}");
    let order: Vec<&str> = body.lines().filter(|l| l.starts_with("- ")).collect();
    assert!(
        order[0].starts_with("- Media/ (folder, 17.1 MiB in 4 files"),
        "{body}"
    );
    assert!(order[2].starts_with("- Huge/"), "{body}");
    assert!(!body.contains("Secret"), "{body}");

    let live = call(
        &server,
        "list_folder",
        json!({"path": "/Docs", "live": true}),
    )
    .await;
    assert!(!text(&live).contains("as of"));
    assert_eq!(f.tree.take_calls(), ["list /Docs"]);

    let file = call(&server, "list_folder", json!({"path": "/docs/notes.md"})).await;
    assert!(is_error(&file) && text(&file).contains("is a file"));
    let hidden = call(&server, "list_folder", json!({"path": "/secret"})).await;
    assert!(is_error(&hidden) && text(&hidden).contains("not found"));
}

#[tokio::test]
async fn search_matches_folded_names_and_honours_search_skip() {
    let f = open(tree(), &["/"], policy());
    f.walk("/").await;
    let server = f.server();
    for query in ["CAF\u{c9}", "cafe", "caf\u{e9}"] {
        let found = call(&server, "search_files", json!({"query": query})).await;
        let body = text(&found);
        assert!(
            body.contains("/Docs/Cafe\u{301} menu.txt"),
            "{query}: {body}"
        );
        assert!(body.starts_with("[rows 1-1 of 1"), "{body}");
    }
    let skipped = call(&server, "search_files", json!({"query": "inside"})).await;
    let body = text(&skipped);
    assert!(!body.contains("/Huge/inside.txt"), "{body}");
    assert!(body.contains("Skipped /Huge"), "{body}");
    let hidden = call(&server, "search_files", json!({"query": "s.txt"})).await;
    assert!(!text(&hidden).contains("Secret"));
    let videos = call(
        &server,
        "search_files",
        json!({"query": "a", "kind": "video"}),
    )
    .await;
    let body = text(&videos);
    assert!(
        body.contains("/Media/a.mov") && !body.contains("report.pdf"),
        "{body}"
    );
}

#[tokio::test]
async fn query_files_bounds_and_structured_content() {
    let f = open(tree(), &["/"], policy());
    f.walk("/").await;
    let server = f.server();
    let movs = call(
        &server,
        "query_files",
        json!({"ext": "mov", "min_size": "4 MB", "modified_after": "2025-05-15", "limit": 1}),
    )
    .await;
    let body = text(&movs);
    assert!(
        body.contains("2 files under / match (9.5 MiB in all)"),
        "{body}"
    );
    assert!(next_cursor(&movs).is_some(), "{body}");
    let rows = movs["structuredContent"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["size"], json!(5_000_000));
    let more = call(
        &server,
        "query_files",
        json!({"ext": "mov", "min_size": "4 MB", "modified_after": "2025-05-15", "limit": 1,
               "cursor": next_cursor(&movs)}),
    )
    .await;
    assert!(text(&more).starts_with("[rows 2-2 of 2"), "{}", text(&more));
    assert_ne!(
        more["structuredContent"]["rows"][0]["path"],
        rows[0]["path"]
    );

    let wrong = call(
        &server,
        "query_files",
        json!({"ext": "pdf", "cursor": next_cursor(&movs)}),
    )
    .await;
    assert!(
        is_error(&wrong) && text(&wrong).contains("cursor"),
        "{}",
        text(&wrong)
    );
    let bad = call(&server, "query_files", json!({"kind": "spaceship"})).await;
    assert!(is_error(&bad));
}

#[tokio::test]
async fn folder_stats_type_breakdown_status_and_refresh() {
    let f = open(tree(), &["/"], policy());
    f.walk("/").await;
    let server = f.server();
    let stats = call(&server, "folder_stats", json!({"depth": 2})).await;
    let body = text(&stats);
    assert!(
        body.contains("/ holds 19.0 MiB in 8 files")
            && body.contains("Subfolders 2 levels down, largest first"),
        "{body}"
    );
    let folders = stats["structuredContent"]["folders"].as_array().unwrap();
    assert_eq!(folders[0]["path"], json!("/Media"));
    assert_eq!(folders[0]["bytes"], json!(18_000_000));

    let types = call(&server, "type_breakdown", json!({"path": "/"})).await;
    let body = text(&types);
    assert!(body.contains("- video: 4 files"), "{body}");
    assert!(body.contains("mov 3"), "{body}");
    assert_eq!(
        types["structuredContent"]["kinds"][0]["kind"],
        json!("video")
    );

    let status = call(&server, "index_status", json!({})).await;
    let body = text(&status);
    assert!(body.contains("- / (every 30 min): as of"), "{body}");
    assert!(body.contains("Backend: healthy."), "{body}");

    let refresh = call(&server, "refresh_index", json!({"path": "/Docs"})).await;
    assert!(
        text(&refresh).contains("Queued a walk of /Docs (in root /)"),
        "{}",
        text(&refresh)
    );
    let again = call(&server, "refresh_index", json!({"path": "/Docs"})).await;
    assert!(text(&again).contains("already queued"));
    let outside = call(&server, "refresh_index", json!({"path": "/Secret"})).await;
    assert!(is_error(&outside));
}

#[tokio::test]
async fn find_duplicates_hashes_once_and_reports_sets() {
    let f = open(tree(), &["/"], policy());
    f.walk("/").await;
    let server = f.server();
    f.tree.take_calls();
    let dups = call(&server, "find_duplicates", json!({})).await;
    let body = text(&dups);
    assert!(
        body.contains("2 copies of 4.7 MiB (4.7 MiB reclaimable)"),
        "{body}"
    );
    assert!(
        body.contains("/Media/a.mov") && body.contains("/Media/copy of a.mov"),
        "{body}"
    );
    assert!(!body.contains("/Media/b.mov\n"), "{body}");
    let hashed: Vec<String> = f.tree.take_calls();
    assert_eq!(hashed.len(), 3, "{hashed:?}");
    assert_eq!(dups["structuredContent"]["sets"][0]["copies"], json!(2));

    let again = call(&server, "find_duplicates", json!({})).await;
    assert_eq!(
        text(&again).lines().skip(1).collect::<Vec<_>>(),
        body.lines().skip(1).collect::<Vec<_>>()
    );
    assert!(f.tree.take_calls().is_empty(), "cached");

    // A changed file loses its cached hash.
    f.tree
        .set_size("/Media/b.mov", 5_000_000, "2026-07-01T00:00:00Z");
    f.walk("/").await;
    f.tree.take_calls();
    call(&server, "find_duplicates", json!({})).await;
    let calls = f.tree.take_calls();
    assert!(
        calls.iter().filter(|c| c.starts_with("hash ")).count() == 1,
        "{calls:?}"
    );
}

#[tokio::test]
async fn recent_changes_and_get_file_info_totals() {
    let f = open(tree(), &["/"], policy());
    f.walk("/").await;
    f.tree.file("/Docs/new.md", 10);
    f.tree.rename("/Media", "/Videos");
    f.walk("/").await;
    let server = f.server();
    let changes = call(&server, "recent_changes", json!({"since": "1d"})).await;
    let body = text(&changes);
    assert!(body.contains("moved folder /Media → /Videos"), "{body}");
    assert!(body.contains("added file /Docs/new.md"), "{body}");
    let only_docs = call(&server, "recent_changes", json!({"path": "/Docs"})).await;
    assert!(!text(&only_docs).contains("/Videos"));
    // Paths are looked up as the index spells them, like the other audit tools.
    let lower = call(&server, "recent_changes", json!({"path": "/docs"})).await;
    let body = text(&lower);
    assert!(body.contains("added file /Docs/new.md"), "{body}");
    assert!(body.contains("Changes under /Docs since"), "{body}");
    // A folder the index no longer has is still a valid place to ask about.
    let gone = call(&server, "recent_changes", json!({"path": "/Media"})).await;
    assert!(!is_error(&gone), "{}", text(&gone));
    assert!(text(&gone).contains("Changes under /Media since"));

    let info = call(&server, "get_file_info", json!({"path": "/Videos"})).await;
    let body = text(&info);
    assert!(body.contains("Contains: 17.1 MiB in 4 files"), "{body}");
    let hashed = call(
        &server,
        "get_file_info",
        json!({"path": "/Videos/a.mov", "hash": true}),
    )
    .await;
    assert!(
        text(&hashed).contains("SHA1: sha1-of-same"),
        "{}",
        text(&hashed)
    );
}

#[tokio::test]
async fn an_unhealthy_backend_shows_in_headers_and_live_calls_fail_fast() {
    let f = open(tree(), &["/"], policy());
    f.walk("/").await;
    let server = f.server();
    *f.tree.down.lock().unwrap() = true;
    f.health.set_failed("pCloud token expired");

    let listed = call(&server, "list_folder", json!({"path": "/Docs"})).await;
    let body = text(&listed);
    assert!(!is_error(&listed), "{body}");
    assert!(
        body.lines()
            .nth(1)
            .unwrap()
            .starts_with("[backend unhealthy since "),
        "{body}"
    );

    let read = call(&server, "read_text_file", json!({"path": "/Docs/notes.md"})).await;
    assert!(is_error(&read));
    assert!(
        text(&read).contains("can't be reached right now"),
        "{}",
        text(&read)
    );

    let info = call(&server, "get_file_info", json!({"path": "/Docs/notes.md"})).await;
    let body = text(&info);
    assert!(
        !is_error(&info) && body.contains("this is from the index"),
        "{body}"
    );

    // Recovery is noticed by the next live call.
    *f.tree.down.lock().unwrap() = false;
    let read = call(&server, "read_text_file", json!({"path": "/Docs/notes.md"})).await;
    assert!(!is_error(&read), "{}", text(&read));
    assert!(f.health.is_healthy());
}

/// Collects every row of a paged tool call; checks each page's size and that rows never repeat.
async fn all_pages(
    server: &chance_of_files_core::McpServer<std::sync::Arc<Tree>>,
    tool: &str,
    args: Value,
    row: fn(&str) -> bool,
) -> Vec<String> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..1000 {
        let mut args = args.clone();
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let result = call(server, tool, args).await;
        let body = text(&result);
        assert!(!is_error(&result), "{body}");
        let size = body.len()
            + result
                .get("structuredContent")
                .map_or(0, |v| v.to_string().len());
        assert!(size <= CAP, "{tool}: {size} characters");
        for line in body.lines().filter(|l| row(l)) {
            assert!(seen.insert(line.to_owned()), "{tool} repeated {line}");
            rows.push(line.to_owned());
        }
        cursor = next_cursor(&result);
        if cursor.is_none() {
            return rows;
        }
    }
    panic!("{tool} never ran out of pages");
}

#[tokio::test(flavor = "multi_thread")]
async fn on_a_150k_entry_tree_every_tool_stays_under_the_cap_and_pages_cover_everything() {
    let t = Tree::new();
    // 150 folders of 1,000 files each, long names, plenty of size collisions.
    for d in 0..150 {
        for i in 0..1000 {
            t.file_with(
                &format!("/Library/Section {d:03} with a fairly long folder name/IMG_{d:03}_{i:04}_a long descriptive file name.jpg"),
                1_048_576 + (i % 500) as u64,
                "2025-01-01T00:00:00Z",
                &format!("{d}-{i}"),
            );
        }
    }
    let f = open(t, &["/"], Policy::default());
    let started = std::time::Instant::now();
    f.walk("/").await;
    eprintln!("walked 150k entries in {:?}", started.elapsed());
    assert_eq!(
        chance_of_files_core::Index::roots(&*f.index).unwrap()[0].entries,
        150_151
    );
    let server = f.server();

    let is_entry = |l: &str| l.starts_with("- ");
    let files = all_pages(&server, "query_files", json!({"limit": 1000}), is_entry).await;
    assert_eq!(files.len(), 150_000);
    let listed = all_pages(
        &server,
        "list_folder",
        json!({"path": "/Library/Section 007 with a fairly long folder name", "sort": "newest"}),
        is_entry,
    )
    .await;
    assert_eq!(listed.len(), 1000);
    let found = all_pages(
        &server,
        "search_files",
        json!({"query": "img_14"}),
        is_entry,
    )
    .await;
    assert_eq!(found.len(), 10_000);
    let folders = all_pages(
        &server,
        "folder_stats",
        json!({"path": "/", "depth": 3, "limit": 1000}),
        |l| l.trim_start().starts_with("- "),
    )
    .await;
    assert_eq!(folders.len(), 151);

    for (tool, args) in [
        ("type_breakdown", json!({})),
        ("index_status", json!({})),
        ("recent_changes", json!({})),
        (
            "list_folder",
            json!({"path": "/Library/Section 001 with a fairly long folder name"}),
        ),
        ("search_files", json!({"query": "jpg"})),
    ] {
        let result = call(&server, tool, args).await;
        assert!(!is_error(&result), "{tool}: {}", text(&result));
        assert!(
            result_size(&result) <= CAP,
            "{tool}: {}",
            result_size(&result)
        );
    }
}

#[tokio::test]
async fn find_duplicates_truncates_one_size_group_too_big_for_a_page() {
    let t = Tree::new();
    let long = "x".repeat(180);
    for i in 0..400 {
        t.file_with(
            &format!("/Big/{long}/{i:04}.bin"),
            2_000_000,
            "2026-01-01T00:00:00Z",
            "same",
        );
    }
    t.file_with("/Small/a.bin", 1_500_000, "2026-01-01T00:00:00Z", "pair")
        .file_with("/Small/b.bin", 1_500_000, "2026-01-01T00:00:00Z", "pair");
    let f = open(t, &["/"], Policy::default());
    f.walk("/").await;
    let server = f.server();
    let dups = call(&server, "find_duplicates", json!({})).await;
    let body = text(&dups);
    assert!(result_size(&dups) <= CAP, "{}", result_size(&dups));
    assert!(body.contains("400 copies of 1.9 MiB"), "{body}");
    let set = &dups["structuredContent"]["sets"][0];
    let shown = set["paths"].as_array().unwrap().len();
    assert!(shown < 400, "{shown}");
    assert_eq!(set["more_paths"], json!(400 - shown));
    assert!(
        body.contains(&format!("… and {} more", 400 - shown)),
        "{body}"
    );
    // The page goes on to the next size group rather than dropping it.
    let mut all = body.clone();
    let mut cursor = next_cursor(&dups);
    while let Some(c) = cursor {
        let page = call(&server, "find_duplicates", json!({"cursor": c})).await;
        all.push_str(&text(&page));
        cursor = next_cursor(&page);
    }
    assert!(
        all.contains("/Small/a.bin") && all.contains("/Small/b.bin"),
        "{all}"
    );
}
