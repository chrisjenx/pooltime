//! `GET /api/v1/analytics/burn?weeks=N` (spec §5): weekly subscription burn, the current week's
//! forecast and the idle accounts, from [`crate::analytics::burn::report`] scoped to the
//! configured accounts. `weeks` defaults to 12 and is capped at 104. Needs the management key.
//!
//! Answer: `{weeks:[{week, start, subs_used, per_account:[{account, consumed, plan?}]}],
//! forecast:{week, subs_used_so_far, elapsed_fraction, projected, accounts}, idle_accounts:[id]}`.
//! `plan` appears only when the account's stored credentials already name one.
//! With `POOLTIME_FIXTURE_STATE` set it answers the fixture's `analytics.burn` document.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::Response;
use time::OffsetDateTime;

use super::mgmt::{bad, blocking, internal, ok};
use super::AppState;
use crate::analytics::burn::{self, DEFAULT_WEEKS, MAX_WEEKS};
use crate::analytics::{local_offset, pool_of};

fn weeks_of(q: &HashMap<String, String>) -> Result<u32, String> {
    match q.get("weeks").map(String::as_str) {
        None | Some("") => Ok(DEFAULT_WEEKS),
        Some(s) => match s.parse::<u32>() {
            Ok(n) if n >= 1 => Ok(n.min(MAX_WEEKS)),
            _ => Err(format!("weeks must be an integer from 1 to {MAX_WEEKS}")),
        },
    }
}

pub(crate) async fn burn(
    State(app): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let weeks = match weeks_of(&q) {
        Ok(w) => w,
        Err(m) => return bad(m),
    };
    if let Some(f) = app.fixture_state.as_ref().map(|f| &f["analytics"]["burn"]) {
        if !f.is_null() {
            return ok(f.clone());
        }
    }
    let now = OffsetDateTime::now_utc();
    let a = app.clone();
    let res = blocking(move || {
        let pool = pool_of(&a.registry);
        burn::report(&a.store, now, local_offset(now), weeks, &pool)
    })
    .await;
    match res {
        Ok(Ok(r)) => match serde_json::to_value(r) {
            Ok(v) => ok(v),
            Err(e) => {
                tracing::error!(error = %e, "serializing the weekly burn failed");
                internal("could not encode the weekly burn")
            }
        },
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "computing the weekly burn failed");
            internal("could not read the usage history")
        }
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weeks_param() {
        let q = |v: &str| HashMap::from([("weeks".to_string(), v.to_string())]);
        assert_eq!(weeks_of(&HashMap::new()), Ok(12));
        assert_eq!(weeks_of(&q("8")), Ok(8));
        assert_eq!(weeks_of(&q("500")), Ok(104));
        assert!(weeks_of(&q("0")).is_err());
        assert!(weeks_of(&q("x")).is_err());
    }
}
