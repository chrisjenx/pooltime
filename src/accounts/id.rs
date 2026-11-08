//! Validated account ids. An id is a directory name under `accounts/`, so it is
//! restricted to `^[A-Za-z0-9._-]{1,64}$` and additionally never `.` or `..`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid account id {0:?}: must match [A-Za-z0-9._-]{{1,64}} and not be \".\" or \"..\"")]
pub struct InvalidAccountId(pub String);

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AccountId {
    type Err = InvalidAccountId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ok_chars = s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if (1..=64).contains(&s.len()) && ok_chars && s != "." && s != ".." {
            Ok(AccountId(s.to_string()))
        } else {
            Err(InvalidAccountId(s.to_string()))
        }
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for AccountId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_validation() {
        assert!("a.b-c_1".parse::<AccountId>().is_ok());
        assert!("a".repeat(64).parse::<AccountId>().is_ok());
        assert!("".parse::<AccountId>().is_err());
        assert!("a/b".parse::<AccountId>().is_err());
        assert!("a".repeat(65).parse::<AccountId>().is_err());
        // path traversal and other junk
        for bad in [".", "..", "a b", "a\\b", "é", "a\0b", "a\n", "../x"] {
            assert!(
                bad.parse::<AccountId>().is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!("...".parse::<AccountId>().is_ok());
        assert_eq!("work-1".parse::<AccountId>().unwrap().to_string(), "work-1");
    }

    #[test]
    fn serde_is_a_validated_string() {
        let id: AccountId = serde_json::from_str("\"acct.1\"").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"acct.1\"");
        assert!(serde_json::from_str::<AccountId>("\"../etc\"").is_err());
        assert!(serde_json::from_str::<AccountId>("\"\"").is_err());
    }
}
