//! Settings: a TOML file for everything that isn't secret (config.example.toml at the repo
//! root documents every key), and env vars for the secrets.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chance_of_files_core::{CLAUDE_ORIGINS, Policy, ServerConfig};
use chance_of_files_rclone::RcloneConfig;
use serde::Deserialize;

/// Where the config file is when neither `--config` nor `CHANCE_OF_FILES_CONFIG` says.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/chance-of-files/config.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct File {
    server: ServerSection,
    rclone: RcloneSection,
    policy: PolicySection,
    guide: GuideSection,
    limits: LimitsSection,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ServerSection {
    listen: String,
    deadline: String,
    allowed_origins: Vec<String>,
    state_dir: Option<PathBuf>,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            deadline: "180s".into(),
            allowed_origins: Vec::new(),
            state_dir: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RcloneSection {
    url: String,
    user: String,
    remote: String,
}

impl Default for RcloneSection {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:5572".into(),
            user: "core".into(),
            remote: "pcloud".into(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PolicySection {
    hidden: Vec<String>,
    protected: Vec<String>,
    search_skip: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GuideSection {
    path: String,
}

impl Default for GuideSection {
    fn default() -> Self {
        Self {
            path: chance_of_files_core::DEFAULT_GUIDE_PATH.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct LimitsSection {
    max_read_bytes: u64,
    max_image_bytes: u64,
    max_listing_bytes: u64,
    search_limit: usize,
    max_body_bytes: usize,
}

impl Default for LimitsSection {
    fn default() -> Self {
        let core = ServerConfig::default();
        Self {
            max_read_bytes: core.max_read_bytes,
            max_image_bytes: core.max_image_bytes,
            max_listing_bytes: 64_000_000,
            search_limit: core.search_limit,
            max_body_bytes: 1024 * 1024,
        }
    }
}

/// Everything the server needs, checked.
pub struct Settings {
    pub listen: SocketAddr,
    pub core: ServerConfig,
    pub policy: Policy,
    pub rclone: RcloneConfig,
    pub internal_token: String,
    pub max_body_bytes: usize,
    pub state_dir: Option<PathBuf>,
    /// Keys in the file this version doesn't know (typos, or newer settings), for the log.
    pub unknown_keys: Vec<String>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the internal token; RcloneConfig leaves out the rc password itself.
        f.debug_struct("Settings")
            .field("listen", &self.listen)
            .field("core", &self.core)
            .field("policy", &self.policy)
            .field("rclone", &self.rclone)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("state_dir", &self.state_dir)
            .finish_non_exhaustive()
    }
}

/// Reads the file at `path` and the secrets from `env`.
pub fn load(path: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Settings, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("can't read config file {}: {e}", path.display()))?;
    parse(&text, env).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn parse(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Settings, String> {
    let mut unknown_keys = Vec::new();
    let deserializer = toml::Deserializer::parse(text).map_err(|e| e.to_string())?;
    let file: File = serde_ignored::deserialize(deserializer, |key| {
        unknown_keys.push(key.to_string());
    })
    .map_err(|e| e.to_string())?;

    let secret = |name: &str| -> Result<String, String> {
        // `echo token | kubectl create secret` and friends keep the newline.
        env(name)
            .map(|v| v.trim_end_matches(['\r', '\n']).to_owned())
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| format!("the {name} environment variable is not set"))
    };
    let internal_token = secret("INTERNAL_TOKEN")?;
    let rc_pass = secret("RC_PASS")?;

    let listen: SocketAddr = file.server.listen.parse().map_err(|_| {
        format!(
            "server.listen {:?} is not an address:port",
            file.server.listen
        )
    })?;
    let deadline = duration(&file.server.deadline).map_err(|e| format!("server.deadline: {e}"))?;
    if deadline < Duration::from_secs(1) {
        return Err("server.deadline must be at least 1s".into());
    }

    // Extra origins add to Claude's, which stay allowed.
    let mut allowed_origins: Vec<String> = CLAUDE_ORIGINS.iter().map(|&o| o.to_owned()).collect();
    for origin in &file.server.allowed_origins {
        let origin = origin.trim().trim_end_matches('/');
        if !origin.is_empty()
            && !allowed_origins
                .iter()
                .any(|known| known.eq_ignore_ascii_case(origin))
        {
            allowed_origins.push(origin.to_owned());
        }
    }

    let policy = Policy::new(
        &file.policy.hidden,
        &file.policy.protected,
        &file.policy.search_skip,
    )
    .map_err(|e| format!("policy: {e}"))?;

    let guide_path = match file.guide.path.trim() {
        "" => None,
        path => Some(
            chance_of_files_core::normalize_path(path).map_err(|e| format!("guide.path: {e}"))?,
        ),
    };

    let limits = &file.limits;
    for (name, value) in [
        ("max_read_bytes", limits.max_read_bytes),
        ("max_image_bytes", limits.max_image_bytes),
        ("max_listing_bytes", limits.max_listing_bytes),
        ("search_limit", limits.search_limit as u64),
        ("max_body_bytes", limits.max_body_bytes as u64),
    ] {
        if value == 0 {
            return Err(format!("limits.{name} must be more than 0"));
        }
    }

    let core = ServerConfig {
        max_read_bytes: limits.max_read_bytes,
        max_image_bytes: limits.max_image_bytes,
        search_limit: limits.search_limit,
        guide_path,
        allowed_origins,
        deadline,
        ..ServerConfig::default()
    };
    let rclone = RcloneConfig {
        url: file.rclone.url,
        user: file.rclone.user,
        password: rc_pass,
        remote: file.rclone.remote,
        max_listing_bytes: limits.max_listing_bytes,
    };
    Ok(Settings {
        listen,
        core,
        policy,
        rclone,
        internal_token,
        max_body_bytes: limits.max_body_bytes,
        state_dir: file.server.state_dir,
        unknown_keys,
    })
}

/// `180s`, `5m`, `1h`, `1500ms`; a bare number is seconds.
pub fn duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as \"180s\" or \"5m\""))?;
    let seconds = |n: u64| Duration::from_secs(n);
    match unit.trim() {
        "ms" => Ok(Duration::from_millis(number)),
        "" | "s" => Ok(seconds(number)),
        "m" => Ok(seconds(number.saturating_mul(60))),
        "h" => Ok(seconds(number.saturating_mul(3600))),
        _ => Err(format!("{text:?} has an unknown unit; use ms, s, m or h")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "INTERNAL_TOKEN" => Some("internal-token-of-some-length\n".into()),
            "RC_PASS" => Some("rc-pass".into()),
            _ => None,
        }
    }

    #[test]
    fn defaults_from_an_empty_file() {
        let s = parse("", &env).unwrap();
        assert_eq!(s.listen.to_string(), "127.0.0.1:8080");
        assert_eq!(s.core.deadline, Duration::from_secs(180));
        assert_eq!(s.core.guide_path.as_deref(), Some("/AGENTS.md"));
        assert_eq!(s.rclone.url, "http://127.0.0.1:5572");
        assert_eq!(s.rclone.remote, "pcloud");
        assert_eq!(s.rclone.user, "core");
        assert_eq!(s.internal_token, "internal-token-of-some-length");
        assert!(s.policy.hidden().is_empty());
        assert!(s.unknown_keys.is_empty());
    }

    #[test]
    fn the_example_file_parses() {
        let example = include_str!("../../../config.example.toml");
        let s = parse(example, &env).unwrap();
        assert!(s.unknown_keys.is_empty(), "{:?}", s.unknown_keys);
        assert_eq!(s.core.deadline, Duration::from_secs(180));
    }

    #[test]
    fn every_section() {
        let s = parse(
            r#"
            [server]
            listen = "0.0.0.0:9000"
            deadline = "2m"
            allowed_origins = ["https://example.org/", "https://claude.ai"]
            state_dir = "/var/lib/chance-of-files/state"
            [rclone]
            url = "http://localhost:15572"
            user = "u"
            remote = "drive:"
            [policy]
            hidden = ["/Crypto Folder"]
            protected = ["/Backups/Immich"]
            search_skip = ["/Backups/Immich"]
            [guide]
            path = ""
            [limits]
            max_read_bytes = 1000
            max_image_bytes = 2000
            max_listing_bytes = 3000
            search_limit = 7
            typo_key = 1
            "#,
            &env,
        )
        .unwrap();
        assert_eq!(s.listen.port(), 9000);
        assert_eq!(s.core.deadline, Duration::from_secs(120));
        assert_eq!(s.core.allowed_origins.len(), 3);
        assert!(s.core.guide_path.is_none());
        assert_eq!(s.core.max_read_bytes, 1000);
        assert_eq!(s.core.search_limit, 7);
        assert_eq!(s.rclone.max_listing_bytes, 3000);
        assert_eq!(s.rclone.remote, "drive:");
        assert!(s.policy.check("/crypto folder/x").is_err());
        assert_eq!(s.unknown_keys, ["limits.typo_key"]);
    }

    #[test]
    fn secrets_are_required() {
        let none = |_: &str| None;
        let err = parse("", &none).unwrap_err();
        assert!(err.contains("INTERNAL_TOKEN"), "{err}");
        let blank = |name: &str| (name == "INTERNAL_TOKEN").then(|| "x".to_owned());
        let err = parse("", &blank).unwrap_err();
        assert!(err.contains("RC_PASS"), "{err}");
    }

    #[test]
    fn bad_values_are_refused() {
        for (text, needle) in [
            ("[server]\nlisten = \"nope\"", "server.listen"),
            ("[server]\ndeadline = \"soon\"", "server.deadline"),
            ("[server]\ndeadline = \"0s\"", "at least 1s"),
            ("[policy]\nhidden = [\"/\"]", "policy"),
            ("[policy]\nhidden = [\"/a/../b\"]", "policy"),
            ("[limits]\nmax_read_bytes = 0", "limits.max_read_bytes"),
            ("[server]\nlisten = 5", "invalid type"),
        ] {
            let err = parse(text, &env).unwrap_err();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(duration("180s"), Ok(Duration::from_secs(180)));
        assert_eq!(duration("90"), Ok(Duration::from_secs(90)));
        assert_eq!(duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(duration("1h"), Ok(Duration::from_secs(3600)));
        assert_eq!(duration("1500ms"), Ok(Duration::from_millis(1500)));
        assert!(duration("1.5s").is_err());
        assert!(duration("s").is_err());
        assert!(duration("3d").is_err());
    }
}
