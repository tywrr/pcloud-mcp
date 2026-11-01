//! Settings: a TOML file for everything that isn't secret (config.example.toml at the repo
//! root documents every key), and env vars for the secrets.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chance_of_files_core::{CLAUDE_ORIGINS, Policy, ServerConfig};
use chance_of_files_index::{IndexConfig, RootConfig};
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
    index: IndexSection,
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
    health_every: String,
}

impl Default for RcloneSection {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:5572".into(),
            user: "core".into(),
            remote: "pcloud".into(),
            health_every: "5m".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct IndexSection {
    enabled: bool,
    dir: Option<PathBuf>,
    roots: Vec<RootSection>,
    max_removal: f64,
    hash_min_size: u64,
    hash_in_background: bool,
}

impl Default for IndexSection {
    fn default() -> Self {
        let defaults = IndexConfig::new("");
        Self {
            enabled: true,
            dir: None,
            roots: vec![RootSection::default()],
            max_removal: defaults.max_removal,
            hash_min_size: defaults.hash_min_size,
            hash_in_background: defaults.hash_in_background,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RootSection {
    path: String,
    every: String,
}

impl Default for RootSection {
    fn default() -> Self {
        Self {
            path: "/".into(),
            every: "30m".into(),
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
    /// `None` when turned off, or when there is nowhere to keep it.
    pub index: Option<IndexConfig>,
    /// The index is on (`[index].enabled`, the default) but neither `[index].dir` nor
    /// `[server].state_dir` says where to keep it, so it is off: worth a warning.
    pub index_needs_dir: bool,
    /// How often rclone's health is checked while it works.
    pub health_every: Duration,
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
            .field("index", &self.index)
            .field("health_every", &self.health_every)
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

    let health_every =
        duration(&file.rclone.health_every).map_err(|e| format!("rclone.health_every: {e}"))?;
    if health_every < Duration::from_secs(10) {
        return Err("rclone.health_every must be at least 10s".into());
    }
    let index = index_config(&file.index, file.server.state_dir.as_deref())?;
    let index_needs_dir = file.index.enabled && index.is_none();

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
        index,
        index_needs_dir,
        health_every,
        unknown_keys,
    })
}

/// The index settings, with `dir` defaulting to `<state_dir>/index`. Roots are checked
/// against the policy when the index opens.
fn index_config(
    section: &IndexSection,
    state_dir: Option<&Path>,
) -> Result<Option<IndexConfig>, String> {
    if !section.enabled {
        return Ok(None);
    }
    let Some(dir) = section
        .dir
        .clone()
        .or_else(|| state_dir.map(|d| d.join("index")))
    else {
        return Ok(None);
    };
    if !(0.0..=1.0).contains(&section.max_removal) {
        return Err("index.max_removal must be between 0 and 1 (0.05 is 5%)".into());
    }
    if section.roots.is_empty() {
        return Err("index.roots is empty; list at least one root, such as \"/\"".into());
    }
    let mut config = IndexConfig::new(dir);
    config.max_removal = section.max_removal;
    config.hash_min_size = section.hash_min_size.max(1);
    config.hash_in_background = section.hash_in_background;
    config.roots = section
        .roots
        .iter()
        .map(|root| {
            let path = chance_of_files_core::normalize_path(&root.path)
                .map_err(|e| format!("index.roots: {e}"))?;
            let every =
                duration(&root.every).map_err(|e| format!("index.roots {path}: every: {e}"))?;
            if every < Duration::from_secs(60) {
                return Err(format!("index.roots {path}: every must be at least 1m"));
            }
            Ok(RootConfig { path, every })
        })
        .collect::<Result<_, String>>()?;
    Ok(Some(config))
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
        "d" => Ok(seconds(number.saturating_mul(86_400))),
        _ => Err(format!(
            "{text:?} has an unknown unit; use ms, s, m, h or d"
        )),
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
        assert!(s.index.is_none(), "the example sets no directory");
    }

    #[test]
    fn the_cluster_config_parses() {
        let base = include_str!("../../../deploy/k8s/base/config.toml");
        let s = parse(base, &env).unwrap();
        assert!(s.unknown_keys.is_empty(), "{:?}", s.unknown_keys);
        let index = s.index.unwrap();
        assert_eq!(index.dir, Path::new("/var/lib/chance-of-files/index"));
        assert_eq!(index.roots[0].path, "/");
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
    fn index_settings() {
        let s = parse("", &env).unwrap();
        assert!(s.index.is_none(), "no state_dir and no index.dir: no index");
        assert!(s.index_needs_dir, "worth a warning");
        assert_eq!(s.health_every, Duration::from_secs(300));
        let off = parse("[index]\nenabled = false", &env).unwrap();
        assert!(
            off.index.is_none() && !off.index_needs_dir,
            "off on purpose"
        );

        let s = parse("[server]\nstate_dir = \"/var/lib/cof\"", &env).unwrap();
        let index = s.index.unwrap();
        assert_eq!(index.dir, Path::new("/var/lib/cof/index"));
        assert_eq!(index.roots.len(), 1);
        assert_eq!(index.roots[0].path, "/");
        assert_eq!(index.roots[0].every, Duration::from_secs(1800));

        let s = parse(
            r#"
            [rclone]
            health_every = "1m"
            [index]
            dir = "/data/index"
            max_removal = 0.1
            hash_in_background = false
            [[index.roots]]
            path = "/"
            every = "30m"
            [[index.roots]]
            path = "Backups/Immich/"
            every = "6h"
            "#,
            &env,
        )
        .unwrap();
        assert!(s.unknown_keys.is_empty(), "{:?}", s.unknown_keys);
        assert_eq!(s.health_every, Duration::from_secs(60));
        let index = s.index.unwrap();
        assert_eq!(index.max_removal, 0.1);
        assert!(!index.hash_in_background);
        assert_eq!(index.roots[1].path, "/Backups/Immich");
        assert_eq!(index.roots[1].every, Duration::from_secs(6 * 3600));

        assert!(
            parse("[index]\nenabled = false\ndir = \"/x\"", &env)
                .unwrap()
                .index
                .is_none()
        );
        for (text, needle) in [
            ("[index]\ndir = \"/x\"\nmax_removal = 2.0", "max_removal"),
            ("[index]\ndir = \"/x\"\nroots = []", "index.roots"),
            (
                "[index]\ndir = \"/x\"\n[[index.roots]]\npath = \"/\"\nevery = \"10s\"",
                "at least 1m",
            ),
            ("[rclone]\nhealth_every = \"1s\"", "health_every"),
        ] {
            let err = parse(text, &env).err().unwrap_or_default();
            assert!(err.contains(needle), "{text}: {err}");
        }
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
        assert_eq!(duration("1d"), Ok(Duration::from_secs(86_400)));
        assert!(duration("3w").is_err());
    }
}
