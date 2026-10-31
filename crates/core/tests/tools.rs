//! Each tool's happy and error paths.

mod common;

use base64::Engine;
use chance_of_files_core::ServerConfig;
use common::*;
use serde_json::json;

fn sample() -> chance_of_files_core::McpServer<MemoryStorage> {
    server(sample_storage())
}

fn assert_tool_error(result: &serde_json::Value, needle: &str) {
    assert!(is_error(result), "expected an error result: {result}");
    let message = text(result);
    assert!(
        message.contains(needle),
        "{message:?} should mention {needle:?}"
    );
}

// --- list_folder ---------------------------------------------------------------------------

#[test]
fn list_root_puts_folders_first_then_names() {
    let result = call(&sample(), "list_folder", json!({}));
    assert!(!is_error(&result));
    let listing = text(&result);
    let lines: Vec<&str> = listing.lines().collect();
    assert_eq!(lines[0], "Folder /: 5 folders, 2 files.");
    let names: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.trim_start_matches("- ").split(" (").next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "alpha/",
            "bin/",
            "Notes/",
            "Photos/",
            "Zeta/",
            "empty.txt",
            "readme.txt"
        ]
    );
    assert!(listing.contains(
        "- readme.txt (file, 19 bytes, modified 2026-10-01T09:00:00Z) path: /readme.txt"
    ));
    assert!(listing.contains("- Notes/ (folder, modified 2026-10-01T09:00:00Z) path: /Notes"));
}

#[test]
fn list_subfolder_with_messy_path() {
    let result = call(&sample(), "list_folder", json!({"path": "Notes//./"}));
    let listing = text(&result);
    assert!(
        listing.starts_with("Folder /Notes: 1 folder, 3 files."),
        "{listing}"
    );
    assert!(listing.contains("path: /Notes/todo.md"));
    // Case-insensitive name order within files.
    let ideas = listing.find("Ideas.md").unwrap();
    let todo = listing.find("todo.md").unwrap();
    let unicode = listing.find("unicode.txt").unwrap();
    assert!(ideas < todo && todo < unicode);
}

#[test]
fn list_empty_folder() {
    let result = call(&sample(), "list_folder", json!({"path": "/Notes/archive"}));
    assert_eq!(text(&result), "Folder /Notes/archive is empty.");
}

#[test]
fn list_errors() {
    let server = sample();
    assert_tool_error(
        &call(&server, "list_folder", json!({"path": "/nope"})),
        "not found: /nope",
    );
    assert_tool_error(
        &call(&server, "list_folder", json!({"path": "/readme.txt"})),
        "not a folder",
    );
    assert_tool_error(
        &call(&server, "list_folder", json!({"path": "/Notes/../.."})),
        "invalid path",
    );
    assert_tool_error(
        &call(&server, "list_folder", json!({"path": 5})),
        "path must be a string",
    );
}

#[test]
fn list_caps_at_1000_entries() {
    let mut storage = MemoryStorage::new();
    for i in 0..1005 {
        storage = storage.file(&format!("/big/file-{i:04}.txt"), "x");
    }
    let result = call(&server(storage), "list_folder", json!({"path": "/big"}));
    let listing = text(&result);
    assert_eq!(
        listing.lines().filter(|l| l.starts_with("- ")).count(),
        1000
    );
    assert!(listing.contains("Showing the first 1000 of 1005 entries"));
    assert!(listing.contains("file-0999.txt"));
    assert!(!listing.contains("file-1000.txt"));
}

#[test]
fn backend_errors_hide_urls_and_credentials() {
    let result = call(&sample(), "list_folder", json!({"path": "/broken"}));
    assert!(is_error(&result));
    let message = text(&result);
    assert!(message.starts_with("storage backend error:"), "{message}");
    assert!(!message.contains("://"), "{message}");
    assert!(!message.contains("dev-password"), "{message}");
}

#[test]
fn too_many_entries_tells_the_model_to_narrow_the_path() {
    for (tool, args) in [
        ("list_folder", json!({"path": "/huge"})),
        ("search_files", json!({"query": "report", "path": "/huge"})),
    ] {
        let result = call(&sample(), tool, args);
        assert_tool_error(&result, "too many entries to go through at once: /huge");
        let message = text(&result);
        assert!(message.contains("Search a narrower path"), "{message}");
        assert!(message.contains("MAX_LISTING_BYTES"), "{message}");
    }
}

// --- get_file_info -------------------------------------------------------------------------

#[test]
fn info_for_file_and_folder() {
    let server = sample();
    let file = text(&call(
        &server,
        "get_file_info",
        json!({"path": "/Photos/dot.png"}),
    ));
    assert_eq!(
        file,
        format!(
            "Path: /Photos/dot.png\nName: dot.png\nType: file\nSize: {} bytes\n\
             Modified: 2026-10-01T09:00:00Z\nContent type: image/png\n",
            PNG_1X1.len()
        )
    );
    let folder = text(&call(&server, "get_file_info", json!({"path": "Photos/"})));
    assert!(folder.contains("Path: /Photos\n"));
    assert!(folder.contains("Type: folder\n"));
    assert!(!folder.contains("Size"));
}

#[test]
fn info_errors() {
    let server = sample();
    assert_tool_error(
        &call(&server, "get_file_info", json!({})),
        "path is required",
    );
    assert_tool_error(
        &call(&server, "get_file_info", json!({"path": null})),
        "path is required",
    );
    assert_tool_error(
        &call(&server, "get_file_info", json!({"path": "/missing.txt"})),
        "not found",
    );
    assert_tool_error(
        &call(&server, "get_file_info", json!({"path": "/a\0b"})),
        "invalid path",
    );
}

// --- read_text_file ------------------------------------------------------------------------

#[test]
fn read_whole_small_file_without_header() {
    let result = call(
        &sample(),
        "read_text_file",
        json!({"path": "/Notes/todo.md"}),
    );
    assert!(!is_error(&result));
    assert_eq!(text(&result), "- buy milk\n- call mum\n");
}

#[test]
fn read_in_pages_with_header() {
    let server = sample();
    let first = text(&call(
        &server,
        "read_text_file",
        json!({"path": "/Notes/todo.md", "max_bytes": 10}),
    ));
    assert_eq!(
        first,
        "[bytes 0-9 of 22; continue with offset=10]\n- buy milk"
    );
    let second = text(&call(
        &server,
        "read_text_file",
        json!({"path": "/Notes/todo.md", "offset": 10, "max_bytes": 100}),
    ));
    assert_eq!(second, "[bytes 10-21 of 22; end of file]\n\n- call mum\n");
}

#[test]
fn read_accepts_lenient_numbers() {
    let result = call(
        &sample(),
        "read_text_file",
        json!({"path": "/Notes/todo.md", "offset": "2", "max_bytes": 3.0}),
    );
    assert_eq!(
        text(&result),
        "[bytes 2-4 of 22; continue with offset=5]\nbuy"
    );
}

#[test]
fn read_never_splits_a_utf8_character() {
    let content = "aé€😀b".repeat(7) + "end";
    let storage = MemoryStorage::new().file("/u.txt", content.clone());
    let server = server(storage);
    // Every page size, including ones smaller than the widest character's tail.
    for max_bytes in 4..=13u64 {
        let mut offset = 0u64;
        let mut joined = String::new();
        for _ in 0..200 {
            let page = text(&call(
                &server,
                "read_text_file",
                json!({"path": "/u.txt", "offset": offset, "max_bytes": max_bytes}),
            ));
            assert!(
                !page.contains('\u{FFFD}'),
                "split character with max_bytes={max_bytes}: {page:?}"
            );
            if offset == 0 && !page.starts_with('[') {
                joined = page;
                break;
            }
            let (header, body) = page.split_once('\n').unwrap();
            joined.push_str(body);
            if header.ends_with("end of file]") {
                break;
            }
            let next = header
                .rsplit_once("offset=")
                .unwrap()
                .1
                .trim_end_matches(']');
            let next: u64 = next.parse().unwrap();
            assert!(next > offset, "no progress at offset {offset}");
            offset = next;
        }
        assert_eq!(joined, content, "max_bytes={max_bytes}");
    }
}

#[test]
fn read_from_inside_a_character_skips_to_the_next() {
    // "€" is bytes 0-2; offset 1 lands inside it.
    let storage = MemoryStorage::new().file("/e.txt", "€abc");
    let result = call(
        &server(storage),
        "read_text_file",
        json!({"path": "/e.txt", "offset": 1}),
    );
    assert_eq!(text(&result), "[bytes 3-5 of 6; end of file]\nabc");
}

#[test]
fn read_page_too_small_for_a_character() {
    let storage = MemoryStorage::new().file("/e.txt", "😀😀");
    let result = call(
        &server(storage),
        "read_text_file",
        json!({"path": "/e.txt", "max_bytes": 2}),
    );
    assert_tool_error(&result, "too small");
}

#[test]
fn read_is_capped_by_config() {
    let config = ServerConfig {
        max_read_bytes: 5,
        ..ServerConfig::default()
    };
    let server = server_with(sample_storage(), config);
    let page = text(&call(
        &server,
        "read_text_file",
        json!({"path": "/Notes/todo.md", "max_bytes": 1000}),
    ));
    assert_eq!(page, "[bytes 0-4 of 22; continue with offset=5]\n- buy");
}

#[test]
fn read_unicode_file() {
    let result = call(
        &sample(),
        "read_text_file",
        json!({"path": "/Notes/unicode.txt"}),
    );
    assert_eq!(text(&result), "Grüße, €uro and 😀!\n");
}

#[test]
fn read_empty_file() {
    let result = call(&sample(), "read_text_file", json!({"path": "/empty.txt"}));
    assert!(!is_error(&result));
    assert!(text(&result).contains("empty"));
}

#[test]
fn read_errors() {
    let server = sample();
    assert_tool_error(
        &call(&server, "read_text_file", json!({})),
        "path is required",
    );
    assert_tool_error(
        &call(&server, "read_text_file", json!({"path": "/Notes"})),
        "is a folder",
    );
    assert_tool_error(
        &call(&server, "read_text_file", json!({"path": "/nope.md"})),
        "not found",
    );
    assert_tool_error(
        &call(&server, "read_text_file", json!({"path": "/bin/blob.bin"})),
        "binary",
    );
    assert_tool_error(
        &call(
            &server,
            "read_text_file",
            json!({"path": "/Photos/dot.png"}),
        ),
        "read_image",
    );
    assert_tool_error(
        &call(
            &server,
            "read_text_file",
            json!({"path": "/readme.txt", "offset": 19}),
        ),
        "past the end",
    );
    assert_tool_error(
        &call(
            &server,
            "read_text_file",
            json!({"path": "/readme.txt", "offset": -1}),
        ),
        "offset must be a whole number",
    );
    assert_tool_error(
        &call(
            &server,
            "read_text_file",
            json!({"path": "/readme.txt", "max_bytes": 0}),
        ),
        "at least 1",
    );
    assert_tool_error(
        &call(
            &server,
            "read_text_file",
            json!({"path": "/readme.txt", "offset": 1.5}),
        ),
        "whole number",
    );
}

#[test]
fn binary_check_looks_at_the_first_8_kib_only() {
    let mut late_nul = vec![b'a'; 9000];
    late_nul[8500] = 0;
    let mut early_nul = vec![b'a'; 9000];
    early_nul[8000] = 0;
    let storage = MemoryStorage::new()
        .file("/late.txt", late_nul)
        .file("/early.txt", early_nul);
    let server = server(storage);
    assert!(!is_error(&call(
        &server,
        "read_text_file",
        json!({"path": "/late.txt"})
    )));
    assert_tool_error(
        &call(&server, "read_text_file", json!({"path": "/early.txt"})),
        "binary",
    );
}

// --- read_image ----------------------------------------------------------------------------

#[test]
fn read_png() {
    let result = call(&sample(), "read_image", json!({"path": "/Photos/dot.png"}));
    assert!(!is_error(&result), "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], json!("text"));
    assert!(
        content[0]["text"]
            .as_str()
            .unwrap()
            .contains("/Photos/dot.png")
    );
    let image = &content[1];
    assert_eq!(image["type"], json!("image"));
    assert_eq!(image["mimeType"], json!("image/png"));
    let data = base64::engine::general_purpose::STANDARD
        .decode(image["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(data, PNG_1X1);
}

#[test]
fn image_type_comes_from_the_bytes() {
    let result = call(
        &sample(),
        "read_image",
        json!({"path": "/Photos/really-a-jpeg.png"}),
    );
    assert!(!is_error(&result), "{result}");
    assert_eq!(result["content"][1]["mimeType"], json!("image/jpeg"));
}

#[test]
fn image_size_limit() {
    let config = ServerConfig {
        max_image_bytes: 64,
        ..ServerConfig::default()
    };
    let mut big = PNG_1X1.to_vec();
    big.resize(65, 0);
    let mut exact = PNG_1X1.to_vec();
    exact.resize(64, 0);
    let storage = MemoryStorage::new()
        .file("/big.png", big)
        .file("/exact.png", exact);
    let server = server_with(storage, config);
    assert_tool_error(
        &call(&server, "read_image", json!({"path": "/big.png"})),
        "larger than the 64 bytes",
    );
    assert!(!is_error(&call(
        &server,
        "read_image",
        json!({"path": "/exact.png"})
    )));
}

#[test]
fn image_errors() {
    let server = sample();
    assert_tool_error(&call(&server, "read_image", json!({})), "path is required");
    assert_tool_error(
        &call(&server, "read_image", json!({"path": "/Photos"})),
        "is a folder",
    );
    assert_tool_error(
        &call(&server, "read_image", json!({"path": "/readme.txt"})),
        "not a PNG, JPEG, GIF or WebP",
    );
    assert_tool_error(
        &call(&server, "read_image", json!({"path": "/Photos/fake.png"})),
        "contents are not",
    );
    assert_tool_error(
        &call(&server, "read_image", json!({"path": "/Photos/gone.png"})),
        "not found",
    );
}

// --- search_files --------------------------------------------------------------------------

#[test]
fn search_finds_names_case_insensitively() {
    let result = call(&sample(), "search_files", json!({"query": "  NOTES "}));
    assert!(!is_error(&result));
    let found = text(&result);
    assert!(
        found.starts_with("2 matches for \"NOTES\" under /:"),
        "{found}"
    );
    assert!(found.contains("path: /Notes\n"));
    assert!(found.contains("path: /Photos/notes-about-photos.txt\n"));
}

#[test]
fn search_under_a_folder() {
    let result = call(
        &sample(),
        "search_files",
        json!({"query": ".md", "path": "/Notes"}),
    );
    let found = text(&result);
    assert!(found.starts_with("2 matches"), "{found}");
    let none = text(&call(
        &sample(),
        "search_files",
        json!({"query": ".md", "path": "/Photos"}),
    ));
    assert_eq!(
        none,
        "No files or folders under /Photos have names containing \".md\"."
    );
}

#[test]
fn search_limit_defaults_and_caps() {
    let mut storage = MemoryStorage::new();
    for i in 0..30 {
        storage = storage.file(&format!("/r/report-{i:02}.txt"), "x");
    }
    let config = ServerConfig {
        search_limit: 10,
        ..ServerConfig::default()
    };
    let server = server_with(storage, config);
    let count = |args| {
        text(&call(&server, "search_files", args))
            .lines()
            .filter(|l| l.starts_with("- "))
            .count()
    };
    assert_eq!(count(json!({"query": "report"})), 10);
    assert_eq!(count(json!({"query": "report", "limit": 3})), 3);
    assert_eq!(count(json!({"query": "report", "limit": 500})), 10);
    let found = text(&call(
        &server,
        "search_files",
        json!({"query": "report", "limit": 3}),
    ));
    assert!(found.contains("there may be more"));
}

#[test]
fn search_errors() {
    let server = sample();
    assert_tool_error(
        &call(&server, "search_files", json!({})),
        "query is required",
    );
    assert_tool_error(
        &call(&server, "search_files", json!({"query": "   "})),
        "query is required",
    );
    assert_tool_error(
        &call(&server, "search_files", json!({"query": 1})),
        "query must be a string",
    );
    assert_tool_error(
        &call(&server, "search_files", json!({"query": "a", "limit": 0})),
        "at least 1",
    );
    assert_tool_error(
        &call(
            &server,
            "search_files",
            json!({"query": "a", "path": "/nope"}),
        ),
        "not found",
    );
    assert_tool_error(
        &call(
            &server,
            "search_files",
            json!({"query": "a", "path": "../x"}),
        ),
        "invalid path",
    );
}

// --- search_files with SEARCH_SKIP ---------------------------------------------------------

/// A drive whose photo backup is too big to search, like the owner's.
fn drive() -> MemoryStorage {
    MemoryStorage::new()
        .file("/Backups/Immich/library/IMG_0001.jpg", "x")
        .file("/Backups/Immich/library/report-photo.jpg", "x")
        .file("/Backups/phone/report-2025.pdf", "x")
        .file("/Backups/report-index.txt", "x")
        .file("/Projects/alpha/report.md", "x")
        .file("/Projects/beta/notes.txt", "x")
        .dir("/R\u{e9}ports")
        .file("/report.txt", "x")
        .too_big("/Backups/Immich")
}

fn skipping(folders: &[&str]) -> ServerConfig {
    ServerConfig {
        search_skip: folders.iter().map(|&f| f.to_owned()).collect(),
        ..ServerConfig::default()
    }
}

fn found_paths(text: &str) -> Vec<&str> {
    text.lines()
        .filter_map(|line| line.split(") path: ").nth(1))
        .collect()
}

#[test]
fn search_goes_around_a_turned_off_folder() {
    // Without SEARCH_SKIP the whole tree is one search, and too big.
    let result = call(&server(drive()), "search_files", json!({"query": "report"}));
    assert_tool_error(&result, "too many entries");

    let server = server_with(drive(), skipping(&["/Backups/Immich"]));
    let result = call(&server, "search_files", json!({"query": "report"}));
    assert!(!is_error(&result), "{result}");
    let found = text(&result);
    // Folders before files at each level; accents ignored as the backend would.
    assert_eq!(
        found_paths(&found),
        [
            "/Backups/phone/report-2025.pdf",
            "/Backups/report-index.txt",
            "/Projects/alpha/report.md",
            "/R\u{e9}ports",
            "/report.txt"
        ],
        "{found}"
    );
    assert!(
        found.starts_with("5 matches for \"report\" under /:\n"),
        "{found}"
    );
    assert!(
        found.ends_with(
            "Skipped /Backups/Immich: search is turned off there. Browse with list_folder instead.\n"
        ),
        "{found}"
    );
    // Only the folders above the turned-off one are listed; nothing in it is touched.
    let calls = server_storage_calls(&server);
    assert_eq!(
        calls,
        [
            "list /",
            "list /Backups",
            "search /Backups/phone",
            "search /Projects",
            "search /R\u{e9}ports"
        ]
    );
}

/// The storage calls since the last time this was asked.
fn server_storage_calls(server: &chance_of_files_core::McpServer<MemoryStorage>) -> Vec<String> {
    server.storage().take_calls()
}

#[test]
fn a_turned_off_folder_at_the_top_still_matches_by_name() {
    let server = server_with(drive(), skipping(&["/Backups"]));
    let found = text(&call(&server, "search_files", json!({"query": "back"})));
    assert_eq!(found_paths(&found), ["/Backups"], "{found}");
    assert!(found.contains("Skipped /Backups: search is turned off there."));
    let report = text(&call(&server, "search_files", json!({"query": "report"})));
    assert_eq!(
        found_paths(&report),
        ["/Projects/alpha/report.md", "/R\u{e9}ports", "/report.txt"],
        "{report}"
    );
}

#[test]
fn turned_off_folders_match_ignoring_case_and_accents() {
    // The owner typed the folders differently from how list_folder shows them.
    let server = server_with(drive(), skipping(&["/backups/IMMICH", "/Re\u{301}ports"]));
    let found = text(&call(&server, "search_files", json!({"query": "report"})));
    assert!(
        found.contains("Skipped /Backups/Immich, /R\u{e9}ports: search is turned off there."),
        "{found}"
    );
    let calls = server_storage_calls(&server);
    assert!(!calls.iter().any(|c| c.contains("Immich")), "{calls:?}");
    // A search inside one, spelled either way, is refused.
    let result = call(
        &server,
        "search_files",
        json!({"query": "jpg", "path": "/Backups/Immich/library"}),
    );
    assert!(is_error(&result));
    assert!(text(&result).starts_with("Search is turned off under /backups/IMMICH"));
}

#[test]
fn searching_at_or_under_a_turned_off_folder_is_refused() {
    let server = server_with(drive(), skipping(&["/Backups/Immich"]));
    for path in [
        "/Backups/Immich",
        "/Backups/Immich/library",
        "Backups//Immich/",
    ] {
        let result = call(
            &server,
            "search_files",
            json!({"query": "jpg", "path": path}),
        );
        assert_eq!(
            text(&result),
            "Search is turned off under /Backups/Immich because it's too large. Browse it with \
             list_folder instead."
        );
        assert!(is_error(&result));
    }
    assert!(server_storage_calls(&server).is_empty());
    // A sibling whose name only starts the same is not under it.
    let server = server_with(
        drive().file("/Backups/Immich-old/a.jpg", "x"),
        skipping(&["/Backups/Immich"]),
    );
    let result = call(
        &server,
        "search_files",
        json!({"query": "jpg", "path": "/Backups/Immich-old"}),
    );
    assert_eq!(found_paths(&text(&result)), ["/Backups/Immich-old/a.jpg"]);
}

#[test]
fn a_search_with_nothing_turned_off_inside_is_one_call() {
    let server = server_with(drive(), skipping(&["/Backups/Immich"]));
    let found = text(&call(
        &server,
        "search_files",
        json!({"query": "o", "path": "/Projects"}),
    ));
    assert_eq!(
        found_paths(&found),
        ["/Projects/alpha/report.md", "/Projects/beta/notes.txt"],
        "{found}"
    );
    assert!(!found.contains("Skipped"), "{found}");
    assert_eq!(server_storage_calls(&server), ["search /Projects"]);
}

#[test]
fn a_split_search_stops_at_the_limit() {
    let server = server_with(drive(), skipping(&["/Backups/Immich"]));
    let found = text(&call(
        &server,
        "search_files",
        json!({"query": "report", "limit": 2}),
    ));
    assert_eq!(
        found_paths(&found),
        [
            "/Backups/phone/report-2025.pdf",
            "/Backups/report-index.txt"
        ],
        "{found}"
    );
    assert!(found.contains("there may be more"), "{found}");
    // Nothing after the second match is looked at.
    assert_eq!(
        server_storage_calls(&server),
        ["list /", "list /Backups", "search /Backups/phone"]
    );
}

#[test]
fn a_subfolder_too_big_to_search_is_noted() {
    let server = server_with(
        drive().too_big("/Projects/beta"),
        skipping(&["/Backups/Immich"]),
    );
    let result = call(&server, "search_files", json!({"query": "report"}));
    assert!(!is_error(&result), "{result}");
    let found = text(&result);
    assert_eq!(
        found_paths(&found),
        [
            "/Backups/phone/report-2025.pdf",
            "/Backups/report-index.txt",
            "/R\u{e9}ports",
            "/report.txt"
        ],
        "{found}"
    );
    assert!(
        found.contains(
            "Not searched, too many entries to go through in one call: /Projects. Search inside \
             it directly, with a narrower path.\n"
        ),
        "{found}"
    );
    // A folder above the turned-off one that is itself too big to list is noted the same way.
    let server = server_with(drive().too_big("/Backups"), skipping(&["/Backups/Immich"]));
    let found = text(&call(&server, "search_files", json!({"query": "report"})));
    assert!(
        found.contains("too many entries to go through in one call: /Backups."),
        "{found}"
    );
    assert!(found.contains("path: /Projects/alpha/report.md"), "{found}");
}

#[test]
fn a_search_that_could_search_nothing_is_an_error() {
    let storage = MemoryStorage::new()
        .file("/Backups/Immich/a.jpg", "x")
        .file("/Backups/Old/b.jpg", "x")
        .file("/Media/c.mp4", "x")
        .too_big("/Backups/Old")
        .too_big("/Media");
    let server = server_with(storage, skipping(&["/Backups/Immich"]));
    let result = call(&server, "search_files", json!({"query": "zzz"}));
    assert!(is_error(&result), "{result}");
    assert_eq!(
        text(&result),
        "Nothing under / could be searched.\n\
         Not searched, too many entries to go through in one call: /Backups/Old, /Media. \
         Search inside each directly, with a narrower path.\n\
         Skipped /Backups/Immich: search is turned off there. Browse with list_folder instead."
    );
    // A name that matches on the way is still worth returning.
    let found = text(&call(&server, "search_files", json!({"query": "media"})));
    assert!(
        found.starts_with("1 match for \"media\" under /:\n"),
        "{found}"
    );
}

#[test]
fn a_split_search_with_no_matches_says_what_it_left_out() {
    let server = server_with(drive(), skipping(&["/Backups/Immich"]));
    let found = text(&call(&server, "search_files", json!({"query": "IMG_0001"})));
    assert_eq!(
        found,
        "No files or folders under / have names containing \"IMG_0001\" in the folders that \
         were searched.\n\
         Skipped /Backups/Immich: search is turned off there. Browse with list_folder instead.\n"
    );
}

#[test]
fn a_split_search_stops_when_the_backend_fails() {
    let server = server_with(drive().dir("/broken"), skipping(&["/Backups/Immich"]));
    let result = call(&server, "search_files", json!({"query": "report"}));
    assert!(!is_error(&result), "{result}");
    let found = text(&result);
    assert_eq!(
        found_paths(&found),
        [
            "/Backups/phone/report-2025.pdf",
            "/Backups/report-index.txt"
        ],
        "{found}"
    );
    assert!(
        found.contains("Could not search /broken (storage backend error: GET [url] failed"),
        "{found}"
    );
    assert!(
        !found.contains("://") && !found.contains("dev-password"),
        "{found}"
    );
    assert!(
        found.contains("The search stopped early because the file server stopped answering"),
        "{found}"
    );
    // The root itself failing is an ordinary error.
    let server = server_with(drive(), skipping(&["/broken/x"]));
    let result = call(
        &server,
        "search_files",
        json!({"query": "a", "path": "/broken"}),
    );
    assert_tool_error(&result, "storage backend error");
}

#[test]
fn a_split_search_makes_at_most_40_calls() {
    let mut storage = MemoryStorage::new().file("/Huge/photos/x.jpg", "x");
    for i in 0..60 {
        storage = storage.file(&format!("/f{i:02}/report-{i:02}.txt"), "x");
    }
    let server = server_with(storage, skipping(&["/Huge/photos"]));
    let found = text(&call(&server, "search_files", json!({"query": "zzz"})));
    assert!(
        found.contains("The search stopped early because it reached its limit of 40 folders"),
        "{found}"
    );
    assert_eq!(server_storage_calls(&server).len(), 40);
}
