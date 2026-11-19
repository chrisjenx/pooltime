//! Release versions (`major.minor.patch[-pre]`), compared by semver precedence: the doctor's
//! release check and `pooltime update` use it.

use std::fmt;

/// A release version: `major.minor.patch[-pre]` (a leading `v`/`V` and any `+build` ignored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let nums: Vec<u64> = core
            .split('.')
            .map(|n| {
                (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| n.parse().ok())
                    .flatten()
            })
            .collect::<Option<_>>()?;
        let [major, minor, patch] = nums[..] else {
            return None;
        };
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                let ok = ids.iter().all(|i| {
                    !i.is_empty() && i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
                if !ok {
                    return None;
                }
                ids
            }
        };
        Some(Version {
            core: [major, minor, patch],
            pre,
        })
    }
}

impl Ord for Version {
    /// Semver precedence: the core numbers, then a pre-release below the release, pre-release
    /// identifiers compared numerically when both are numbers, numbers below words, and a
    /// shorter list below a longer one it starts.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => return Ordering::Equal,
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                _ => {}
            }
            for (a, b) in self.pre.iter().zip(&other.pre) {
                let o = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => a.cmp(b),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            self.pre.len().cmp(&other.pre.len())
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c] = self.core;
        write!(f, "{a}.{b}.{c}")?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_ordering() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("v0.2.0") > v("0.1.9"));
        assert!(v("0.10.0") > v("0.9.0"), "numeric, not lexical");
        assert!(
            v("1.0.0") > v("1.0.0-rc.1"),
            "release above its pre-release"
        );
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert_eq!(v("v1.2.3+build.5"), v("1.2.3"));
        assert_eq!(v("V1.2.3"), v("1.2.3"));
        assert_eq!(v("v1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        for bad in ["", "1.2", "1.2.3.4", "x.y.z", "1.2.3-", "1..3", "1.2.3-a$b"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }
}
