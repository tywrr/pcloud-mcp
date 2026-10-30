//! Turning storage paths into request paths and multistatus hrefs back into storage paths.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

/// Everything except RFC 3986 "unreserved" characters. Over-encoding is always safe in a path
/// segment, and it covers the characters that break URLs in file names: space, `#`, `?`, `%`.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encodes each segment of a normalized path: `/Café menu.txt` becomes
/// `/Caf%C3%A9%20menu.txt`. The root stays `/`.
pub(crate) fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.extend(utf8_percent_encode(segment, SEGMENT));
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Turns a multistatus `href` into a normalized absolute path below the served root, or
/// `None` when it points somewhere else (outside `base_path`, or with `..` in it).
///
/// `base_path` is the URL path the server is mounted at (empty or `/` for the root), as the
/// transport sees it; hrefs carry it as a prefix.
pub(crate) fn href_to_path(href: &str, base_path: &str) -> Option<String> {
    let href = href.trim();
    // Most servers send a path, but RFC 4918 also allows an absolute URL.
    let path = strip_origin(href);
    // A literal `?` or `#` in a name always arrives percent-encoded, so these only start a
    // query or fragment.
    let path = path.split(['?', '#']).next().unwrap_or_default();
    // Decode once, after splitting: `+` stays a plus sign (this is a path, not a form).
    let decoded = percent_decode_str(path).decode_utf8_lossy();
    let base = percent_decode_str(base_path).decode_utf8_lossy();
    let relative = strip_base(&decoded, &base)?;
    normalize_href_path(relative)
}

/// The last segment of a normalized path, or `/` for the root.
pub(crate) fn file_name(path: &str) -> &str {
    match path.rsplit('/').next() {
        Some(name) if !name.is_empty() => name,
        _ => "/",
    }
}

/// True when `child` sits directly inside the folder `parent` (both normalized).
pub(crate) fn is_direct_child(parent: &str, child: &str) -> bool {
    let Some(rest) = child.strip_prefix(parent) else {
        return false;
    };
    let rest = if parent == "/" {
        rest
    } else {
        match rest.strip_prefix('/') {
            Some(r) => r,
            None => return false,
        }
    };
    !rest.is_empty() && !rest.contains('/')
}

fn strip_origin(href: &str) -> &str {
    let lower = href.get(..8).unwrap_or(href).to_ascii_lowercase();
    let scheme_len = if lower.starts_with("https://") {
        8
    } else if lower.starts_with("http://") {
        7
    } else {
        return href;
    };
    let rest = &href[scheme_len..];
    rest.find('/').map_or("/", |i| &rest[i..])
}

fn strip_base<'a>(path: &'a str, base: &str) -> Option<&'a str> {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return Some(path);
    }
    let rest = path.strip_prefix(base)?;
    if rest.is_empty() || rest.starts_with('/') {
        Some(rest)
    } else {
        // `/davx/...` is not under `/dav`.
        None
    }
}

fn normalize_href_path(path: &str) -> Option<String> {
    let mut out = String::with_capacity(path.len() + 1);
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            s => {
                out.push('/');
                out.push_str(s);
            }
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_each_segment() {
        assert_eq!(encode_path("/"), "/");
        assert_eq!(encode_path(""), "/");
        assert_eq!(encode_path("/notes/todo.md"), "/notes/todo.md");
        assert_eq!(encode_path("/Café menu.txt"), "/Caf%C3%A9%20menu.txt");
        assert_eq!(
            encode_path("/a#b/q?.txt/100%.txt"),
            "/a%23b/q%3F.txt/100%25.txt"
        );
        assert_eq!(encode_path("/plus+sign &amp"), "/plus%2Bsign%20%26amp");
        assert_eq!(encode_path("/日本語/ファイル.txt").matches('%').count(), 21);
        assert_eq!(encode_path("/folder/"), "/folder");
    }

    #[test]
    fn hrefs_become_normalized_paths() {
        assert_eq!(href_to_path("/", "").as_deref(), Some("/"));
        assert_eq!(href_to_path("/notes/", "").as_deref(), Some("/notes"));
        assert_eq!(
            href_to_path("/Caf%C3%A9%20menu.txt", "").as_deref(),
            Some("/Café menu.txt")
        );
        // `+` is not a space in a path.
        assert_eq!(
            href_to_path("/plus+sign.txt", "").as_deref(),
            Some("/plus+sign.txt")
        );
        assert_eq!(href_to_path("//a//b/./c/", "").as_deref(), Some("/a/b/c"));
        assert_eq!(href_to_path("/a/../etc", ""), None);
        assert_eq!(
            href_to_path("  /trimmed  ", "").as_deref(),
            Some("/trimmed")
        );
    }

    #[test]
    fn absolute_url_hrefs_and_base_paths() {
        assert_eq!(
            href_to_path("http://127.0.0.1:8080/notes/todo.md", "").as_deref(),
            Some("/notes/todo.md")
        );
        assert_eq!(
            href_to_path("HTTPS://example.com", "").as_deref(),
            Some("/")
        );
        assert_eq!(
            href_to_path("/dav/notes/todo.md", "/dav").as_deref(),
            Some("/notes/todo.md")
        );
        assert_eq!(href_to_path("/dav/", "/dav/").as_deref(), Some("/"));
        assert_eq!(href_to_path("/dav", "/dav").as_deref(), Some("/"));
        assert_eq!(href_to_path("/davx/a", "/dav"), None);
        assert_eq!(href_to_path("/other/a", "/dav"), None);
        // The base path may itself be percent-encoded.
        assert_eq!(
            href_to_path("/my%20dav/a%20b", "/my dav").as_deref(),
            Some("/a b")
        );
        assert_eq!(
            href_to_path("/my%20dav/a%20b", "/my%20dav").as_deref(),
            Some("/a b")
        );
    }

    #[test]
    fn round_trips_awkward_names() {
        for name in [
            "/a#b.txt",
            "/q?.txt",
            "/100%.txt",
            "/plus+sign.txt",
            "/amp&lt.txt",
            "/<tag>.txt",
            "/it's.txt",
            "/ünïcödé.txt",
            "/semi;colon.txt",
            "/sub dir/x y.txt",
        ] {
            assert_eq!(href_to_path(&encode_path(name), "").as_deref(), Some(name));
        }
    }

    #[test]
    fn names_and_children() {
        assert_eq!(file_name("/"), "/");
        assert_eq!(file_name("/a"), "a");
        assert_eq!(file_name("/a/b c.txt"), "b c.txt");
        assert!(is_direct_child("/", "/a"));
        assert!(!is_direct_child("/", "/"));
        assert!(!is_direct_child("/", "/a/b"));
        assert!(is_direct_child("/a", "/a/b"));
        assert!(!is_direct_child("/a", "/a"));
        assert!(!is_direct_child("/a", "/ab"));
        assert!(!is_direct_child("/a", "/a/b/c"));
    }
}
