//! OAuth scopes granted to the client behind a request.
//!
//! The auth Worker forwards the grant's scopes in `X-CoF-Scopes`; the server trusts that
//! header only on a request that carried the internal token, and sets
//! [`crate::HttpRequest::scopes`] from it. Without the header, a request may only read.

use std::fmt;

/// Header the auth Worker puts the granted scopes in, space-separated.
pub const SCOPES_HEADER: &str = "x-cof-scopes";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// List, read and search.
    Read,
    /// Create, move, copy, upload, delete to trash (no tools yet).
    Write,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "files:read",
            Self::Write => "files:write",
        }
    }

    fn bit(self) -> u8 {
        match self {
            Self::Read => 1,
            Self::Write => 2,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of scopes. The default is read-only, what a request without the header gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scopes(u8);

impl Default for Scopes {
    fn default() -> Self {
        Self::read_only()
    }
}

impl Scopes {
    pub const fn none() -> Self {
        Self(0)
    }

    pub const fn read_only() -> Self {
        Self(1)
    }

    pub fn all() -> Self {
        Self(Scope::Read.bit() | Scope::Write.bit())
    }

    /// Scopes from a trusted `X-CoF-Scopes` value: missing means read-only; present means
    /// exactly what it lists (unknown names are ignored).
    pub fn from_header(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::read_only();
        };
        let mut scopes = Self::none();
        for name in value.split([' ', ',']).filter(|s| !s.is_empty()) {
            match name {
                "files:read" => scopes.0 |= Scope::Read.bit(),
                "files:write" => scopes.0 |= Scope::Write.bit(),
                _ => {}
            }
        }
        scopes
    }

    pub fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    pub fn with(mut self, scope: Scope) -> Self {
        self.0 |= scope.bit();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parsing() {
        assert_eq!(Scopes::from_header(None), Scopes::read_only());
        assert_eq!(Scopes::from_header(Some("")), Scopes::none());
        assert_eq!(Scopes::from_header(Some("files:read")), Scopes::read_only());
        let both = Scopes::from_header(Some("files:read  files:write"));
        assert!(both.contains(Scope::Read) && both.contains(Scope::Write));
        let write = Scopes::from_header(Some("files:write admin"));
        assert!(!write.contains(Scope::Read) && write.contains(Scope::Write));
        assert_eq!(Scopes::from_header(Some("FILES:READ")), Scopes::none());
    }
}
