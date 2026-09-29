//! When something was last launched, and how much that should count.
//!
//! The clock lives with the caller. [`Timestamp`] is a plain count of seconds
//! since the Unix epoch, so frecency maths is a pure function that a test can
//! drive to any point in time without sleeping, and no code in this crate ever
//! calls `SystemTime::now`.

use std::time::Duration;

/// Seconds since the Unix epoch, UTC.
///
/// Deliberately not a `std::time::SystemTime`: this type cannot read a clock,
/// which is what makes every frecency function in this module total and
/// testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The Unix epoch itself. Useful as a sentinel and as the "now" of a test
    /// that only cares about relative ages.
    pub const EPOCH: Timestamp = Timestamp(0);

    /// Wraps a raw Unix-seconds value. Negative values (pre-1970) are allowed;
    /// nothing in the frecency maths depends on the sign.
    pub const fn from_unix_seconds(seconds: i64) -> Timestamp {
        Timestamp(seconds)
    }

    /// The raw Unix-seconds value.
    pub const fn unix_seconds(self) -> i64 {
        self.0
    }

    /// Returns a timestamp `seconds` later, saturating instead of wrapping.
    #[must_use]
    pub const fn saturating_add_secs(self, seconds: i64) -> Timestamp {
        Timestamp(self.0.saturating_add(seconds))
    }

    /// Whole days since the epoch. Used by config, which specifies the decay
    /// half-life in days because that is what a human reads.
    #[must_use]
    pub const fn whole_days(self) -> i64 {
        self.0.div_euclid(SECONDS_PER_DAY)
    }

    /// How long ago this timestamp was, relative to `now`.
    ///
    /// Clamped at zero on purpose. A timestamp in the future means either
    /// clock skew or a machine that moved backwards in time; treating it as
    /// "0 seconds ago" keeps the decay at its maximum instead of producing a
    /// recency above 1.0 that would break the score's range guarantee.
    #[must_use]
    pub const fn age(self, now: Timestamp) -> Duration {
        let seconds = now.0.saturating_sub(self.0);
        if seconds <= 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(seconds as u64)
        }
    }

    /// Whether this timestamp is at or before `now`.
    #[must_use]
    pub const fn is_at_or_before(self, now: Timestamp) -> bool {
        self.0 <= now.0
    }
}

/// Seconds in one day. `const`, because the config default and the tests both
/// need it and neither should be able to disagree about it.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// Default half-life for the recency term: 14 days.
///
/// Two weeks is the point where "recently" stops being a useful signal. A
/// shorter life makes a launcher forget within a day; a longer one means a
/// program used in March still outranks one used this morning.
pub const DEFAULT_HALF_LIFE: Duration = Duration::from_secs(14 * SECONDS_PER_DAY as u64);

/// Share of [`Frecency::score`] given to raw launch count.
pub const FREQUENCY_SHARE: f64 = 0.45;

/// Share of [`Frecency::score`] given to how recently it was launched.
///
/// Recency outweighs frequency. A launcher that has been opened 300 times
/// because it is on the startup path is not what the user means when they type
/// its name; a program opened twice, five minutes ago, is.
pub const RECENCY_SHARE: f64 = 0.55;

/// How often, and how recently, an item has been launched.
///
/// This is the only state a launcher has about its user, so it is deliberately
/// tiny: two numbers, no event log, no decay-on-read. Decay is computed at
/// score time from an explicit `now`, which is what allows the exact same
/// numbers to be scored against two different clocks in two different tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Frecency {
    /// How many times the item has been launched.
    launches: u32,
    /// When it was last launched, if ever.
    last_launch: Option<Timestamp>,
}

impl Frecency {
    /// Nothing has ever been launched. Scores zero on every term.
    pub const NEVER: Frecency = Frecency {
        launches: 0,
        last_launch: None,
    };

    /// Builds a history, sanitising the count and dropping a `last_launch`
    /// that is inconsistent with a zero count.
    #[must_use]
    pub const fn new(launches: u32, last_launch: Option<Timestamp>) -> Frecency {
        Frecency {
            launches,
            last_launch,
        }
    }

    /// How many times the item has been launched.
    #[must_use]
    pub const fn launches(self) -> u32 {
        self.launches
    }

    /// When it was last launched, if ever.
    #[must_use]
    pub const fn last_launch(self) -> Option<Timestamp> {
        self.last_launch
    }

    /// Whether this item has no history at all.
    #[must_use]
    pub const fn is_never(self) -> bool {
        self.launches == 0 && self.last_launch.is_none()
    }

    /// Records a launch, keeping the most recent timestamp.
    #[must_use]
    pub fn launched_at(mut self, at: Timestamp) -> Frecency {
        self.launches = self.launches.saturating_add(1);
        self.last_launch = Some(match self.last_launch {
            // A launch that appears to predate a recorded one is a clock
            // change, not a reason to forget the newer fact.
            Some(previous) if previous > at => previous,
            _ => at,
        });
        self
    }

    /// How strongly the *count* of launches counts, on `0.0 ..= 1.0`.
    ///
    /// `n / (n + 1)`: zero launches scores exactly zero, one launch scores
    /// 0.5, and the curve flattens toward 1.0 without ever reaching it. The
    /// flattening is the point — the difference between 1 and 2 launches is
    /// huge in relative terms and irrelevant in absolute ones, and a linear
    /// or logarithmic count would either clip or over-rank a single launch.
    #[must_use]
    pub fn frequency(self) -> f64 {
        if self.launches == 0 {
            return 0.0;
        }
        let n = f64::from(self.launches);
        n / (n + 1.0)
    }

    /// How strongly *recency* counts, on `0.0 ..= 1.0`.
    ///
    /// Exponential decay with the given half-life: 1.0 at the moment of the
    /// last launch, 0.5 one half-life later, asymptotically 0.0. Exponential
    /// rather than linear because the interesting range is the first few days;
    /// a linear decay would still be at 0.9 after a month.
    #[must_use]
    pub fn recency(self, now: Timestamp, half_life: Duration) -> f64 {
        let Some(last) = self.last_launch else {
            return 0.0;
        };
        // A zero half-life is a misconfiguration, not a reason to return NaN.
        // Clamping the denominator keeps `age / half_life` finite in both
        // directions: 0 stays 0 (recency 1.0), anything else becomes large
        // enough that the exponential underflows to 0.0.
        let half_life = half_life.as_secs_f64().max(f64::MIN_POSITIVE);
        let exponent = -(last.age(now).as_secs_f64() / half_life);
        2.0f64.powf(exponent)
    }

    /// The combined frecency score, on `0.0 ..= 1.0`.
    #[must_use]
    pub fn score(self, now: Timestamp, half_life: Duration) -> f64 {
        let score =
            FREQUENCY_SHARE * self.frequency() + RECENCY_SHARE * self.recency(now, half_life);
        crate::policy::clamp01(score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(count: i64) -> Duration {
        Duration::from_secs((count * SECONDS_PER_DAY) as u64)
    }

    #[test]
    fn never_scores_zero_on_every_term() {
        let now = Timestamp::from_unix_seconds(1_000_000);
        assert!(Frecency::NEVER.is_never());
        assert_eq!(Frecency::NEVER.frequency(), 0.0);
        assert_eq!(Frecency::NEVER.recency(now, DEFAULT_HALF_LIFE), 0.0);
        assert_eq!(Frecency::NEVER.score(now, DEFAULT_HALF_LIFE), 0.0);
    }

    #[test]
    fn never_but_timestamped_is_still_counted() {
        // A row written by an older build could plausibly have a timestamp and
        // no count. Dropping the timestamp would silently forget the launch.
        let odd = Frecency::new(0, Some(Timestamp::EPOCH));
        assert_eq!(odd.frequency(), 0.0);
        assert!(
            odd.recency(Timestamp::EPOCH, DEFAULT_HALF_LIFE) > 0.9,
            "a timestamp alone should still carry recency"
        );
    }

    #[test]
    fn recency_halves_exactly_at_the_half_life() {
        let launched = Frecency::new(1, Some(Timestamp::EPOCH));
        let at = |halves: u32| {
            Timestamp::EPOCH
                .saturating_add_secs((DEFAULT_HALF_LIFE.as_secs() * u64::from(halves)) as i64)
        };

        assert!((launched.recency(at(0), DEFAULT_HALF_LIFE) - 1.0).abs() < 1e-12);
        assert!((launched.recency(at(1), DEFAULT_HALF_LIFE) - 0.5).abs() < 1e-12);
        assert!((launched.recency(at(2), DEFAULT_HALF_LIFE) - 0.25).abs() < 1e-12);
        assert!((launched.recency(at(4), DEFAULT_HALF_LIFE) - 0.0625).abs() < 1e-12);
    }

    #[test]
    fn a_shorter_half_life_decays_faster() {
        let launched = Frecency::new(1, Some(Timestamp::EPOCH));
        let week = Duration::from_secs(7 * SECONDS_PER_DAY as u64);
        let now = Timestamp::from_unix_seconds(SECONDS_PER_DAY * 7);
        assert!(launched.recency(now, week) < launched.recency(now, DEFAULT_HALF_LIFE));
        assert!((launched.recency(now, week) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn recency_is_monotonically_decreasing_and_stays_in_range() {
        let base = Timestamp::from_unix_seconds(1_700_000_000);
        let launched = Frecency::new(1, Some(base));
        let mut previous = f64::INFINITY;
        for step in 0..200 {
            let now = base.saturating_add_secs(step * 3_600);
            let recency = launched.recency(now, DEFAULT_HALF_LIFE);
            assert!((0.0..=1.0).contains(&recency), "{recency} out of range");
            assert!(recency <= previous, "recency rose at step {step}");
            previous = recency;
        }
    }

    #[test]
    fn a_zero_half_life_degrades_to_a_step_function_instead_of_nan() {
        let now = Timestamp::from_unix_seconds(10);
        let launched = Frecency::new(1, Some(Timestamp::EPOCH));
        assert_eq!(launched.recency(now, Duration::ZERO), 0.0);
        assert_eq!(launched.recency(Timestamp::EPOCH, Duration::ZERO), 1.0);
        // And the assert_eq above is the real assertion: NaN != NaN would have
        // failed here even though the values "looked" fine.
    }

    #[test]
    fn a_future_timestamp_cannot_exceed_the_top_of_the_scale() {
        // Clock skew, or a machine whose clock moved back. Must clamp, not
        // exceed 1.0, or the final score could exceed its documented range.
        let now = Timestamp::from_unix_seconds(1_000);
        let from_the_future = Frecency::new(1, Some(Timestamp::from_unix_seconds(9_999)));
        assert_eq!(from_the_future.recency(now, DEFAULT_HALF_LIFE), 1.0);
        assert!(from_the_future.score(now, DEFAULT_HALF_LIFE) <= 1.0);
    }

    #[test]
    fn frequency_is_bounded_and_monotonic() {
        let mut previous = -1.0;
        for launches in 0..64u32 {
            let frequency = Frecency::new(launches, None).frequency();
            assert!((0.0..=1.0).contains(&frequency), "{frequency} out of range");
            assert!(frequency > previous, "frequency dipped at {launches}");
            previous = frequency;
        }
        assert_eq!(Frecency::new(0, None).frequency(), 0.0);
        assert!((Frecency::new(1, None).frequency() - 0.5).abs() < 1e-12);
        // Never reaches 1.0, so the recency term always has room to matter.
        assert!(Frecency::new(u32::MAX, None).frequency() < 1.0);
    }

    #[test]
    fn recency_outweighs_frequency_at_equal_ages() {
        let now = Timestamp::from_unix_seconds(1_000_000);
        let day_old = now.saturating_add_secs(-SECONDS_PER_DAY);
        let much_used = Frecency::new(1_000, Some(day_old));

        // Isolate the recency term from a frecency score by subtracting the
        // frequency contribution back out.
        let recency_of = |f: Frecency| {
            (f.score(now, DEFAULT_HALF_LIFE) - FREQUENCY_SHARE * f.frequency()) / RECENCY_SHARE
        };

        // A million launches a day old should not beat one launch a minute old.
        // The frequency term alone ranks the first enormously higher.
        let fresh = Frecency::new(1, Some(now.saturating_add_secs(-60)));
        assert!(recency_of(fresh) > recency_of(much_used));
        assert!(
            much_used.frequency() > fresh.frequency() * 1.5,
            "precondition: frequency alone would rank the much-used item far higher, \
             {} vs {}",
            much_used.frequency(),
            fresh.frequency()
        );
    }

    #[test]
    fn launched_at_counts_up_and_keeps_the_newest_timestamp() {
        let history = Frecency::NEVER
            .launched_at(Timestamp::from_unix_seconds(100))
            .launched_at(Timestamp::from_unix_seconds(300))
            .launched_at(Timestamp::from_unix_seconds(200));

        assert_eq!(history.launches(), 3);
        assert_eq!(
            history.last_launch(),
            Some(Timestamp::from_unix_seconds(300))
        );
    }

    #[test]
    fn launched_at_saturates_instead_of_overflowing() {
        let history = Frecency::new(u32::MAX, None).launched_at(Timestamp::EPOCH);
        assert_eq!(history.launches(), u32::MAX);
    }

    #[test]
    fn age_clamps_and_whole_days_truncates_towards_negative_infinity() {
        let now = Timestamp::from_unix_seconds(SECONDS_PER_DAY * 10 + 100);
        assert_eq!(
            now.saturating_add_secs(-SECONDS_PER_DAY * 3).age(now),
            days(3)
        );
        assert_eq!(now.age(now), Duration::ZERO);
        // A launch "in the future" reads as zero age, never as a negative one.
        assert_eq!(now.saturating_add_secs(500).age(now), Duration::ZERO);
        assert!(now.age(now).as_secs() < SECONDS_PER_DAY as u64);
        assert_eq!(Timestamp::from_unix_seconds(-1).whole_days(), -1);
    }

    #[test]
    fn saturating_add_secs_does_not_wrap() {
        assert_eq!(
            Timestamp::from_unix_seconds(i64::MAX)
                .saturating_add_secs(10)
                .unix_seconds(),
            i64::MAX
        );
        assert_eq!(
            Timestamp::from_unix_seconds(i64::MIN)
                .saturating_add_secs(-10)
                .unix_seconds(),
            i64::MIN
        );
    }
}
