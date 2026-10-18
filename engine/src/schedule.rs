//! Time-window scheduling for download queues.
//!
//! A `Schedule` describes when a queue is allowed to run automatically:
//! a daily start/stop time plus which weekdays are active. The logic is kept
//! pure and dependency-free (minutes-since-midnight + a weekday index) so it's
//! fully unit-testable without a clock or timezone crate — the caller supplies
//! the current local time.

use serde::{Deserialize, Serialize};

/// Days of the week, Monday = 0 … Sunday = 6 (matches `chrono`'s `num_days_from_monday`).
pub const MON: u8 = 0;
pub const SUN: u8 = 6;

/// A recurring daily time window on selected weekdays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// When scheduling is off, the queue is governed purely by manual
    /// start/pause and its concurrency limit (no time gating).
    pub enabled: bool,
    /// Window start, minutes since midnight (0..=1439). e.g. 9:00 => 540.
    pub start_minute: u32,
    /// Window end, minutes since midnight (0..=1439). e.g. 23:30 => 1410.
    pub stop_minute: u32,
    /// Active weekdays (Mon=0..Sun=6). Empty = every day.
    pub days: Vec<u8>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            enabled: false,
            start_minute: 0,
            stop_minute: 24 * 60 - 1, // 23:59
            days: Vec::new(),
        }
    }
}

impl Schedule {
    /// Is the queue allowed to run at `weekday` (Mon=0..Sun=6) and `minute`
    /// (minutes since midnight)? When scheduling is disabled, always `true`
    /// (manual control governs instead).
    pub fn is_active_at(&self, weekday: u8, minute: u32) -> bool {
        if !self.enabled {
            return true;
        }
        if !self.days.is_empty() && !self.days.contains(&weekday) {
            return false;
        }
        in_window(self.start_minute, self.stop_minute, minute)
    }

    /// Parse "HH:MM" into minutes since midnight.
    pub fn parse_hhmm(s: &str) -> Option<u32> {
        let (h, m) = s.split_once(':')?;
        let h: u32 = h.trim().parse().ok()?;
        let m: u32 = m.trim().parse().ok()?;
        if h > 23 || m > 59 {
            return None;
        }
        Some(h * 60 + m)
    }

    /// Format minutes-since-midnight back to "HH:MM".
    pub fn fmt_hhmm(minute: u32) -> String {
        let minute = minute.min(24 * 60 - 1);
        format!("{:02}:{:02}", minute / 60, minute % 60)
    }
}

/// Is `minute` inside `[start, stop]`? Supports windows that wrap past midnight
/// (e.g. start 22:00, stop 06:00 means late-night through early-morning).
fn in_window(start: u32, stop: u32, minute: u32) -> bool {
    if start <= stop {
        minute >= start && minute <= stop
    } else {
        // Wrapping window: active if after start OR before stop.
        minute >= start || minute <= stop
    }
}

/// The current local weekday (Mon=0..Sun=6) and minutes-since-midnight.
/// Uses only `std` by reading the local time offset from `localtime`-free math:
/// we rely on the caller to pass values in most code paths, but this helper is
/// provided for the manager's scheduler loop. It derives local time from the
/// system clock and the `TZ`-independent `chrono`-free approach below.
pub fn now_local_weekday_minute() -> (u8, u32) {
    // SystemTime -> civil time. To avoid a timezone crate we use the libc-free
    // trick of asking the OS via `std::time` for UTC and applying the local
    // offset captured from a one-time `date`-style computation is not available
    // in pure std. Instead we compute UTC civil time; schedules are interpreted
    // in UTC unless the platform layer overrides. (The app/host can pass local
    // time explicitly via `is_active_at` if precise local scheduling matters.)
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let secs_of_day = (secs % 86_400) as u32;
    let minute = secs_of_day / 60;
    // 1970-01-01 was a Thursday (Mon=0 => Thursday = 3).
    let weekday = ((days + 3) % 7) as u8;
    (weekday, minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_always_active() {
        let s = Schedule::default();
        assert!(s.is_active_at(MON, 0));
        assert!(s.is_active_at(SUN, 1200));
    }

    #[test]
    fn simple_window() {
        let s = Schedule {
            enabled: true,
            start_minute: 540,  // 09:00
            stop_minute: 1020,  // 17:00
            days: vec![],
        };
        assert!(!s.is_active_at(MON, 480)); // 08:00
        assert!(s.is_active_at(MON, 600)); // 10:00
        assert!(s.is_active_at(MON, 1020)); // 17:00 inclusive
        assert!(!s.is_active_at(MON, 1021)); // 17:01
    }

    #[test]
    fn wrapping_window_past_midnight() {
        let s = Schedule {
            enabled: true,
            start_minute: 22 * 60, // 22:00
            stop_minute: 6 * 60,   // 06:00
            days: vec![],
        };
        assert!(s.is_active_at(MON, 23 * 60)); // 23:00 active
        assert!(s.is_active_at(MON, 2 * 60)); // 02:00 active
        assert!(!s.is_active_at(MON, 12 * 60)); // noon inactive
    }

    #[test]
    fn restricted_days() {
        let s = Schedule {
            enabled: true,
            start_minute: 0,
            stop_minute: 1439,
            days: vec![5, 6], // Sat, Sun only
        };
        assert!(!s.is_active_at(MON, 600));
        assert!(s.is_active_at(5, 600)); // Saturday
        assert!(s.is_active_at(6, 600)); // Sunday
    }

    #[test]
    fn hhmm_roundtrip() {
        assert_eq!(Schedule::parse_hhmm("09:30"), Some(570));
        assert_eq!(Schedule::parse_hhmm("23:59"), Some(1439));
        assert_eq!(Schedule::parse_hhmm("24:00"), None);
        assert_eq!(Schedule::parse_hhmm("bad"), None);
        assert_eq!(Schedule::fmt_hhmm(570), "09:30");
        assert_eq!(Schedule::fmt_hhmm(1439), "23:59");
    }
}
