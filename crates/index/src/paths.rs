//! Path arithmetic on stored (normalized, absolute) paths.

/// `/a/b` → `/a`, `/a` → `/`, `/` → `/`.
pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

/// Bounds such that `lo <= p < hi` holds exactly for the paths strictly under `folder`:
/// `'0'` is the byte after `'/'`, so `/a0` sorts after every `/a/...` and before `/a0...`.
pub fn under_range(folder: &str) -> (String, String) {
    if folder == "/" {
        ("/".into(), "0".into())
    } else {
        (format!("{folder}/"), format!("{folder}0"))
    }
}

/// How many levels below the root a path is: `/` 0, `/a` 1, `/a/b` 2.
pub fn depth(path: &str) -> u32 {
    if path == "/" {
        0
    } else {
        path.matches('/').count() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_ranges_depths() {
        assert_eq!(parent_of("/a/b"), "/a");
        assert_eq!(parent_of("/a"), "/");
        assert_eq!(parent_of("/"), "/");
        let (lo, hi) = under_range("/a");
        for inside in ["/a/", "/a/b", "/a/zzz/\u{10ffff}"] {
            assert!(lo.as_str() <= inside && inside < hi.as_str(), "{inside}");
        }
        for outside in ["/a", "/a0", "/a b", "/b", "/a.txt"] {
            assert!(
                !(lo.as_str() <= outside && outside < hi.as_str()),
                "{outside}"
            );
        }
        let (lo, hi) = under_range("/");
        assert!(lo.as_str() <= "/x" && "/x" < hi.as_str());
        assert_eq!((depth("/"), depth("/a"), depth("/a/b")), (0, 1, 2));
    }
}
