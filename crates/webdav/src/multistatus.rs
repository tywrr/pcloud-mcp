//! Streaming parser for WebDAV `207 Multi-Status` PROPFIND responses (RFC 4918 section 13).
//!
//! It matches elements by namespace, not prefix: rclone writes `D:`, others use `d:`, `lp1:`
//! or a default namespace. Entries are handed to a callback as soon as each `<response>`
//! closes, so a search can stop early instead of building the whole (possibly large) tree,
//! and a filter sees each path as soon as its `<href>` closes, so a search skips the rest of
//! every `<response>` it doesn't want.

use std::ops::ControlFlow;

use chance_of_files_core::Entry;
use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::{Namespace, QName, ResolveResult};

use crate::paths::{file_name, href_to_path};
use crate::time::http_date_to_rfc3339;

const DAV: Namespace<'static> = Namespace("DAV:");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Response,
    Href,
    Status,
    Propstat,
    Prop,
    ResourceType,
    Collection,
    ContentLength,
    LastModified,
    ContentType,
    Other,
}

impl Tag {
    fn classify(ns: &ResolveResult<'_>, local: &str) -> Self {
        if !matches!(ns, ResolveResult::Bound(n) if *n == DAV) {
            return Self::Other;
        }
        match local {
            "response" => Self::Response,
            "href" => Self::Href,
            "status" => Self::Status,
            "propstat" => Self::Propstat,
            "prop" => Self::Prop,
            "resourcetype" => Self::ResourceType,
            "collection" => Self::Collection,
            "getcontentlength" => Self::ContentLength,
            "getlastmodified" => Self::LastModified,
            "getcontenttype" => Self::ContentType,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Default)]
struct Props {
    is_collection: bool,
    saw_resourcetype: bool,
    content_length: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
}

impl Props {
    fn merge(&mut self, other: Props) {
        self.is_collection |= other.is_collection;
        self.saw_resourcetype |= other.saw_resourcetype;
        self.content_length = other.content_length.or(self.content_length.take());
        self.last_modified = other.last_modified.or(self.last_modified.take());
        self.content_type = other.content_type.or(self.content_type.take());
    }
}

#[derive(Debug, Default)]
struct ResponseState {
    /// From the `<href>`; `None` until it closes, or when it points outside the served root.
    path: Option<String>,
    /// The href ended in a slash (a folder, by convention).
    slash: bool,
    /// The filter turned this resource down; the rest of it is skipped unread.
    skip: bool,
    /// A `<status>` directly in `<response>` reports on the whole resource (e.g. 404).
    status: Option<u16>,
    props: Props,
}

#[derive(Debug, Default)]
struct PropstatState {
    status: Option<u16>,
    props: Props,
}

/// Parses a multistatus body and calls `visit` with each resource it describes, in document
/// order. `base_path` is stripped from hrefs (see [`crate::Transport::base_path`]). Resources
/// whose href lies outside it, or that report a non-2xx status, are skipped. Properties from
/// non-2xx `<propstat>` blocks (the "404 Not Found" list of properties a resource lacks) are
/// ignored.
///
/// `wanted` gets each resource's path and name before its properties are read (RFC 4918 puts
/// `<href>` first); resources it turns down never reach `visit`, and the rest of their XML is
/// skipped without being parsed.
pub(crate) fn parse_multistatus<W, F>(
    xml: &[u8],
    base_path: &str,
    wanted: W,
    visit: F,
) -> Result<(), String>
where
    W: FnMut(&str, &str) -> bool,
    F: FnMut(Entry) -> ControlFlow<()>,
{
    let mut caller = Caller {
        base_path,
        wanted,
        visit,
    };
    let mut reader = NsReader::from_reader(xml);
    let mut stack: Vec<Tag> = Vec::with_capacity(8);
    let mut response: Option<ResponseState> = None;
    // The open `<response>`'s tag as written (`D:response`), to skip to its end.
    let mut response_tag = String::new();
    let mut propstat: Option<PropstatState> = None;
    // Character data of the innermost open element. Text arrives split around entity
    // references (`a &amp; b` is three events), so it is collected and trimmed on close.
    let mut text = String::new();

    loop {
        let (ns, event) = reader
            .read_resolved_event()
            .map_err(|e| format!("invalid multistatus XML: {e}"))?;
        let (tag, closes_now) = match event {
            Event::Start(ref e) => (Tag::classify(&ns, e.local_name().as_ref()), false),
            Event::Empty(ref e) => (Tag::classify(&ns, e.local_name().as_ref()), true),
            Event::End(_) => {
                let Some(tag) = stack.last().copied() else {
                    continue;
                };
                let parent = stack.len().checked_sub(2).map(|i| stack[i]);
                let flow = close(
                    tag,
                    parent,
                    &text,
                    &mut response,
                    &mut propstat,
                    &mut caller,
                );
                stack.pop();
                text.clear();
                if flow.is_break() {
                    return Ok(());
                }
                if tag == Tag::Href && response.as_ref().is_some_and(|r| r.skip) {
                    // Nothing else in this response matters: jump past its end tag without
                    // resolving namespaces or looking at text. Elements in between can't change
                    // the namespaces in force outside it.
                    reader
                        .read_to_end(QName(&response_tag))
                        .map_err(|e| format!("invalid multistatus XML: {e}"))?;
                    response = None;
                    propstat = None;
                    stack.pop();
                }
                continue;
            }
            Event::Text(t) => {
                text.push_str(&t.xml10_content());
                continue;
            }
            Event::CData(c) => {
                text.push_str(&c.xml10_content());
                continue;
            }
            Event::GeneralRef(r) => {
                match r.resolve_char_ref() {
                    Ok(Some(c)) => text.push(c),
                    Ok(None) => {
                        if let Some(s) = quick_xml::escape::resolve_predefined_entity(&r) {
                            text.push_str(s);
                        }
                    }
                    Err(e) => return Err(format!("invalid multistatus XML: {e}")),
                }
                continue;
            }
            Event::Eof => return Ok(()),
            _ => continue,
        };

        // An element opens.
        match tag {
            Tag::Response => {
                response = Some(ResponseState::default());
                if let Event::Start(e) = &event {
                    response_tag.clear();
                    response_tag.push_str(e.name().as_ref());
                }
            }
            Tag::Propstat => propstat = Some(PropstatState::default()),
            _ => {}
        }
        stack.push(tag);
        text.clear();
        if closes_now {
            let parent = stack.len().checked_sub(2).map(|i| stack[i]);
            let flow = close(tag, parent, "", &mut response, &mut propstat, &mut caller);
            stack.pop();
            if flow.is_break() {
                return Ok(());
            }
        }
    }
}

/// What [`parse_multistatus`] was given besides the XML.
struct Caller<'a, W, F> {
    base_path: &'a str,
    wanted: W,
    visit: F,
}

/// Handles the end of `tag`, whose parent element is `parent`.
fn close<W, F>(
    tag: Tag,
    parent: Option<Tag>,
    text: &str,
    response: &mut Option<ResponseState>,
    propstat: &mut Option<PropstatState>,
    caller: &mut Caller<'_, W, F>,
) -> ControlFlow<()>
where
    W: FnMut(&str, &str) -> bool,
    F: FnMut(Entry) -> ControlFlow<()>,
{
    let in_prop = parent == Some(Tag::Prop);
    let props = propstat.as_mut().map(|p| &mut p.props);
    match (tag, parent) {
        (Tag::Response, _) => {
            if let Some(entry) = response.take().and_then(to_entry) {
                return (caller.visit)(entry);
            }
        }
        (Tag::Href, Some(Tag::Response)) => {
            if let Some(r) = response.as_mut() {
                let href = text.trim();
                r.slash = href.ends_with('/');
                r.path = href_to_path(href, caller.base_path);
                r.skip = r
                    .path
                    .as_deref()
                    .is_none_or(|path| !(caller.wanted)(path, file_name(path)));
            }
        }
        (Tag::Status, Some(Tag::Response)) => {
            if let Some(r) = response.as_mut() {
                r.status = parse_status_line(text);
            }
        }
        (Tag::Status, Some(Tag::Propstat)) => {
            if let Some(p) = propstat.as_mut() {
                p.status = parse_status_line(text);
            }
        }
        (Tag::Collection, Some(Tag::ResourceType)) => {
            if let Some(p) = props {
                p.is_collection = true;
            }
        }
        (Tag::ResourceType, _) if in_prop => {
            if let Some(p) = props {
                p.saw_resourcetype = true;
            }
        }
        (Tag::ContentLength, _) if in_prop => {
            if let Some(p) = props {
                p.content_length = non_empty(text);
            }
        }
        (Tag::LastModified, _) if in_prop => {
            if let Some(p) = props {
                p.last_modified = non_empty(text);
            }
        }
        (Tag::ContentType, _) if in_prop => {
            if let Some(p) = props {
                p.content_type = non_empty(text);
            }
        }
        (Tag::Propstat, Some(Tag::Response)) => {
            if let (Some(p), Some(r)) = (propstat.take(), response.as_mut())
                && p.status.is_none_or(is_success)
            {
                r.props.merge(p.props);
            }
        }
        _ => {}
    }
    ControlFlow::Continue(())
}

fn to_entry(response: ResponseState) -> Option<Entry> {
    if response.status.is_some_and(|s| !is_success(s)) {
        return None;
    }
    let path = response.path?;
    let props = response.props;
    // rclone always reports resourcetype; the trailing slash is a fallback for servers that
    // leave it out.
    let is_dir = props.is_collection || (!props.saw_resourcetype && response.slash);
    Some(Entry {
        name: file_name(&path).to_owned(),
        is_dir,
        size: if is_dir {
            None
        } else {
            props.content_length.and_then(|s| s.parse().ok())
        },
        modified: props
            .last_modified
            .as_deref()
            .and_then(http_date_to_rfc3339),
        content_type: if is_dir { None } else { props.content_type },
        path,
    })
}

/// `HTTP/1.1 200 OK` to 200.
fn parse_status_line(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

fn non_empty(text: &str) -> Option<String> {
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(xml: &str, base: &str) -> Vec<Entry> {
        let mut out = Vec::new();
        parse_multistatus(
            xml.as_bytes(),
            base,
            |_, _| true,
            |e| {
                out.push(e);
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        out
    }

    fn paths(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    fn find<'a>(entries: &'a [Entry], path: &str) -> &'a Entry {
        entries
            .iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("{path} missing"))
    }

    const ROOT_DEPTH1: &str = include_str!("../tests/data/rclone-root-depth1.xml");
    const ROOT_INFINITY: &str = include_str!("../tests/data/rclone-root-infinity.xml");
    const FILE_DEPTH0: &str = include_str!("../tests/data/rclone-file-depth0.xml");
    const NESTED_DEPTH1: &str = include_str!("../tests/data/rclone-nested-depth1.xml");
    const ALLPROP_DEPTH1: &str = include_str!("../tests/data/rclone-allprop-depth1.xml");
    const SPECIAL_NAMES: &str = include_str!("../tests/data/rclone-special-names-depth1.xml");

    #[test]
    fn rclone_root_listing() {
        let entries = parse(ROOT_DEPTH1, "");
        assert_eq!(
            paths(&entries),
            [
                "/",
                "/Café menu.txt",
                "/README.txt",
                "/binary.bin",
                "/deep",
                "/images",
                "/notes"
            ]
        );
        assert_eq!(
            find(&entries, "/Café menu.txt"),
            &Entry {
                path: "/Café menu.txt".into(),
                name: "Café menu.txt".into(),
                is_dir: false,
                size: Some(100),
                modified: Some("2026-10-30T00:32:52Z".into()),
                content_type: Some("text/plain; charset=utf-8".into()),
            }
        );
        let root = find(&entries, "/");
        assert!(root.is_dir);
        assert_eq!(root.name, "/");
        let deep = find(&entries, "/deep");
        assert!(deep.is_dir);
        assert_eq!(deep.name, "deep");
        assert_eq!(deep.size, None);
        assert_eq!(deep.content_type, None);
        assert_eq!(deep.modified.as_deref(), Some("2026-10-30T00:32:52Z"));
        assert_eq!(find(&entries, "/binary.bin").size, Some(19));
        assert_eq!(
            find(&entries, "/binary.bin").content_type.as_deref(),
            Some("application/octet-stream")
        );
    }

    #[test]
    fn rclone_infinity_walks_the_whole_tree() {
        let entries = parse(ROOT_INFINITY, "");
        assert_eq!(entries.len(), 13);
        let leaf = find(&entries, "/deep/a/b/c/leaf.txt");
        assert_eq!(leaf.name, "leaf.txt");
        assert_eq!(leaf.size, Some(48));
        assert!(find(&entries, "/deep/a/b/c").is_dir);
        assert_eq!(
            find(&entries, "/images/pixel.png").content_type.as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn rclone_single_file_and_nested_folder() {
        let file = parse(FILE_DEPTH0, "");
        assert_eq!(paths(&file), ["/Café menu.txt"]);
        let nested = parse(NESTED_DEPTH1, "");
        assert_eq!(paths(&nested), ["/deep/a", "/deep/a/b"]);
        assert!(nested.iter().all(|e| e.is_dir && e.size.is_none()));
    }

    #[test]
    fn rclone_allprop_with_redeclared_namespace() {
        // Without a request body rclone sends every property, including lock info that
        // redeclares the DAV: prefix on a nested element.
        let entries = parse(ALLPROP_DEPTH1, "");
        assert_eq!(paths(&entries), ["/notes", "/notes/todo.md"]);
        let todo = find(&entries, "/notes/todo.md");
        assert_eq!(todo.size, Some(169));
        assert_eq!(
            todo.content_type.as_deref(),
            Some("text/markdown; charset=utf-8")
        );
        assert!(find(&entries, "/notes").is_dir);
    }

    #[test]
    fn rclone_awkward_names() {
        let entries = parse(SPECIAL_NAMES, "");
        let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "/",
                "100%.txt",
                "<tag>.txt",
                "a#b.txt",
                "amp&lt.txt",
                "empty.txt",
                "it's.txt",
                "plus+sign.txt",
                "q?.txt",
                "semi;colon.txt",
                "sub dir",
                "ünïcödé.txt",
            ]
        );
        assert_eq!(find(&entries, "/empty.txt").size, Some(0));
        assert!(find(&entries, "/sub dir").is_dir);
    }

    #[test]
    fn other_prefixes_urls_and_failed_statuses() {
        // Apache-style: default namespace, a second prefix bound to DAV:, absolute URLs,
        // indentation, a non-DAV element with a DAV-like name, and a 404 response.
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:lp1="DAV:" xmlns:x="urn:example">
  <response>
    <href>https://files.example.com/dav/Reports%202026/</href>
    <propstat>
      <prop>
        <lp1:resourcetype><lp1:collection/></lp1:resourcetype>
        <lp1:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</lp1:getlastmodified>
        <getcontenttype>httpd/unix-directory</getcontenttype>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
  <response>
    <href>/dav/Reports%202026/Q3 &amp; Q4.csv</href>
    <propstat>
      <prop>
        <resourcetype/>
        <getcontentlength> 1234 </getcontentlength>
        <x:getcontentlength>999</x:getcontentlength>
        <getcontenttype><![CDATA[text/csv]]></getcontenttype>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
    <propstat>
      <prop><getlastmodified>Mon, 07 Nov 1994 08:49:37 GMT</getlastmodified></prop>
      <status>HTTP/1.1 404 Not Found</status>
    </propstat>
  </response>
  <response>
    <href>/dav/gone.txt</href>
    <status>HTTP/1.1 404 Not Found</status>
  </response>
  <response>
    <href>/elsewhere/outside.txt</href>
    <propstat><prop><resourcetype/></prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
</multistatus>"#;
        let entries = parse(xml, "/dav");
        assert_eq!(
            paths(&entries),
            ["/Reports 2026", "/Reports 2026/Q3 & Q4.csv"]
        );
        let folder = &entries[0];
        assert!(folder.is_dir);
        assert_eq!(folder.content_type, None);
        assert_eq!(folder.modified.as_deref(), Some("1994-11-06T08:49:37Z"));
        let csv = &entries[1];
        assert_eq!(csv.name, "Q3 & Q4.csv");
        assert_eq!(csv.size, Some(1234));
        assert_eq!(csv.content_type.as_deref(), Some("text/csv"));
        // The date sat in a 404 propstat, so it doesn't count.
        assert_eq!(csv.modified, None);
    }

    #[test]
    fn char_references_and_missing_resourcetype() {
        let xml = r#"<d:multistatus xmlns:d="DAV:">
<d:response><d:href>/caf&#xE9;/</d:href></d:response>
<d:response><d:href>/caf&#233;/menu.txt</d:href></d:response>
</d:multistatus>"#;
        let entries = parse(xml, "");
        assert_eq!(paths(&entries), ["/café", "/café/menu.txt"]);
        // No resourcetype at all: the trailing slash decides.
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
    }

    #[test]
    fn the_filter_sees_each_path_before_its_properties() {
        let mut asked = Vec::new();
        let mut kept = Vec::new();
        parse_multistatus(
            ROOT_DEPTH1.as_bytes(),
            "",
            |path, name| {
                asked.push((path.to_owned(), name.to_owned()));
                name.ends_with(".txt")
            },
            |e| {
                kept.push(e);
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(asked.len(), 7);
        assert_eq!(asked[0], ("/".to_owned(), "/".to_owned()));
        assert_eq!(
            asked[1],
            ("/Café menu.txt".to_owned(), "Café menu.txt".to_owned())
        );
        // Kept entries are complete; the rest never became entries.
        assert_eq!(paths(&kept), ["/Café menu.txt", "/README.txt"]);
        assert_eq!(kept[0], parse(ROOT_DEPTH1, "")[1]);
    }

    #[test]
    fn skipped_responses_leave_namespaces_intact() {
        // Skipping jumps over the rest of a response without tracking namespaces, so the
        // declarations on and inside it must not leak out, nor the outer ones get lost.
        let xml = r#"<multistatus xmlns="DAV:">
<x:response xmlns:x="DAV:"><x:href>/skip-1</x:href><x:propstat xmlns:y="urn:other">
  <x:prop><x:resourcetype><y:collection/></x:resourcetype></x:prop></x:propstat></x:response>
<response><href>/skip-2</href><propstat><prop><response><href>/nested</href></response></prop></propstat></response>
<response><href>/keep.txt</href><propstat><prop><getcontentlength>5</getcontentlength></prop>
  <status>HTTP/1.1 200 OK</status></propstat></response>
<x:response><x:href>/unbound-prefix</x:href></x:response>
</multistatus>"#;
        let mut kept = Vec::new();
        parse_multistatus(
            xml.as_bytes(),
            "",
            |_, name| name.starts_with("keep") || name.starts_with("unbound"),
            |e| {
                kept.push(e);
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(paths(&kept), ["/keep.txt"]);
        assert_eq!(kept[0].size, Some(5));
    }

    #[test]
    fn stops_when_the_visitor_says_so() {
        let mut seen = 0;
        parse_multistatus(
            ROOT_INFINITY.as_bytes(),
            "",
            |_, _| true,
            |_| {
                seen += 1;
                if seen == 3 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            },
        )
        .unwrap();
        assert_eq!(seen, 3);
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        let err = parse_multistatus(
            b"<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/a</d:hre></d:response>",
            "",
            |_, _| true,
            |_| ControlFlow::Continue(()),
        )
        .unwrap_err();
        assert!(err.starts_with("invalid multistatus XML"), "{err}");
        // Not XML at all (an HTML error page, say) yields nothing rather than failing.
        let mut n = 0;
        parse_multistatus(
            b"<html><body>502 Bad Gateway</body></html>",
            "",
            |_, _| true,
            |_| {
                n += 1;
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(n, 0);
    }
}
