//! Settings: a TOML file for everything that isn't secret (config.example.toml at the repo
//! root documents every key), and env vars for the secrets.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chance_of_files_core::{
    Bind, CLAUDE_ORIGINS, LinkKey, LinkSettings, Policy, ServerConfig, WRITE_TOOLS,
};
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
    writes: WritesSection,
    links: LinksSection,
    disk: DiskSection,
}

/// A byte count: a number, or text with a unit (`"10GB"`, `"90 MiB"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Size {
    Bytes(u64),
    Text(String),
}

impl Size {
    fn bytes(&self, key: &str) -> Result<u64, String> {
        match self {
            Self::Bytes(n) => Ok(*n),
            Self::Text(text) => size(text).map_err(|e| format!("{key}: {e}")),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct WritesSection {
    tools: Vec<String>,
    max_inline_bytes: Size,
}

impl Default for WritesSection {
    fn default() -> Self {
        Self {
            tools: WRITE_TOOLS.iter().map(|&t| t.to_owned()).collect(),
            max_inline_bytes: Size::Bytes(5_000_000),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct LinksSection {
    enabled: bool,
    public_url: String,
    default_ttl: String,
    max_ttl: String,
    bind: String,
    max_zip_bytes: Size,
    staging: Option<PathBuf>,
    staging_max_bytes: Size,
    part_bytes: Size,
}

impl Default for LinksSection {
    fn default() -> Self {
        Self {
            enabled: true,
            public_url: String::new(),
            default_ttl: "15m".into(),
            max_ttl: "24h".into(),
            bind: "net".into(),
            max_zip_bytes: Size::Text("10GB".into()),
            staging: None,
            staging_max_bytes: Size::Text("30GB".into()),
            part_bytes: Size::Text("90MiB".into()),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct DiskSection {
    reserve: Size,
}

impl Default for DiskSection {
    fn default() -> Self {
        Self {
            reserve: Size::Text("20GB".into()),
        }
    }
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
            // A 5 MB write_file in base64 is 6.7 MB of JSON.
            max_body_bytes: 8 * 1024 * 1024,
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
    /// Where upload parts and small writes are staged: `[links] staging`, else
    /// `<state_dir>/staging`. rclone must see it at the same path.
    pub staging: Option<PathBuf>,
    /// `None` when off; [`Settings::links_off`] says why.
    pub links: Option<LinkSettings>,
    pub links_off: Option<String>,
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
            .field("staging", &self.staging)
            .field("links", &self.links)
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

    let mut write_tools = Vec::new();
    for tool in &file.writes.tools {
        if !WRITE_TOOLS.contains(&tool.as_str()) {
            return Err(format!(
                "writes.tools: unknown tool {tool:?}; the write tools are {}",
                WRITE_TOOLS.join(", ")
            ));
        }
        if !write_tools.contains(tool) {
            write_tools.push(tool.clone());
        }
    }
    let max_inline_bytes = file
        .writes
        .max_inline_bytes
        .bytes("writes.max_inline_bytes")?;
    // base64 grows a file by a third, plus the JSON around it.
    let inline_body = max_inline_bytes.saturating_mul(4) / 3 + 64 * 1024;
    if write_tools.iter().any(|t| t == "write_file") && inline_body > limits.max_body_bytes as u64 {
        return Err(format!(
            "writes.max_inline_bytes ({max_inline_bytes}) doesn't fit in limits.max_body_bytes \
             ({}) as base64; raise max_body_bytes to at least {inline_body}",
            limits.max_body_bytes
        ));
    }
    let state_dir = file.server.state_dir.clone();
    let staging = file
        .links
        .staging
        .clone()
        .or_else(|| state_dir.as_ref().map(|d| d.join("staging")));
    if let Some(dir) = &staging {
        let text = dir.to_string_lossy();
        if !dir.is_absolute() || text.contains(':') || text.contains(',') {
            return Err(format!(
                "links.staging {text:?} must be an absolute path without ':' or ','"
            ));
        }
    }
    let (links, links_off) = link_settings(&file.links, &file.disk, state_dir.is_some(), env)?;

    let core = ServerConfig {
        max_read_bytes: limits.max_read_bytes,
        max_image_bytes: limits.max_image_bytes,
        search_limit: limits.search_limit,
        guide_path,
        allowed_origins,
        deadline,
        write_tools,
        max_inline_bytes,
        ..ServerConfig::default()
    };
    let rclone = RcloneConfig {
        url: file.rclone.url,
        user: file.rclone.user,
        password: rc_pass,
        remote: file.rclone.remote,
        max_listing_bytes: limits.max_listing_bytes,
        staging: staging.clone(),
    };
    Ok(Settings {
        listen,
        core,
        policy,
        rclone,
        internal_token,
        max_body_bytes: limits.max_body_bytes,
        state_dir,
        index,
        index_needs_dir,
        health_every,
        staging,
        links,
        links_off,
        unknown_keys,
    })
}

/// The link settings, or why links are off. A missing `LINK_KEY` turns them off (with a
/// warning at startup) rather than stopping the server.
fn link_settings(
    section: &LinksSection,
    disk: &DiskSection,
    has_state: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Option<LinkSettings>, Option<String>), String> {
    if !section.enabled {
        return Ok((None, Some("turned off ([links] enabled = false)".into())));
    }
    let ttl = |key: &str, text: &str| duration(text).map_err(|e| format!("links.{key}: {e}"));
    let default_ttl = ttl("default_ttl", &section.default_ttl)?;
    let max_ttl = ttl("max_ttl", &section.max_ttl)?;
    if default_ttl < Duration::from_secs(60) || default_ttl > max_ttl {
        return Err("links.default_ttl must be at least 1m and at most links.max_ttl".into());
    }
    if max_ttl > Duration::from_secs(7 * 86_400) {
        return Err("links.max_ttl can be at most 7d".into());
    }
    let bind = Bind::parse(&section.bind)
        .ok_or_else(|| format!("links.bind {:?} must be net, ip or none", section.bind))?;
    let part_bytes = section.part_bytes.bytes("links.part_bytes")?;
    // Cloudflare refuses request bodies over 100 MB (decimal) on the free plan.
    if !(1024 * 1024..=99_000_000).contains(&part_bytes) {
        return Err("links.part_bytes must be between 1 MiB and 99 MB".into());
    }
    let public_url = section.public_url.trim().trim_end_matches('/');
    if !public_url.is_empty() && !public_url.starts_with("https://") {
        return Err("links.public_url must start with https://".into());
    }
    let key = env("LINK_KEY")
        .map(|v| v.trim_end_matches(['\r', '\n']).to_owned())
        .filter(|v| !v.trim().is_empty());
    let Some(key) = key else {
        return Ok((
            None,
            Some("off: the LINK_KEY environment variable is not set".into()),
        ));
    };
    if !has_state {
        return Ok((None, Some("off: links need [server] state_dir".into())));
    }
    let mut links = LinkSettings::new(LinkKey::new(&key)?);
    links.public_url = (!public_url.is_empty()).then(|| public_url.to_owned());
    links.default_ttl = default_ttl;
    links.max_ttl = max_ttl;
    links.bind = bind;
    links.max_zip_bytes = section.max_zip_bytes.bytes("links.max_zip_bytes")?;
    links.part_bytes = part_bytes;
    links.staging_max_bytes = section.staging_max_bytes.bytes("links.staging_max_bytes")?;
    links.reserve_bytes = disk.reserve.bytes("disk.reserve")?;
    Ok((Some(links), None))
}

/// `10GB`, `90 MiB`, `500k`, `1234`: KB, MB, GB and TB are powers of 1000; KiB, MiB, GiB,
/// TiB and K, M, G, T powers of 1024.
pub fn size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '_')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .replace('_', "")
        .parse()
        .map_err(|_| format!("{text:?} is not a size such as \"10GB\" or \"90MiB\""))?;
    let scale: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1000,
        "mb" => 1000_u64.pow(2),
        "gb" => 1000_u64.pow(3),
        "tb" => 1000_u64.pow(4),
        "k" | "kib" => 1024,
        "m" | "mib" => 1024_u64.pow(2),
        "g" | "gib" => 1024_u64.pow(3),
        "t" | "tib" => 1024_u64.pow(4),
        _ => {
            return Err(format!(
                "{text:?} has an unknown unit; use B, KB, MB, GB, MiB, GiB..."
            ));
        }
    };
    number
        .checked_mul(scale)
        .ok_or_else(|| format!("{text:?} is too big"))
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
    fn writes_and_links() {
        let s = parse("", &env).unwrap();
        assert_eq!(s.core.write_tools.len(), 5);
        assert_eq!(s.core.max_inline_bytes, 5_000_000);
        assert!(s.links.is_none(), "no LINK_KEY");
        assert!(s.links_off.unwrap().contains("LINK_KEY"));
        let with_key = |name: &str| match name {
            "LINK_KEY" => Some("k".repeat(40) + "\n"),
            other => env(other),
        };
        let s = parse("", &with_key).unwrap();
        assert!(s.links.is_none(), "no state_dir");
        let s = parse(
            r#"
            [server]
            state_dir = "/var/lib/cof"
            [writes]
            tools = ["create_folder", "move"]
            max_inline_bytes = "1MiB"
            [links]
            public_url = "https://cof.example.workers.dev/"
            default_ttl = "10m"
            bind = "ip"
            max_zip_bytes = "2GB"
            part_bytes = "50MiB"
            [disk]
            reserve = 1000
            "#,
            &with_key,
        )
        .unwrap();
        assert!(s.unknown_keys.is_empty(), "{:?}", s.unknown_keys);
        assert_eq!(s.core.write_tools, ["create_folder", "move"]);
        assert_eq!(s.core.max_inline_bytes, 1024 * 1024);
        assert_eq!(
            s.staging.as_deref(),
            Some(Path::new("/var/lib/cof/staging"))
        );
        assert_eq!(s.rclone.staging, s.staging);
        let links = s.links.unwrap();
        assert_eq!(
            links.public_url.as_deref(),
            Some("https://cof.example.workers.dev")
        );
        assert_eq!(links.default_ttl, Duration::from_secs(600));
        assert_eq!(links.bind, Bind::Ip);
        assert_eq!(links.max_zip_bytes, 2_000_000_000);
        assert_eq!(links.part_bytes, 50 * 1024 * 1024);
        assert_eq!(links.reserve_bytes, 1000);
        for (text, needle) in [
            ("[writes]\ntools = [\"rm\"]", "writes.tools"),
            ("[links]\nbind = \"x\"", "links.bind"),
            ("[links]\npart_bytes = \"200MB\"", "part_bytes"),
            ("[links]\ndefault_ttl = \"2d\"", "default_ttl"),
            ("[links]\nstaging = \"rel/dir\"", "links.staging"),
            ("[links]\npublic_url = \"http://x\"", "public_url"),
            ("[limits]\nmax_body_bytes = 1048576", "max_inline_bytes"),
        ] {
            let err = parse(text, &with_key).err().unwrap_or_default();
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(size("10GB"), Ok(10_000_000_000));
        assert_eq!(size("90MiB"), Ok(90 * 1024 * 1024));
        assert_eq!(size("90 M"), Ok(90 * 1024 * 1024));
        assert_eq!(size("5_000_000"), Ok(5_000_000));
        assert!(size("1.5GB").is_err());
        assert!(size("10 parsecs").is_err());
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
