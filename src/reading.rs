//! A tiny validity wrapper for measurements that may not be obtainable.
//!
//! NeoMonitor samples data from OS APIs that can be unsupported, fail transiently, or
//! return values that no longer reflect "now". Wrapping such values in [`Reading`] keeps
//! "we have a real number", "we have an old number", and "we have nothing" distinct, so
//! the UI never has to invent a `0` to mean "unknown".
//!
//! This is deliberately a small, typed design rather than a general framework.

use std::time::Duration;

/// A single measurement and how much it can be trusted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reading<T> {
    /// A fresh, trustworthy measurement.
    Valid(T),
    /// A previously valid measurement that could not be refreshed; it is reported for
    /// continuity but should be flagged as stale by the UI.
    Stale(T),
    /// No measurement could be obtained (unsupported platform, missing hardware, error,
    /// or first sample before enough data exists).
    #[default]
    Unavailable,
}

/// Small, reusable accessor surface; not every method is used by the current UI, but the
/// whole set is exercised by the unit tests below.
#[allow(dead_code)]
impl<T> Reading<T> {
    /// A reading is "available" when it carries a value, fresh or stale.
    pub fn is_available(&self) -> bool {
        !matches!(self, Reading::Unavailable)
    }

    /// A reading is "valid" only when it is fresh.
    pub fn is_valid(&self) -> bool {
        matches!(self, Reading::Valid(_))
    }

    /// Borrow the underlying value, fresh or stale.
    pub fn value(&self) -> Option<&T> {
        match self {
            Reading::Valid(v) | Reading::Stale(v) => Some(v),
            Reading::Unavailable => None,
        }
    }

    /// Consume the reading, discarding the freshness distinction.
    pub fn into_value(self) -> Option<T> {
        match self {
            Reading::Valid(v) | Reading::Stale(v) => Some(v),
            Reading::Unavailable => None,
        }
    }

    /// Map the value while preserving the freshness state.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Reading<U> {
        match self {
            Reading::Valid(v) => Reading::Valid(f(v)),
            Reading::Stale(v) => Reading::Stale(f(v)),
            Reading::Unavailable => Reading::Unavailable,
        }
    }

    /// Demote a fresh value to stale, preserving unavailable.
    pub fn as_stale(&self) -> Reading<T>
    where
        T: Clone,
    {
        match self {
            Reading::Valid(v) => Reading::Stale(v.clone()),
            other => other.clone(),
        }
    }
}

#[allow(dead_code)] // Small reusable API surface; exercised by the tests below.
impl<T: Copy> Reading<T> {
    /// Return the value when available, otherwise `default`.
    pub fn unwrap_or(self, default: T) -> T {
        self.into_value().unwrap_or(default)
    }
}

/// How far a held value has drifted from its freshness budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Within the freshness budget.
    Current,
    /// Up to twice the budget old.
    Stale,
    /// Older than twice the budget.
    Expired,
}

/// Classify `age` against a `budget` (the expected update interval).
///
/// A zero budget means "always expired", which is the safe choice for callers that have
/// not established a real cadence yet.
pub fn freshness(age: Duration, budget: Duration) -> Freshness {
    if budget.is_zero() {
        return Freshness::Expired;
    }
    if age <= budget {
        Freshness::Current
    } else if age <= budget.saturating_mul(2) {
        Freshness::Stale
    } else {
        Freshness::Expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unavailable() {
        assert_eq!(Reading::<u64>::default(), Reading::Unavailable);
        assert!(!Reading::<u64>::default().is_available());
    }

    #[test]
    fn validity_predicates() {
        assert!(Reading::Valid(1u64).is_valid());
        assert!(Reading::Valid(1u64).is_available());
        assert!(!Reading::Stale(1u64).is_valid());
        assert!(Reading::Stale(1u64).is_available());
        assert!(!Reading::<u64>::Unavailable.is_available());
    }

    #[test]
    fn value_access_preserves_state() {
        assert_eq!(Reading::Valid(7u64).value(), Some(&7));
        assert_eq!(Reading::Stale(7u64).value(), Some(&7));
        assert_eq!(Reading::<u64>::Unavailable.value(), None);
        assert_eq!(Reading::Valid(7u64).into_value(), Some(7));
        assert_eq!(Reading::<u64>::Unavailable.into_value(), None);
    }

    #[test]
    fn map_keeps_state() {
        assert_eq!(Reading::Valid(2u64).map(|v| v * 10), Reading::Valid(20));
        assert_eq!(Reading::Stale(2u64).map(|v| v * 10), Reading::Stale(20));
        assert_eq!(
            Reading::<u64>::Unavailable.map(|v| v * 10),
            Reading::Unavailable
        );
    }

    #[test]
    fn as_stale_only_demotes_valid() {
        assert_eq!(Reading::Valid(3u64).as_stale(), Reading::Stale(3));
        assert_eq!(Reading::Stale(3u64).as_stale(), Reading::Stale(3));
        assert_eq!(Reading::<u64>::Unavailable.as_stale(), Reading::Unavailable);
    }

    #[test]
    fn unwrap_or_defaults_only_when_missing() {
        assert_eq!(Reading::Valid(5u64).unwrap_or(0), 5);
        assert_eq!(Reading::Stale(5u64).unwrap_or(0), 5);
        assert_eq!(Reading::<u64>::Unavailable.unwrap_or(9), 9);
    }

    #[test]
    fn freshness_boundaries() {
        let budget = Duration::from_millis(100);
        assert_eq!(
            freshness(Duration::from_millis(0), budget),
            Freshness::Current
        );
        assert_eq!(
            freshness(Duration::from_millis(100), budget),
            Freshness::Current
        );
        assert_eq!(
            freshness(Duration::from_millis(101), budget),
            Freshness::Stale
        );
        assert_eq!(
            freshness(Duration::from_millis(200), budget),
            Freshness::Stale
        );
        assert_eq!(
            freshness(Duration::from_millis(201), budget),
            Freshness::Expired
        );
        assert_eq!(
            freshness(Duration::from_secs(1), Duration::ZERO),
            Freshness::Expired
        );
    }
}
