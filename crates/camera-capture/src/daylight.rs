//! Daylight gating: skip capture outside the sun's up-hours.
//!
//! Sunrise and sunset are computed locally — pure Rust, no network — from the
//! site latitude/longitude via the `sunrise` crate, so the loop decides whether
//! to capture before any ONVIF or cloud round-trip and keeps working if the Pi
//! loses internet. The capture window is `[sunrise + margin, sunset - margin]`
//! expressed in UTC; all times are UTC so the result is independent of the Pi's
//! local timezone. A positive `daylight_margin_mins` drops the twilight edges
//! where frames would be too dark.

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use sunrise::{Coordinates, SolarDay, SolarEvent};

/// True if `now` falls inside the configured daylight capture window.
///
/// `daylight_only` and `margin_mins` are per-camera; `coord` is the shared
/// site location. Returns `true` when daylight gating is disabled, so the
/// caller can invoke this unconditionally. Coordinates are validated at config
/// load, so the missing branch is defensive and treats the tick as daylight
/// rather than silently stalling the loop.
pub fn is_daylight(
    daylight_only: bool,
    margin_mins: i64,
    coord: Coordinates,
    now: DateTime<Utc>,
) -> bool {
    if !daylight_only {
        return true;
    }

    let date = NaiveDate::from_ymd_opt(now.year(), now.month(), now.day())
        .expect("valid date constructed from a DateTime");
    let day = SolarDay::new(coord, date);
    let sunrise = day.event_time(SolarEvent::Sunrise);
    let sunset = day.event_time(SolarEvent::Sunset);

    let margin = chrono::Duration::minutes(margin_mins);
    now >= sunrise + margin && now <= sunset - margin
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tel Aviv — a mid-latitude site with non-trivial sunrise/sunset year-round.
    fn coord() -> Coordinates {
        Coordinates::new(32.0853, 34.7818).unwrap()
    }

    #[test]
    fn noon_is_daylight() {
        // 2024-06-21 11:00 UTC = 14:00 local — bright midday, well inside.
        let now = DateTime::<Utc>::from_timestamp(1718958000, 0).unwrap();
        assert!(is_daylight(true, 0, coord(), now));
    }

    #[test]
    fn midnight_is_not_daylight() {
        // 2024-06-21 00:00 UTC — long before sunrise.
        let now = DateTime::<Utc>::from_timestamp(1718918400, 0).unwrap();
        assert!(!is_daylight(true, 0, coord(), now));
    }

    #[test]
    fn disabled_is_always_daylight() {
        // Deep night, but gating off → capture anyway.
        let now = DateTime::<Utc>::from_timestamp(1718918400, 0).unwrap();
        assert!(is_daylight(false, 0, coord(), now));
    }

    #[test]
    fn margin_trims_window_edges() {
        // The instant of sunrise itself is inside the window at margin 0, but
        // a 60-minute margin pushes the start past it.
        let coord = coord();
        let date = NaiveDate::from_ymd_opt(2024, 6, 21).unwrap();
        let sunrise = SolarDay::new(coord, date).event_time(SolarEvent::Sunrise);

        assert!(is_daylight(true, 0, coord, sunrise));
        assert!(!is_daylight(true, 60, coord, sunrise));
    }
}
