//! Account identity: fetch who a token belongs to and check it against what the account was
//! enrolled as. Match on `account.uuid` (stable); the email is for display and `login_hint` and is
//! only a fallback when a uuid is missing on either side (docs/spikes.md S1, spec §16).

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use tokio::time::Instant;

use super::Identity;
use crate::upstream::{join_url, Limits, Upstream};

/// Claude Code's profile call times out after 10 s.
const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);
/// Profile bodies are small; refuse to accept anything absurd.
const MAX_PROFILE_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
struct Profile {
    account: Option<ProfileAccount>,
    organization: Option<ProfileOrg>,
}

#[derive(Deserialize)]
struct ProfileAccount {
    uuid: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct ProfileOrg {
    uuid: Option<String>,
}

/// `GET {api_base}/api/oauth/profile` with `token`, as Claude Code does (Bearer, its UA,
/// `Content-Type: application/json`, `Cache-Control: no-cache`, 10 s). Needs the `user:profile`
/// scope. Errors carry only the HTTP status, never the body or the token.
pub async fn fetch_identity(
    up: &Upstream,
    api_base: &str,
    token: &str,
    ua: &str,
) -> anyhow::Result<Identity> {
    let url: reqwest::Url = join_url(api_base, "/api/oauth/profile")
        .parse()
        .context("profile URL is not valid")?;
    let mut req = reqwest::Request::new(reqwest::Method::GET, url);
    let h = req.headers_mut();
    h.insert(
        http::header::USER_AGENT,
        ua.parse().context("user agent is not a valid header")?,
    );
    let mut auth: http::HeaderValue = format!("Bearer {token}")
        .parse()
        .map_err(|_| anyhow!("token is not a valid header value"))?;
    auth.set_sensitive(true);
    h.insert(http::header::AUTHORIZATION, auth);
    h.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    h.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-cache"),
    );

    let lim = Limits {
        header_wait: PROFILE_TIMEOUT,
        idle: PROFILE_TIMEOUT,
        deadline: Instant::now() + PROFILE_TIMEOUT,
    };
    let resp = up
        .send(req, &lim)
        .await
        .map_err(|k| anyhow!("profile request failed: {k:?}"))?;
    let status = resp.status;
    let body = resp
        .body
        .text()
        .await
        .map_err(|k| anyhow!("profile response failed: {k:?}"))?;
    if !status.is_success() {
        bail!("profile request returned HTTP {}", status.as_u16());
    }
    if body.len() > MAX_PROFILE_BYTES {
        bail!("profile response too large");
    }
    let p: Profile =
        serde_json::from_str(&body).map_err(|_| anyhow!("profile response is not valid JSON"))?;
    let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let (account_uuid, email) = p
        .account
        .map(|a| (nonempty(a.uuid), nonempty(a.email)))
        .unwrap_or_default();
    let id = Identity {
        account_uuid,
        email,
        org_uuid: p.organization.and_then(|o| nonempty(o.uuid)),
    };
    if id.account_uuid.is_none() && id.email.is_none() {
        bail!("profile response carries no account identity");
    }
    Ok(id)
}

/// Outcome of comparing a fresh login's identity with the account's stored baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// Same person.
    Match,
    /// A different account signed in.
    Mismatch,
    /// Nothing to compare against (no baseline, or the two sides share no comparable field):
    /// the caller must ask the user to confirm and bind.
    NoBaseline,
}

/// Compare `got` against the stored `expected` identity.
///
/// When both sides have an `account_uuid` it alone decides (a changed email or org does not matter
/// and an identical email with another uuid is a different account). Only if a uuid is absent on
/// either side does the email (case-insensitive) decide. The org uuid is never compared: orgs
/// change, people do not.
pub fn verify(expected: &Identity, got: &Identity) -> Verify {
    fn present(s: &Option<String>) -> Option<&str> {
        s.as_deref().filter(|s| !s.trim().is_empty())
    }
    if let (Some(e), Some(g)) = (present(&expected.account_uuid), present(&got.account_uuid)) {
        return if e == g {
            Verify::Match
        } else {
            Verify::Mismatch
        };
    }
    if let (Some(e), Some(g)) = (present(&expected.email), present(&got.email)) {
        return if e.trim().eq_ignore_ascii_case(g.trim()) {
            Verify::Match
        } else {
            Verify::Mismatch
        };
    }
    Verify::NoBaseline
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Identity;

    fn id(uuid: Option<&str>, email: Option<&str>, org: Option<&str>) -> Identity {
        Identity {
            account_uuid: uuid.map(Into::into),
            email: email.map(Into::into),
            org_uuid: org.map(Into::into),
        }
    }

    #[test]
    fn uuid_decides_when_both_have_it() {
        let exp = id(Some("u1"), Some("a@x.com"), Some("o1"));
        // Same uuid: match even if email and org changed.
        assert_eq!(
            verify(&exp, &id(Some("u1"), Some("new@x.com"), Some("o2"))),
            Verify::Match
        );
        // Different uuid: mismatch even if the email is identical.
        assert_eq!(
            verify(&exp, &id(Some("u2"), Some("a@x.com"), Some("o1"))),
            Verify::Mismatch
        );
    }

    #[test]
    fn email_fallback_only_when_uuid_absent_on_either_side() {
        // Baseline without uuid (imported account): case-insensitive email decides.
        let exp = id(None, Some("Me@X.com"), None);
        assert_eq!(
            verify(&exp, &id(Some("u1"), Some("me@x.COM"), None)),
            Verify::Match
        );
        assert_eq!(
            verify(&exp, &id(Some("u1"), Some("other@x.com"), None)),
            Verify::Mismatch
        );
        // Fresh identity without uuid.
        let exp = id(Some("u1"), Some("me@x.com"), None);
        assert_eq!(
            verify(&exp, &id(None, Some("me@x.com"), None)),
            Verify::Match
        );
        assert_eq!(
            verify(&exp, &id(None, Some("x@x.com"), None)),
            Verify::Mismatch
        );
    }

    #[test]
    fn no_baseline_or_not_comparable_needs_binding() {
        let got = id(Some("u1"), Some("me@x.com"), Some("o1"));
        assert_eq!(verify(&Identity::default(), &got), Verify::NoBaseline);
        assert_eq!(
            verify(&id(None, None, Some("o1")), &got),
            Verify::NoBaseline,
            "org alone is not an identity"
        );
        // Baseline has only a uuid but the new identity carries no uuid and no email.
        assert_eq!(
            verify(&id(Some("u1"), None, None), &id(None, None, Some("o1"))),
            Verify::NoBaseline
        );
    }
}
