//! Each tool's happy and error paths.

mod common;

use base64::Engine;
use common::*;
use pcloud_mcp_core::ServerConfig;
use serde_json::json;

fn sample() -> pcloud_mcp_core::McpServer<MemoryStorage> {
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
