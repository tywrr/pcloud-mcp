//! Signed, expiring links: the token format, client networks for binding, and `Range`
//! parsing. docs/architecture.md ("Links") is the contract the auth Worker follows.
//!
//! A token is `<id>.<exp>.<mac>`: `id` is 128 random bits, `exp` the expiry in Unix seconds,
//! and `mac` the first 128 bits of HMAC-SHA256(`LINK_KEY`, `"<kind>.<id>.<exp>"`), with `kind`
//! `dl` or `ul` from the route, so a download token never opens an upload. `id` and `mac` are
//! unpadded base64url (22 characters each).

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::state::LinkKind;

/// Bytes of the id and of the truncated MAC.
const ID_BYTES: usize = 16;
const MAC_BYTES: usize = 16;
/// The shortest `LINK_KEY` accepted.
pub const MIN_KEY_LEN: usize = 32;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// The HMAC key. Its bytes are the `LINK_KEY` value as given (UTF-8, trailing newline
/// removed), so the Worker can import the same string as a raw key.
#[derive(Clone)]
pub struct LinkKey(Vec<u8>);

impl fmt::Debug for LinkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkKey(..)")
    }
}

/// A token's parts, once its MAC checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub id: String,
    pub expires: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Not a token, or the MAC is wrong: answer as if nothing were there.
    Invalid,
    Expired,
}

impl LinkKey {
    pub fn new(key: &str) -> Result<Self, String> {
        let key = key.trim_end_matches(['\r', '\n']);
        if key.len() < MIN_KEY_LEN {
            return Err(format!(
                "LINK_KEY must be at least {MIN_KEY_LEN} characters (use a long random value)"
            ));
        }
        Ok(Self(key.as_bytes().to_vec()))
    }

    fn mac(&self, kind: LinkKind, id: &str, expires: i64) -> [u8; MAC_BYTES] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC takes any key length");
        mac.update(format!("{}.{id}.{expires}", kind.as_str()).as_bytes());
        let full = mac.finalize().into_bytes();
        let mut out = [0u8; MAC_BYTES];
        out.copy_from_slice(&full[..MAC_BYTES]);
        out
    }

    /// A new random id and its token.
    pub fn mint(&self, kind: LinkKind, expires: i64) -> (String, String) {
        let id = b64().encode(random_bytes::<ID_BYTES>());
        let token = self.token(kind, &id, expires);
        (id, token)
    }

    pub fn token(&self, kind: LinkKind, id: &str, expires: i64) -> String {
        let mac = b64().encode(self.mac(kind, id, expires));
        format!("{id}.{expires}.{mac}")
    }

    /// Checks a token's shape, MAC (in constant time) and expiry.
    pub fn verify(&self, kind: LinkKind, token: &str, now: i64) -> Result<Token, TokenError> {
        let mut parts = token.split('.');
        let (Some(id), Some(exp), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenError::Invalid);
        };
        let id_ok = b64().decode(id).is_ok_and(|b| b.len() == ID_BYTES);
        let expires: i64 = match exp.parse() {
            Ok(n) if id_ok && exp.bytes().all(|b| b.is_ascii_digit()) => n,
            _ => return Err(TokenError::Invalid),
        };
        let Ok(given) = b64().decode(mac) else {
            return Err(TokenError::Invalid);
        };
        if given.len() != MAC_BYTES || !bool::from(given.ct_eq(&self.mac(kind, id, expires))) {
            return Err(TokenError::Invalid);
        }
        if expires <= now {
            return Err(TokenError::Expired);
        }
        Ok(Token {
            id: id.to_owned(),
            expires,
        })
    }
}

/// `N` bytes from the operating system's generator.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("the OS random number generator works");
    buf
}

/// What a link is bound to after its first use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    /// The first client's /24 (IPv4) or /64 (IPv6).
    Net,
    /// The first client's exact address.
    Ip,
    None,
}

impl Bind {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "net" => Some(Self::Net),
            "ip" => Some(Self::Ip),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// The network `ip` belongs to under this rule, as text (`160.79.106.0/24`), or `None`
    /// when links aren't bound. A string that isn't an address can't be bound to anything.
    pub fn network(self, ip: &str) -> Option<Result<String, ()>> {
        if self == Self::None {
            return None;
        }
        let Ok(addr) = ip.trim().parse::<IpAddr>() else {
            return Some(Err(()));
        };
        let addr = match addr {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 => v4,
        };
        Some(Ok(match (self, addr) {
            (Self::Ip, IpAddr::V4(a)) => format!("{a}/32"),
            (Self::Ip, IpAddr::V6(a)) => format!("{a}/128"),
            (_, IpAddr::V4(a)) => {
                let o = a.octets();
                format!("{}.{}.{}.0/24", o[0], o[1], o[2])
            }
            (_, IpAddr::V6(a)) => {
                let s = a.segments();
                let net = std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0);
                format!("{net}/64")
            }
        }))
    }
}

/// Link limits, from `[links]`.
#[derive(Debug, Clone)]
pub struct LinkSettings {
    pub key: LinkKey,
    /// Where links are reached, e.g. `https://chance-of-files.example.workers.dev`. When
    /// unset, the auth Worker's `X-CoF-Public-Url` header says.
    pub public_url: Option<String>,
    pub default_ttl: Duration,
    pub max_ttl: Duration,
    pub bind: Bind,
    /// Largest streamed zip (the files inside, before headers).
    pub max_zip_bytes: u64,
    /// Largest upload part (Cloudflare caps a request body at 100 MB).
    pub part_bytes: u64,
    /// Largest upload, and the most staging space all upload links may hold at once.
    pub staging_max_bytes: u64,
    /// Free space the staging volume keeps after every reservation.
    pub reserve_bytes: u64,
    /// Most files one `create_download_link` without zip makes links for.
    pub max_links_per_call: usize,
}

impl LinkSettings {
    pub fn new(key: LinkKey) -> Self {
        Self {
            key,
            public_url: None,
            default_ttl: Duration::from_secs(15 * 60),
            max_ttl: Duration::from_secs(24 * 3600),
            bind: Bind::Net,
            max_zip_bytes: 10_000_000_000,
            part_bytes: 90 * 1024 * 1024,
            staging_max_bytes: 30_000_000_000,
            reserve_bytes: 20_000_000_000,
            max_links_per_call: 200,
        }
    }
}

/// How a `Range` header applies to a body of `size` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeAnswer {
    /// No usable range: send everything (a header we can't parse is ignored, as HTTP says).
    Full,
    /// Bytes `start..=end`.
    Part { start: u64, end: u64 },
    /// 416: starts at or past the end, or several ranges (rclone can't serve those).
    Unsatisfiable,
}

/// Parses a single `bytes=` range against `size`. rclone answers a range past the end with an
/// empty `206`, so this decides before rclone is asked.
pub fn parse_range(header: Option<&str>, size: u64) -> RangeAnswer {
    let Some(spec) = header.map(str::trim).and_then(|h| h.strip_prefix("bytes=")) else {
        return RangeAnswer::Full;
    };
    if spec.contains(',') {
        return RangeAnswer::Unsatisfiable;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeAnswer::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    let number = |s: &str| -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            s.parse().ok()
        }
    };
    match (first.is_empty(), number(first), number(last)) {
        // bytes=-N: the last N bytes.
        (true, _, Some(n)) => {
            if n == 0 || size == 0 {
                RangeAnswer::Unsatisfiable
            } else {
                RangeAnswer::Part {
                    start: size.saturating_sub(n),
                    end: size - 1,
                }
            }
        }
        (false, Some(start), end) if last.is_empty() || end.is_some() => {
            if start >= size {
                return RangeAnswer::Unsatisfiable;
            }
            match end {
                Some(end) if end < start => RangeAnswer::Full,
                Some(end) => RangeAnswer::Part {
                    start,
                    end: end.min(size - 1),
                },
                None => RangeAnswer::Part {
                    start,
                    end: size - 1,
                },
            }
        }
        _ => RangeAnswer::Full,
    }
}

/// A `Content-Disposition` value that survives any file name: an ASCII fallback plus the
/// RFC 5987 UTF-8 form.
pub fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded =
        percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC).to_string();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> LinkKey {
        LinkKey::new("0123456789abcdef0123456789abcdef-test").unwrap()
    }

    #[test]
    fn tokens_verify_only_for_their_kind_and_until_expiry() {
        let k = key();
        let (id, token) = k.mint(LinkKind::Download, 1000);
        assert_eq!(id.len(), 22);
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2].len(), 22);
        assert_eq!(
            k.verify(LinkKind::Download, &token, 999),
            Ok(Token {
                id: id.clone(),
                expires: 1000
            })
        );
        assert_eq!(
            k.verify(LinkKind::Download, &token, 1000),
            Err(TokenError::Expired)
        );
        assert_eq!(
            k.verify(LinkKind::Upload, &token, 1),
            Err(TokenError::Invalid)
        );
        let other = LinkKey::new("another-key-0123456789abcdef01234567").unwrap();
        assert_eq!(
            other.verify(LinkKind::Download, &token, 1),
            Err(TokenError::Invalid)
        );
        // A later expiry with the old MAC doesn't verify.
        let stretched = format!("{}.{}.{}", parts[0], 99999, parts[2]);
        assert_eq!(
            k.verify(LinkKind::Download, &stretched, 1),
            Err(TokenError::Invalid)
        );
        for junk in ["", "a.b.c", "x", &format!("{token}.x"), "..", &token[1..]] {
            assert_eq!(
                k.verify(LinkKind::Download, junk, 1),
                Err(TokenError::Invalid),
                "{junk}"
            );
        }
        assert!(LinkKey::new("short").is_err());
        assert_eq!(format!("{k:?}"), "LinkKey(..)");
    }

    #[test]
    fn known_vectors_for_the_worker() {
        // docs/architecture.md ("Links") lists these, so the auth Worker can test its check
        // against the same values.
        let k = LinkKey::new("test-link-key-0123456789abcdef0123456789").unwrap();
        let id = "AAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(
            k.token(LinkKind::Download, id, 1_900_000_000),
            "AAAAAAAAAAAAAAAAAAAAAA.1900000000.-m9jVnui9Q5UG9NP0NRQyQ"
        );
        assert_eq!(
            k.token(LinkKind::Upload, id, 1_900_000_000),
            "AAAAAAAAAAAAAAAAAAAAAA.1900000000.OrCNstMVWn-HHbWyszsjSw"
        );
        // And the core accepts them back, each for its own kind only.
        for (kind, mac) in [
            (LinkKind::Download, "-m9jVnui9Q5UG9NP0NRQyQ"),
            (LinkKind::Upload, "OrCNstMVWn-HHbWyszsjSw"),
        ] {
            let token = format!("{id}.1900000000.{mac}");
            assert_eq!(
                k.verify(kind, &token, 1_800_000_000),
                Ok(Token {
                    id: id.into(),
                    expires: 1_900_000_000
                })
            );
        }
        assert_eq!(
            k.verify(
                LinkKind::Upload,
                &format!("{id}.1900000000.-m9jVnui9Q5UG9NP0NRQyQ"),
                1
            ),
            Err(TokenError::Invalid)
        );
    }

    #[test]
    fn networks() {
        assert_eq!(
            Bind::Net.network("160.79.106.131"),
            Some(Ok("160.79.106.0/24".into()))
        );
        assert_eq!(
            Bind::Net.network("2001:db8:1:2:3:4:5:6"),
            Some(Ok("2001:db8:1:2::/64".into()))
        );
        assert_eq!(
            Bind::Net.network("::ffff:10.1.2.3"),
            Some(Ok("10.1.2.0/24".into()))
        );
        assert_eq!(Bind::Ip.network("10.1.2.3"), Some(Ok("10.1.2.3/32".into())));
        assert_eq!(Bind::Net.network("nope"), Some(Err(())));
        assert_eq!(Bind::None.network("10.1.2.3"), None);
    }

    #[test]
    fn ranges() {
        use RangeAnswer::*;
        assert_eq!(parse_range(None, 10), Full);
        assert_eq!(
            parse_range(Some("bytes=0-4"), 10),
            Part { start: 0, end: 4 }
        );
        assert_eq!(parse_range(Some("bytes=5-"), 10), Part { start: 5, end: 9 });
        assert_eq!(
            parse_range(Some("bytes=5-100"), 10),
            Part { start: 5, end: 9 }
        );
        assert_eq!(parse_range(Some("bytes=-3"), 10), Part { start: 7, end: 9 });
        assert_eq!(
            parse_range(Some("bytes=-30"), 10),
            Part { start: 0, end: 9 }
        );
        assert_eq!(parse_range(Some("bytes=10-"), 10), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=20-30"), 10), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-"), 0), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 10), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=0-1,4-5"), 10), Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=5-2"), 10), Full);
        assert_eq!(parse_range(Some("items=0-1"), 10), Full);
        assert_eq!(parse_range(Some("bytes=a-b"), 10), Full);
    }

    #[test]
    fn dispositions() {
        assert_eq!(
            content_disposition("a b.txt"),
            "attachment; filename=\"a b.txt\"; filename*=UTF-8''a%20b%2Etxt"
        );
        assert!(
            content_disposition("Caf\u{e9}\".pdf")
                .starts_with("attachment; filename=\"Caf__.pdf\"")
        );
    }
}
