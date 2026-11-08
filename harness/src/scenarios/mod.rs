//! Scenario definitions. A scenario is data: the accounts to seed, how the fake upstreams answer,
//! and the client steps to send. The runner plays it against bro and pooltime (each with its own
//! identically scripted fakes) and compares the observations.

mod anthropic;
mod codex;
mod routing;

use std::time::Duration;

use serde_json::{json, Value};

use crate::fake::{Recorded, Upstreams};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
}

/// One synthetic account (fake tokens derived from `name`).
#[derive(Debug, Clone)]
pub struct AccountSpec {
    pub name: &'static str,
    pub provider: Provider,
    /// Lower is preferred (both daemons default to 100).
    pub priority: u32,
    /// Seed an access token that has already expired (forces a refresh on first use).
    pub expired: bool,
}

pub fn claude(name: &'static str, priority: u32) -> AccountSpec {
    AccountSpec {
        name,
        provider: Provider::Anthropic,
        priority,
        expired: false,
    }
}

pub fn codex(name: &'static str, priority: u32) -> AccountSpec {
    AccountSpec {
        name,
        provider: Provider::OpenAi,
        priority,
        expired: false,
    }
}

impl AccountSpec {
    pub fn expired(mut self) -> AccountSpec {
        self.expired = true;
        self
    }
}

/// How much of a response the client reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// Everything, until the server ends the body.
    All,
    /// Hang up after this many non-ping SSE events.
    DisconnectAfter(usize),
}

#[derive(Debug, Clone)]
pub struct Step {
    pub method: &'static str,
    pub path: &'static str,
    pub body: Option<Value>,
    pub read: Read,
    /// Pause after the step (lets background bookkeeping settle before the next step).
    pub wait_after: Duration,
}

impl Step {
    pub fn post(path: &'static str, body: Value) -> Step {
        Step {
            method: "POST",
            path,
            body: Some(body),
            read: Read::All,
            wait_after: Duration::from_millis(100),
        }
    }

    pub fn messages(body: Value) -> Step {
        Step::post("/v1/messages", body)
    }

    pub fn get(path: &'static str) -> Step {
        Step {
            method: "GET",
            path,
            body: None,
            read: Read::All,
            wait_after: Duration::from_millis(100),
        }
    }

    pub fn disconnect_after(mut self, events: usize) -> Step {
        self.read = Read::DisconnectAfter(events);
        self
    }

    pub fn wait(mut self, d: Duration) -> Step {
        self.wait_after = d;
        self
    }
}

/// Upstream request projection for scenarios that compare request bodies.
pub type Detail = fn(&Recorded) -> Option<String>;

pub struct Scenario {
    pub name: &'static str,
    pub about: &'static str,
    pub accounts: Vec<AccountSpec>,
    /// Turn on the Claude-family -> Codex mappings (bundled default rows) in both daemons.
    pub mapping: bool,
    /// Script one daemon's fakes (called once per daemon, on fresh fakes).
    pub script: fn(&Upstreams),
    pub steps: Vec<Step>,
    /// Rename accounts by order of first upstream use (`A`, `B`, ...) before comparing, for
    /// scenarios where *which* of several equal accounts is chosen is unspecified but the
    /// pattern (same/different) is the behavior under test.
    pub canonical_accounts: bool,
    /// Extra per-request projection of upstream inference requests.
    pub detail: Option<Detail>,
}

impl Scenario {
    pub fn new(name: &'static str, about: &'static str, script: fn(&Upstreams)) -> Scenario {
        Scenario {
            name,
            about,
            accounts: Vec::new(),
            mapping: false,
            script,
            steps: Vec::new(),
            canonical_accounts: false,
            detail: None,
        }
    }

    pub fn accounts(mut self, a: Vec<AccountSpec>) -> Scenario {
        self.accounts = a;
        self
    }

    pub fn steps(mut self, s: Vec<Step>) -> Scenario {
        self.steps = s;
        self
    }

    pub fn mapping(mut self) -> Scenario {
        self.mapping = true;
        self
    }

    pub fn canonical(mut self) -> Scenario {
        self.canonical_accounts = true;
        self
    }

    pub fn detail(mut self, d: Detail) -> Scenario {
        self.detail = Some(d);
        self
    }
}

/// An Anthropic Messages request body.
pub fn claude_body(model: &str, stream: bool, user: Option<&str>) -> Value {
    let mut b = json!({
        "model": model,
        "max_tokens": 2048,
        "stream": stream,
        "messages": [{"role": "user", "content": "hello"}],
    });
    if let Some(u) = user {
        b["metadata"] = json!({ "user_id": u });
    }
    b
}

/// Every scenario, in run order.
pub fn all() -> Vec<Scenario> {
    let mut v = Vec::new();
    v.extend(anthropic::scenarios());
    v.extend(codex::scenarios());
    v.extend(routing::scenarios());
    v
}
