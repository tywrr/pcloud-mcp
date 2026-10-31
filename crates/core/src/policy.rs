//! Which paths exist as far as clients can tell, and which may change.
//!
//! [`Policy`] is the only enforcement point for hidden and protected paths: rclone's filters
//! don't cover single-object calls (phase 0), so nothing below this module hides anything. A
//! backend takes only [`CheckedPath`]s, and only a `Policy` makes one from client input.
//!
//! Paths are compared folded (lower case, decomposed, without accents; see
//! [`crate::matching`]), so "/crypto folder" and "/Crypto Folder" are the same rule and a
//! name that differs only in Unicode form can't slip past one.

use std::fmt;

use crate::matching::fold;
use crate::storage::{StorageError, normalize_path};

/// A normalized absolute path that has passed [`Policy::check`]: not hidden, and not under
/// anything hidden.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckedPath(String);

/// Only this crate makes these: from [`Policy::check`], or as children of a checked folder
/// that a policy-aware caller has already ruled on (see [`CheckedPath::child`]).
impl CheckedPath {
    /// The path, e.g. `/` or `/Notes/todo.md`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path without its leading slash (`""` for the root), as rclone wants it.
    pub fn relative(&self) -> &str {
        &self.0[1..]
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// `name` inside this folder. The caller vouches that `name` isn't hidden (it came from
    /// a listing the policy filtered, or isn't excluded).
    pub(crate) fn child(&self, name: &str) -> Self {
        if self.is_root() {
            Self(format!("/{name}"))
        } else {
            Self(format!("{}/{name}", self.0))
        }
    }
}

impl fmt::Display for CheckedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CheckedPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A set of folders compared folded: is a path one of them or inside one, and does a folder
/// have one somewhere below it.
#[derive(Debug, Clone, Default)]
pub struct PathSet {
    /// As configured, normalized, for messages.
    shown: Vec<String>,
    folded: Vec<String>,
}

impl PathSet {
    pub fn new<I, S>(paths: I) -> Result<Self, StorageError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = Self::default();
        for path in paths {
            let path = normalize_path(path.as_ref())?;
            let folded = fold(&path);
            if !set.folded.contains(&folded) {
                set.shown.push(path);
                set.folded.push(folded);
            }
        }
        Ok(set)
    }

    pub fn is_empty(&self) -> bool {
        self.folded.is_empty()
    }

    /// The paths as configured (normalized).
    pub fn paths(&self) -> &[String] {
        &self.shown
    }

    /// The configured path that `path` is at or under, if any.
    pub fn covering(&self, path: &str) -> Option<&str> {
        let path = fold(path);
        self.folded
            .iter()
            .position(|p| is_at_or_under(&path, p))
            .map(|i| self.shown[i].as_str())
    }

    pub fn covers(&self, path: &str) -> bool {
        self.covering(path).is_some()
    }

    /// True when `path` is exactly one of the set (ignoring case and accents).
    pub fn contains(&self, path: &str) -> bool {
        let path = fold(path);
        self.folded.contains(&path)
    }

    /// True when one of the set is strictly inside `folder`.
    pub fn any_below(&self, folder: &str) -> bool {
        let folder = fold(folder);
        self.folded.iter().any(|p| is_under(p, &folder))
    }

    fn extend(&mut self, other: &Self) {
        for (shown, folded) in other.shown.iter().zip(&other.folded) {
            if !self.folded.contains(folded) {
                self.shown.push(shown.clone());
                self.folded.push(folded.clone());
            }
        }
    }
}

/// Hidden, protected and search-skipped paths.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    hidden: PathSet,
    protected: PathSet,
    search_skip: PathSet,
}

impl Policy {
    /// Fails when a path can't be normalized, or when `/` itself is hidden.
    pub fn new(
        hidden: &[impl AsRef<str>],
        protected: &[impl AsRef<str>],
        search_skip: &[impl AsRef<str>],
    ) -> Result<Self, StorageError> {
        let hidden = PathSet::new(hidden)?;
        if hidden.contains("/") {
            return Err(StorageError::InvalidPath(
                "\"/\" can't be hidden; that would hide everything".into(),
            ));
        }
        Ok(Self {
            hidden,
            protected: PathSet::new(protected)?,
            search_skip: PathSet::new(search_skip)?,
        })
    }

    /// Normalizes client input and refuses anything hidden or under something hidden. A
    /// hidden path is reported as not found, exactly like one that doesn't exist.
    pub fn check(&self, input: &str) -> Result<CheckedPath, StorageError> {
        let path = normalize_path(input)?;
        if self.hidden.covers(&path) {
            return Err(StorageError::NotFound(path));
        }
        Ok(CheckedPath(path))
    }

    /// Like [`Policy::check`], and also refuses a path that a write would affect while it, an
    /// ancestor or a descendant is protected. Moving or deleting `/` is the caller's to refuse.
    pub fn check_write(&self, input: &str) -> Result<CheckedPath, StorageError> {
        let path = self.check(input)?;
        if let Some(rule) = self.protected.covering(path.as_str()) {
            return Err(StorageError::Protected(format!(
                "{path} (everything under {rule} is read-only)"
            )));
        }
        if self.protected.any_below(path.as_str()) {
            return Err(StorageError::Protected(format!(
                "{path} contains a read-only folder"
            )));
        }
        Ok(path)
    }

    /// Whether an entry a backend returned may be shown.
    pub fn visible(&self, path: &str) -> bool {
        !self.hidden.covers(path)
    }

    pub fn hidden(&self) -> &PathSet {
        &self.hidden
    }

    pub fn protected(&self) -> &PathSet {
        &self.protected
    }

    pub fn search_skip(&self) -> &PathSet {
        &self.search_skip
    }

    /// Hidden and search-skipped paths together: what a name search must not go into.
    pub(crate) fn search_excludes(&self) -> PathSet {
        let mut set = self.hidden.clone();
        set.extend(&self.search_skip);
        set
    }
}

/// True when `path` is `folder` or anywhere inside it (both normalized and folded alike).
pub(crate) fn is_at_or_under(path: &str, folder: &str) -> bool {
    folder == "/"
        || path
            .strip_prefix(folder)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// True when `path` is inside `folder`, at any depth, but not `folder` itself.
pub(crate) fn is_under(path: &str, folder: &str) -> bool {
    path != folder && is_at_or_under(path, folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy::new(
            &["/Crypto Folder", "/Private/Caf\u{e9}"],
            &["/Backups/Immich"],
            &["/Media"],
        )
        .unwrap()
    }

    #[test]
    fn hidden_paths_and_everything_under_them_are_not_found() {
        let p = policy();
        for input in [
            "/Crypto Folder",
            "Crypto Folder",
            "/crypto folder/",
            "//CRYPTO FOLDER//x/./y",
            "/Crypto Folder/a/b.txt",
            "/private/cafe\u{301}",
            "/Private/CAFE/z",
        ] {
            assert!(
                matches!(p.check(input), Err(StorageError::NotFound(_))),
                "{input:?}"
            );
        }
        for input in [
            "/",
            "/Crypto",
            "/Crypto Folder2",
            "/Private",
            "/Private/Cafeteria",
        ] {
            assert!(p.check(input).is_ok(), "{input:?}");
        }
    }

    #[test]
    fn the_root_cant_be_hidden() {
        assert!(Policy::new(&["//"], &[] as &[&str], &[] as &[&str]).is_err());
    }

    #[test]
    fn visibility_of_listed_entries() {
        let p = policy();
        assert!(!p.visible("/CRYPTO FOLDER"));
        assert!(!p.visible("/Crypto Folder/x"));
        assert!(p.visible("/Crypto Folder x"));
    }

    #[test]
    fn protected_paths_refuse_writes_around_them() {
        let p = policy();
        for input in ["/Backups/Immich", "/backups/immich/a.jpg", "/Backups", "/"] {
            assert!(
                matches!(p.check_write(input), Err(StorageError::Protected(_))),
                "{input:?}"
            );
        }
        assert!(p.check_write("/Backups/phone").is_ok());
        assert!(p.check("/Backups/Immich/a.jpg").is_ok());
        assert!(matches!(
            p.check_write("/Crypto Folder/x"),
            Err(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn checked_paths() {
        let p = policy();
        let root = p.check("").unwrap();
        assert!(root.is_root());
        assert_eq!(root.relative(), "");
        assert_eq!(root.child("a").as_str(), "/a");
        let a = p.check("/a//b/").unwrap();
        assert_eq!(a.relative(), "a/b");
        assert_eq!(a.child("c").as_str(), "/a/b/c");
    }

    #[test]
    fn path_sets() {
        let set = PathSet::new(["/A/B", "/a/b/", "/C"]).unwrap();
        assert_eq!(set.paths(), ["/A/B", "/C"]);
        assert!(set.any_below("/a"));
        assert!(set.any_below("/"));
        assert!(!set.any_below("/A/B"));
        assert!(set.contains("/a/B"));
        assert_eq!(set.covering("/a/b/c"), Some("/A/B"));
    }
}
