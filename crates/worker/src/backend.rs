//! How the Worker reaches rclone: a Workers VPC Service binding through the tunnel in
//! production, or a plain fetch to `BACKEND_URL` in local development. `BACKEND_URL` wins when
//! both are there: it only ever lives in `.dev.vars`, so the binding can stay in
//! wrangler.toml during local work.

use pcloud_mcp_webdav::{Transport, TransportRequest, TransportResponse};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::{Env, Fetch, Fetcher, js_sys, web_sys};

/// The VPC Service decides where requests go; the URL only supplies the path and the Host
/// header (rclone ignores Host).
const BINDING_ORIGIN: &str = "http://localhost";

pub enum Backend {
    /// Workers VPC Service binding `PCLOUD_BACKEND`.
    Binding(Fetcher),
    /// Global `fetch` to `BACKEND_URL`.
    Url(BackendUrl),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendUrl {
    /// Scheme, host, port and path, without a trailing slash.
    pub base: String,
    /// Path part of `base` (empty for the root); hrefs from rclone start with it.
    pub path: String,
}

impl Backend {
    pub fn from_env(env: &Env) -> Result<Self, String> {
        let raw = env
            .var("BACKEND_URL")
            .map(|v| v.to_string())
            .unwrap_or_default();
        if !raw.trim().is_empty() {
            return parse_backend_url(&raw).map(Self::Url);
        }
        vpc_binding(env).map(Self::Binding).ok_or_else(|| {
            "no backend: bind the PCLOUD_BACKEND VPC service, or set BACKEND_URL for local development"
                .into()
        })
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Self::Binding(_) => "VPC binding PCLOUD_BACKEND",
            Self::Url(_) => "BACKEND_URL",
        }
    }
}

/// The binding as a Fetcher. `Env::service` checks the JS constructor name is `Fetcher`;
/// should a VPC binding ever report another name, anything with a `fetch` method will do.
fn vpc_binding(env: &Env) -> Option<Fetcher> {
    if let Ok(fetcher) = env.service("PCLOUD_BACKEND") {
        return Some(fetcher);
    }
    let value = js_sys::Reflect::get(env.as_ref(), &JsValue::from_str("PCLOUD_BACKEND")).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    let fetch = js_sys::Reflect::get(&value, &JsValue::from_str("fetch")).ok()?;
    fetch
        .is_function()
        .then(|| value.unchecked_into::<Fetcher>())
}

/// Validates `BACKEND_URL` and splits off its path. Errors never echo the value, in case
/// someone put a password in it.
pub fn parse_backend_url(raw: &str) -> Result<BackendUrl, String> {
    let url = worker::Url::parse(raw.trim())
        .map_err(|e| format!("BACKEND_URL is not a valid URL ({e})"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("BACKEND_URL must start with http:// or https://".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "BACKEND_URL must not contain credentials; set BACKEND_USER and BACKEND_PASSWORD instead"
                .into(),
        );
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("BACKEND_URL must not have a query or fragment".into());
    }
    let path = url.path().trim_end_matches('/').to_owned();
    let origin = url.origin().ascii_serialization();
    Ok(BackendUrl {
        base: format!("{origin}{path}"),
        path,
    })
}

impl Transport for Backend {
    async fn send(&self, request: TransportRequest) -> Result<TransportResponse, String> {
        let url = match self {
            Self::Binding(_) => format!("{BINDING_ORIGIN}{}", request.path_and_query),
            Self::Url(u) => format!("{}{}", u.base, request.path_and_query),
        };
        let js_request = build_request(&url, &request).map_err(js_error)?;
        let result = match self {
            Self::Binding(fetcher) => fetcher.fetch_request(js_request).await,
            Self::Url(_) => Fetch::Request(js_request).send().await,
        };
        let mut response = result.map_err(|e| {
            let message = e.to_string();
            worker::console_warn!(
                "backend {} {} failed: {message}",
                request.method,
                request.path_and_query
            );
            message
        })?;
        let status = response.status_code();
        let headers = response.headers().entries().collect();
        let body = response.bytes().await.map_err(|e| e.to_string())?;
        Ok(TransportResponse {
            status,
            headers,
            body,
        })
    }

    fn base_path(&self) -> &str {
        match self {
            Self::Binding(_) => "",
            Self::Url(u) => &u.path,
        }
    }
}

/// Builds the request through web_sys because `worker::Method` has no PROPFIND (unknown
/// methods silently become GET).
fn build_request(url: &str, request: &TransportRequest) -> Result<worker::Request, JsValue> {
    let headers = web_sys::Headers::new()?;
    for (name, value) in &request.headers {
        headers.append(name, value)?;
    }
    let init = web_sys::RequestInit::new();
    init.set_method(&request.method);
    init.set_headers(&headers);
    if !request.body.is_empty() {
        init.set_body(&js_sys::Uint8Array::from(request.body.as_slice()));
    }
    let js_request = web_sys::Request::new_with_str_and_init(url, &init)?;
    Ok(worker::Request::from(js_request))
}

fn js_error(value: JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| "JavaScript error".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_urls() {
        assert_eq!(
            parse_backend_url("http://127.0.0.1:8080").unwrap(),
            BackendUrl {
                base: "http://127.0.0.1:8080".into(),
                path: String::new()
            }
        );
        assert_eq!(
            parse_backend_url(" http://127.0.0.1:8080/ ").unwrap().base,
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            parse_backend_url("https://files.example.com/dav/").unwrap(),
            BackendUrl {
                base: "https://files.example.com/dav".into(),
                path: "/dav".into()
            }
        );
        assert!(parse_backend_url("127.0.0.1:8080").is_err());
        assert!(parse_backend_url("ftp://host/").is_err());
        assert!(parse_backend_url("http://host/?x=1").is_err());
        let err = parse_backend_url("http://mcp:hunter2@host:8080").unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
    }
}
