//! The embedded web dashboard: `GET /` (the page) and `GET /assets/{name}` (its script and
//! stylesheet). Plain HTML + vanilla JS from `web/`, compiled into the binary with
//! `include_str!`; no build step, nothing fetched from anywhere but this server.
//!
//! # Session
//! `pooltime open` prints `http://127.0.0.1:<port>/?k=<management key>`. A valid `k` is traded for
//! the management session cookie ([`mgmt::session_cookie_header`], the same cookie
//! `POST /api/v1/session` sets) with a `303` to `/`, so the key leaves the address bar and the
//! history at once. Without a valid cookie (or bearer key) the page renders read-only: it shows
//! only what `GET /api/v1/status` already serves unauthenticated, plus a "paste key / run
//! `pooltime open`" hint. An invalid `k` sets nothing and is not echoed. The page learns its mode
//! from the server (`data-mode` on `<body>`), because the cookie is `HttpOnly`.
//!
//! # Headers & caching
//! Every response carries a strict CSP (`default-src 'self'`, no inline script or style, no
//! framing), `Referrer-Policy: no-referrer` and `nosniff`. The page is `no-store` (it depends on
//! the cookie). Assets are served with a strong `ETag` (SHA-256 of the embedded bytes) and
//! `Cache-Control: no-cache`: the browser revalidates on every load and gets a `304` while the
//! binary is unchanged, and an upgraded binary's assets are picked up immediately, without
//! hashed file names or a build step.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION,
    REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use http::{HeaderMap, HeaderValue, StatusCode};
use sha2::{Digest, Sha256};

use super::auth::keys_match;
use super::router::method_not_allowed;
use super::{mgmt, AppState};
use crate::providers::anthropic::anthropic_error;

/// The policy on every dashboard response.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'self'; frame-ancestors 'none'";

const INDEX_HTML: &str = include_str!("../../web/index.html");
/// Replaced in [`INDEX_HTML`] with `manage` or `read-only`.
const MODE_SLOT: &str = "__POOLTIME_MODE__";

struct Asset {
    body: &'static str,
    mime: &'static str,
    etag: String,
}

impl Asset {
    fn new(body: &'static str, mime: &'static str) -> Asset {
        let digest = Sha256::digest(body.as_bytes());
        Asset {
            body,
            mime,
            etag: format!("\"{}\"", &hex::encode(digest)[..32]),
        }
    }
}

static ASSETS: LazyLock<HashMap<&'static str, Asset>> = LazyLock::new(|| {
    HashMap::from([
        (
            "app.js",
            Asset::new(
                include_str!("../../web/app.js"),
                "text/javascript; charset=utf-8",
            ),
        ),
        (
            "style.css",
            Asset::new(
                include_str!("../../web/style.css"),
                "text/css; charset=utf-8",
            ),
        ),
    ])
});

/// `/` and `/assets/{name}`.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/",
            get(index).fallback(|| async { method_not_allowed("GET") }),
        )
        .route(
            "/assets/{name}",
            get(asset).fallback(|| async { method_not_allowed("GET") }),
        )
}

fn harden(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

async fn index(
    State(app): State<Arc<AppState>>,
    query: Result<Query<HashMap<String, String>>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let presented = query.ok().and_then(|Query(mut q)| q.remove("k"));
    let key_ok = !app.mgmt_key.is_empty()
        && presented
            .as_deref()
            .is_some_and(|k| keys_match(k, &app.mgmt_key));
    let mut r = if key_ok {
        let mut r = StatusCode::SEE_OTHER.into_response();
        r.headers_mut()
            .insert(LOCATION, HeaderValue::from_static("/"));
        if let Ok(v) = HeaderValue::from_str(&mgmt::session_cookie_header(&app.mgmt_key)) {
            r.headers_mut().insert(SET_COOKIE, v);
        }
        r
    } else {
        let mode = if mgmt::authorized(&headers, &app.mgmt_key) {
            "manage"
        } else {
            "read-only"
        };
        (
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            INDEX_HTML.replace(MODE_SLOT, mode),
        )
            .into_response()
    };
    r.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    harden(&mut r);
    r
}

async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Response {
    let Some(a) = ASSETS.get(name.as_str()) else {
        return anthropic_error(404, "not_found_error", "pooltime: unknown dashboard asset");
    };
    let fresh = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == a.etag || t.trim() == "*"));
    let mut r = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(CONTENT_TYPE, a.mime)], a.body).into_response()
    };
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&a.etag) {
        h.insert(ETAG, v);
    }
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    harden(&mut r);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_has_mode_slot_and_assets_exist() {
        assert_eq!(INDEX_HTML.matches(MODE_SLOT).count(), 1);
        assert!(ASSETS.contains_key("app.js") && ASSETS.contains_key("style.css"));
        assert!(CSP.starts_with("default-src 'self'"));
    }
}
