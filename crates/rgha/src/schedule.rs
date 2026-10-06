//! Scheduled warm pools: keep more idle runners during working hours (when
//! CI fans out and cold starts cost the most) and scale to the class
//! `min_idle` outside them.

use anyhow::{Context, bail};
use chrono::{DateTime, Datelike, NaiveTime, Utc, Weekday};
use chrono_tz::Tz;
use serde::Deserialize;

/// One `[[class.warm_schedule]]` window, e.g.
/// `{ days = ["mon","tue","wed","thu","fri"], from = "08:00", to = "18:00", timezone = "America/Chicago", min_idle = 4 }`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WarmWindow {
    /// Weekdays the window starts on (`mon`..`sun`). Empty = every day.
    #[serde(default)]
    pub days: Vec<String>,
    /// Local start time, `HH:MM`.
    pub from: String,
    /// Local end time, `HH:MM` (exclusive). Earlier than `from` wraps past midnight.
    pub to: String,
    /// IANA timezone. Default: UTC.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Idle runners kept warm while the window is open.
    pub min_idle: u32,
}

/// A validated window.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    days: Vec<Weekday>,
    from: NaiveTime,
    to: NaiveTime,
    tz: Tz,
    pub min_idle: u32,
}

impl WarmWindow {
    pub fn parse(&self) -> anyhow::Result<Window> {
        let time =
            |s: &str| NaiveTime::parse_from_str(s, "%H:%M").with_context(|| format!("invalid time {s:?} (want HH:MM)"));
        let from = time(&self.from)?;
        let to = time(&self.to)?;
        if from == to {
            bail!("warm_schedule window {}–{} is empty", self.from, self.to);
        }
        let tz: Tz = match &self.timezone {
            Some(name) => name.parse().map_err(|_| anyhow::anyhow!("unknown timezone {name:?}"))?,
            None => Tz::UTC,
        };
        let days = self
            .days
            .iter()
            .map(|d| d.parse::<Weekday>().map_err(|_| anyhow::anyhow!("unknown weekday {d:?}")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Window { days, from, to, tz, min_idle: self.min_idle })
    }
}

impl Window {
    /// Whether the window is open at `now`. A window that wraps past
    /// midnight belongs to the day it starts on.
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        let local = now.with_timezone(&self.tz);
        let t = local.time();
        let on = |day: Weekday| self.days.is_empty() || self.days.contains(&day);
        if self.from < self.to {
            on(local.weekday()) && t >= self.from && t < self.to
        } else {
            (on(local.weekday()) && t >= self.from) || (on(local.weekday().pred()) && t < self.to)
        }
    }
}

/// Warm runners wanted at `now`: the largest open window, else `default`.
pub fn min_idle_at(windows: &[Window], default: u32, now: DateTime<Utc>) -> u32 {
    windows.iter().filter(|w| w.is_open(now)).map(|w| w.min_idle).max().unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(days: &[&str], from: &str, to: &str, tz: &str, n: u32) -> Window {
        WarmWindow {
            days: days.iter().map(|d| d.to_string()).collect(),
            from: from.into(),
            to: to.into(),
            timezone: Some(tz.into()),
            min_idle: n,
        }
        .parse()
        .unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn business_hours_in_a_timezone() {
        let w = [window(&["mon", "tue", "wed", "thu", "fri"], "08:00", "18:00", "America/Chicago", 4)];
        // Monday 2026-10-05: Chicago is UTC-5 (CDT).
        assert_eq!(min_idle_at(&w, 0, at("2026-10-05T12:59:59Z")), 0, "07:59 local");
        assert_eq!(min_idle_at(&w, 0, at("2026-10-05T13:00:00Z")), 4, "08:00 local");
        assert_eq!(min_idle_at(&w, 0, at("2026-10-05T22:59:59Z")), 4, "17:59 local");
        assert_eq!(min_idle_at(&w, 0, at("2026-10-05T23:00:00Z")), 0, "18:00 local is exclusive");
        assert_eq!(min_idle_at(&w, 1, at("2026-10-04T15:00:00Z")), 1, "Sunday falls back to the default");
    }

    #[test]
    fn dst_shifts_the_window_in_utc() {
        let w = [window(&[], "08:00", "18:00", "America/Chicago", 2)];
        // After DST ends (2026-11-01) Chicago is UTC-6.
        assert_eq!(min_idle_at(&w, 0, at("2026-11-02T13:30:00Z")), 0, "07:30 CST");
        assert_eq!(min_idle_at(&w, 0, at("2026-11-02T14:00:00Z")), 2, "08:00 CST");
    }

    #[test]
    fn overnight_window_belongs_to_its_start_day() {
        let w = [window(&["fri"], "22:00", "02:00", "UTC", 3)];
        assert_eq!(min_idle_at(&w, 0, at("2026-10-09T23:00:00Z")), 3, "Friday 23:00");
        assert_eq!(min_idle_at(&w, 0, at("2026-10-10T01:00:00Z")), 3, "Saturday 01:00 still Friday's window");
        assert_eq!(min_idle_at(&w, 0, at("2026-10-10T23:00:00Z")), 0, "Saturday 23:00");
    }

    #[test]
    fn overlapping_windows_take_the_largest() {
        let w = [window(&[], "08:00", "18:00", "UTC", 2), window(&[], "12:00", "14:00", "UTC", 5)];
        assert_eq!(min_idle_at(&w, 0, at("2026-10-05T13:00:00Z")), 5);
        assert_eq!(min_idle_at(&w, 0, at("2026-10-05T09:00:00Z")), 2);
    }

    #[test]
    fn rejects_bad_input() {
        let bad = |days: &[&str], from: &str, to: &str, tz: &str| {
            WarmWindow {
                days: days.iter().map(|d| d.to_string()).collect(),
                from: from.into(),
                to: to.into(),
                timezone: Some(tz.into()),
                min_idle: 1,
            }
            .parse()
            .is_err()
        };
        assert!(bad(&[], "8am", "18:00", "UTC"));
        assert!(bad(&[], "08:00", "08:00", "UTC"));
        assert!(bad(&["funday"], "08:00", "18:00", "UTC"));
        assert!(bad(&[], "08:00", "18:00", "Mars/Olympus"));
    }
}
