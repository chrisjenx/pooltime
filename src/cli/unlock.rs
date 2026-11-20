//! `pooltime unlock`: print a one-time code that turns an open read-only dashboard tab into a
//! managing one (type it into the tab's Unlock field), instead of opening a new tab like
//! `pooltime open`. The code goes to stdout only, never to a log; see [`crate::server::unlock`].

use serde_json::json;

use super::client;
use crate::config::Paths;

pub async fn run(paths: &Paths) -> anyhow::Result<i32> {
    let c = client::require(paths).await?;
    let v = c.post("/api/v1/session/code", json!({})).await?;
    let code = v["code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the daemon returned no unlock code"))?;
    let mins = v["expires_in_secs"].as_u64().unwrap_or(300).div_ceil(60);
    println!("{code}");
    eprintln!(
        "type it into the dashboard's Unlock field ({}): works once, for {mins} minutes",
        c.base()
    );
    Ok(0)
}
