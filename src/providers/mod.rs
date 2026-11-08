//! Provider identity. Only the id enum exists so far; the `Provider` trait and
//! friends arrive with the provider tasks.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Which upstream an account belongs to. Serialized as `"anthropic"` / `"openai"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Anthropic,
    OpenAi,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Anthropic => "anthropic",
            ProviderId::OpenAi => "openai",
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider {0:?} (expected \"anthropic\" or \"openai\")")]
pub struct UnknownProvider(pub String);

impl FromStr for ProviderId {
    type Err = UnknownProvider;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(ProviderId::Anthropic),
            "openai" => Ok(ProviderId::OpenAi),
            other => Err(UnknownProvider(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_fromstr_and_serde_agree() {
        for (p, s) in [
            (ProviderId::Anthropic, "anthropic"),
            (ProviderId::OpenAi, "openai"),
        ] {
            assert_eq!(p.to_string(), s);
            assert_eq!(s.parse::<ProviderId>().unwrap(), p);
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{s}\""));
            assert_eq!(
                serde_json::from_str::<ProviderId>(&format!("\"{s}\"")).unwrap(),
                p
            );
        }
        assert!("OpenAI".parse::<ProviderId>().is_err());
    }
}
