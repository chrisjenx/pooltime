//! `pooltime open`: print the dashboard URL (`http://127.0.0.1:<port>/?k=<management key>`) and
//! open it in the browser. The key in the URL is the user's own local secret: it goes to stdout
//! only (the dashboard trades it for a session cookie), never to a log.

use super::client;
use crate::config::Paths;

pub async fn run(paths: &Paths) -> anyhow::Result<i32> {
    let c = client::require(paths).await?;
    let url = format!("{}/?k={}", c.base(), c.mgmt_key()?);
    println!("{url}");
    client::open_browser(&url);
    Ok(0)
}
