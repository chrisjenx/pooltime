//! Usage analytics (spec §5): weekly "subscriptions burned" from the 7-day window history.
//!
//! - [`sampler`] records each account's 7-day utilization into `usage_samples` as it changes.
//! - [`burn`] totals the history per local ISO week ([`burn::report`], shared by the management
//!   API, `pooltime status` and MCP).

pub mod burn;
pub mod sampler;

use time::{OffsetDateTime, UtcOffset};

use crate::accounts::Registry;

/// The configured accounts as `(id, plan)` for [`burn::report`]. The plan is the one already
/// stored with the account's credentials (from its OAuth profile); nothing is fetched. Blocking
/// (reads account files).
pub fn pool_of(registry: &Registry) -> Vec<(String, Option<String>)> {
    registry
        .list()
        .into_iter()
        .map(|m| {
            let plan = registry
                .read_creds(&m.id)
                .ok()
                .flatten()
                .and_then(|c| c.plan);
            (m.id.to_string(), plan)
        })
        .collect()
}

/// The machine's local UTC offset at `at` (weeks start Monday 00:00 local time), or UTC when it
/// cannot be determined.
pub fn local_offset(at: OffsetDateTime) -> UtcOffset {
    let t = at.unix_timestamp() as libc::time_t;
    // SAFETY: `tm` is plain old data, zero is a valid value for every field; `localtime_r` only
    // writes into the buffer we pass and returns NULL on failure.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::localtime_r(&t, &mut tm) };
    if res.is_null() {
        return UtcOffset::UTC;
    }
    i32::try_from(tm.tm_gmtoff)
        .ok()
        .and_then(|s| UtcOffset::from_whole_seconds(s).ok())
        .unwrap_or(UtcOffset::UTC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_offset_is_a_sane_offset() {
        let off = local_offset(OffsetDateTime::now_utc());
        assert!(off.whole_hours().abs() <= 14, "{off}");
    }
}
