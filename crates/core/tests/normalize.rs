use chance_of_files_core::{StorageError, normalize_path};

fn ok(input: &str) -> String {
    normalize_path(input).unwrap_or_else(|e| panic!("{input:?} was rejected: {e}"))
}

fn rejected(input: &str) -> bool {
    matches!(normalize_path(input), Err(StorageError::InvalidPath(_)))
}

#[test]
fn root_forms() {
    for input in ["", "/", "//", "///", ".", "/.", "./", "/./."] {
        assert_eq!(ok(input), "/", "{input:?}");
    }
}

#[test]
fn collapses_slashes_and_dots() {
    assert_eq!(ok("/Library/notes.md"), "/Library/notes.md");
    assert_eq!(ok("Library/notes.md"), "/Library/notes.md");
    assert_eq!(ok("//Library///notes.md"), "/Library/notes.md");
    assert_eq!(ok("/Library/"), "/Library");
    assert_eq!(ok("./Library/./notes.md/."), "/Library/notes.md");
}

#[test]
fn keeps_names_as_given() {
    assert_eq!(ok("/My Notes/a b.txt"), "/My Notes/a b.txt");
    assert_eq!(ok("/Café/naïve 😀.md"), "/Café/naïve 😀.md");
    assert_eq!(ok("/.hidden/..."), "/.hidden/...");
    assert_eq!(ok("/a..b/..c"), "/a..b/..c");
    // Backslash is a literal character, not a separator.
    assert_eq!(ok("a\\b"), "/a\\b");
    assert_eq!(ok("..\\etc"), "/..\\etc");
    // No percent-decoding: this is a name, not "..".
    assert_eq!(ok("/%2e%2e/x"), "/%2e%2e/x");
}

#[test]
fn rejects_traversal() {
    for input in [
        "..",
        "/..",
        "../x",
        "/a/../b",
        "/a/..",
        "a/b/../../..",
        "/./../",
    ] {
        assert!(rejected(input), "{input:?} should be rejected");
    }
}

#[test]
fn rejects_control_characters() {
    for input in [
        "a\0b",
        "/a\nb",
        "/tab\there",
        "\r",
        "/del\u{7f}",
        "/c1\u{85}",
    ] {
        assert!(rejected(input), "{input:?} should be rejected");
    }
}
