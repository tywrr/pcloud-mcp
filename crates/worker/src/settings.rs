//! Configuration from Worker vars and secrets. Parsing takes a lookup function rather than
//! `worker::Env`, so it can be tested natively.

use pcloud_mcp_core::ServerConfig;
use pcloud_mcp_webdav::DEFAULT_MAX_LISTING_BYTES;

/// Everything the `/mcp` handler needs besides the backend binding.
#[derive(Debug, Clone)]
pub struct Settings {
    pub backend_user: String,
    pub backend_password: String,
    /// Largest WebDAV listing a list or search parses (`MAX_LISTING_BYTES`).
    pub max_listing_bytes: usize,
    pub server: ServerConfig,
    /// Vars that were set but unusable, for the log. Never contains secret values.
    pub warnings: Vec<String>,
}

/// Reads a var or secret; `None` when unset or blank.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The shared secret the auth front sends as `Authorization: Bearer ...`. `None` when unset
/// or empty, in which case every `/mcp` request must be refused.
pub fn internal_token(lookup: Lookup<'_>) -> Option<String> {
    lookup("INTERNAL_TOKEN")
        .map(|t| strip_newline(&t).to_owned())
        .filter(|t| !t.is_empty())
}

pub fn load(lookup: Lookup<'_>) -> Settings {
    let mut warnings = Vec::new();
    let defaults = ServerConfig::default();
    let mut number = |name: &str, default: u64| -> u64 {
        match lookup(name) {
            None => default,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(n) if n > 0 => n,
                _ => {
                    warnings.push(format!(
                        "{name} must be a positive whole number; using the default {default}"
                    ));
                    default
                }
            },
        }
    };
    let max_read_bytes = number("MAX_READ_BYTES", defaults.max_read_bytes);
    let max_image_bytes = number("MAX_IMAGE_BYTES", defaults.max_image_bytes);
    let search_limit = number("SEARCH_LIMIT", defaults.search_limit as u64);
    let max_listing_bytes = number("MAX_LISTING_BYTES", DEFAULT_MAX_LISTING_BYTES as u64);
    // ALLOWED_ORIGINS adds to Claude's origins rather than replacing them.
    let mut allowed_origins = defaults.allowed_origins.clone();
    for origin in lookup("ALLOWED_ORIGINS").unwrap_or_default().split(',') {
        let origin = origin.trim().trim_end_matches('/');
        if !origin.is_empty()
            && !allowed_origins
                .iter()
                .any(|known| known.eq_ignore_ascii_case(origin))
        {
            allowed_origins.push(origin.to_owned());
        }
    }

    Settings {
        backend_user: lookup("BACKEND_USER").unwrap_or_else(|| "mcp".into()),
        // `echo pw | wrangler secret put` stores the newline too; rclone's password has none.
        backend_password: lookup("BACKEND_PASSWORD")
            .map(|p| strip_newline(&p).to_owned())
            .unwrap_or_default(),
        max_listing_bytes: usize::try_from(max_listing_bytes).unwrap_or(usize::MAX),
        server: ServerConfig {
            max_read_bytes,
            max_image_bytes,
            search_limit: usize::try_from(search_limit).unwrap_or(usize::MAX),
            allowed_origins,
            ..defaults
        },
        warnings,
    }
}

fn strip_newline(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).filter(|v| !v.trim().is_empty()).cloned()
    }

    #[test]
    fn defaults_when_nothing_is_set() {
        let lookup = lookup_from(&[]);
        let s = load(&lookup);
        let d = ServerConfig::default();
        assert_eq!(s.backend_user, "mcp");
        assert_eq!(s.backend_password, "");
        assert_eq!(s.server.max_read_bytes, d.max_read_bytes);
        assert_eq!(s.server.max_image_bytes, d.max_image_bytes);
        assert_eq!(s.server.search_limit, d.search_limit);
        assert_eq!(s.max_listing_bytes, DEFAULT_MAX_LISTING_BYTES);
        assert_eq!(
            s.server.allowed_origins,
            ["https://claude.ai", "https://claude.com"]
        );
        assert!(s.warnings.is_empty());
        assert_eq!(internal_token(&lookup), None);
    }

    #[test]
    fn reads_every_setting() {
        let lookup = lookup_from(&[
            ("INTERNAL_TOKEN", "tok\n"),
            ("BACKEND_USER", "reader"),
            ("BACKEND_PASSWORD", "pw with space \r\n"),
            ("MAX_READ_BYTES", " 5000 "),
            ("MAX_IMAGE_BYTES", "1048576"),
            ("SEARCH_LIMIT", "7"),
            ("MAX_LISTING_BYTES", "2000000"),
            (
                "ALLOWED_ORIGINS",
                "http://localhost:6274/, HTTPS://CLAUDE.AI ,,",
            ),
        ]);
        let s = load(&lookup);
        assert_eq!(internal_token(&lookup).as_deref(), Some("tok"));
        assert_eq!(s.backend_user, "reader");
        assert_eq!(s.backend_password, "pw with space ");
        assert_eq!(s.server.max_read_bytes, 5000);
        assert_eq!(s.server.max_image_bytes, 1_048_576);
        assert_eq!(s.server.search_limit, 7);
        assert_eq!(s.max_listing_bytes, 2_000_000);
        // Added to Claude's origins, without a duplicate.
        assert_eq!(
            s.server.allowed_origins,
            [
                "https://claude.ai",
                "https://claude.com",
                "http://localhost:6274"
            ]
        );
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn bad_numbers_fall_back_with_a_warning() {
        let lookup = lookup_from(&[
            ("MAX_READ_BYTES", "100kB"),
            ("SEARCH_LIMIT", "0"),
            ("MAX_LISTING_BYTES", "-1"),
        ]);
        let s = load(&lookup);
        assert_eq!(
            s.server.max_read_bytes,
            ServerConfig::default().max_read_bytes
        );
        assert_eq!(s.server.search_limit, ServerConfig::default().search_limit);
        assert_eq!(s.max_listing_bytes, DEFAULT_MAX_LISTING_BYTES);
        assert_eq!(s.warnings.len(), 3);
    }

    #[test]
    fn blank_token_counts_as_unset() {
        assert_eq!(
            internal_token(&lookup_from(&[("INTERNAL_TOKEN", "\n")])),
            None
        );
    }
}
