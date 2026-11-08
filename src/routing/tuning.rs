//! Weighted-selection tuning knobs (INV §2).

use anyhow::bail;

use crate::config::RoutingCfg;

/// The three knobs of the weighted strategy. Fields are public for reading; build via
/// [`Tuning::new`] (validated), [`Tuning::default`] or `From<&RoutingCfg>`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// Exponent on the five-hour factor, in `[0, 5]`.
    pub five_hour_exp: f64,
    /// Minimum 5h headroom (0-1 fraction) for an account to count as viable, in `[0, 1]`.
    pub min_headroom: f64,
    /// 5h headroom below which the score tapers, in `(0, 1]`.
    pub headroom_taper_start: f64,
}

impl Tuning {
    /// Validates every range (NaN is rejected).
    pub fn new(
        five_hour_exp: f64,
        min_headroom: f64,
        headroom_taper_start: f64,
    ) -> anyhow::Result<Tuning> {
        let t = Tuning {
            five_hour_exp,
            min_headroom,
            headroom_taper_start,
        };
        t.validate()?;
        Ok(t)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !(0.0..=5.0).contains(&self.five_hour_exp) {
            bail!(
                "five_hour_exp must be within 0..=5, got {}",
                self.five_hour_exp
            );
        }
        if !(0.0..=1.0).contains(&self.min_headroom) {
            bail!(
                "min_headroom must be within 0..=1, got {}",
                self.min_headroom
            );
        }
        if !(self.headroom_taper_start > 0.0 && self.headroom_taper_start <= 1.0) {
            bail!(
                "headroom_taper_start must be within (0, 1], got {}",
                self.headroom_taper_start
            );
        }
        Ok(())
    }
}

impl Default for Tuning {
    /// Same values as `RoutingCfg::default()`.
    fn default() -> Tuning {
        Tuning::from(&RoutingCfg::default())
    }
}

/// `Config` validation already enforces the ranges on `RoutingCfg`, so this copies the
/// three knobs without re-checking (use [`Tuning::validate`] for a hand-built config).
impl From<&RoutingCfg> for Tuning {
    fn from(c: &RoutingCfg) -> Tuning {
        Tuning {
            five_hour_exp: c.five_hour_exp,
            min_headroom: c.min_headroom,
            headroom_taper_start: c.headroom_taper_start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_routing_cfg_and_is_valid() {
        let t = Tuning::default();
        assert_eq!(t.five_hour_exp, 1.0);
        assert_eq!(t.min_headroom, 0.1);
        assert_eq!(t.headroom_taper_start, 0.2);
        t.validate().unwrap();
    }

    #[test]
    fn new_enforces_ranges() {
        assert!(Tuning::new(0.0, 0.0, 1.0).is_ok());
        assert!(Tuning::new(5.0, 1.0, 0.01).is_ok());
        for (e, m, h) in [
            (-0.1, 0.1, 0.2),
            (5.1, 0.1, 0.2),
            (1.0, -0.1, 0.2),
            (1.0, 1.1, 0.2),
            (1.0, 0.1, 0.0),
            (1.0, 0.1, 1.1),
            (f64::NAN, 0.1, 0.2),
            (1.0, f64::NAN, 0.2),
            (1.0, 0.1, f64::NAN),
        ] {
            assert!(Tuning::new(e, m, h).is_err(), "{e} {m} {h}");
        }
    }
}
