//! Human-readable remaining-time strings for UI clients.
//!
//! Frontends keep `resets_at` as an absolute timestamp and format it at render
//! time so the string stays accurate between provider refreshes. Snapshots do
//! not carry a pre-rendered duration: a five-minute-old "in 2 hours" would be
//! wrong even when the timestamp is still correct.
//!
//! Long style (`in 1 day 23 hours`) is for the web and GTK apps. Short style
//! (`in 1d 23h`) is the compact form used by the Plasma widget. Both keep two
//! units so a remainder like 23 hours is not rounded into a whole day.

use time::OffsetDateTime;

/// How to render a relative timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeTimeStyle {
    /// `in 1 day 23 hours`, `in 3 hours 12 min`.
    Long,
    /// `in 1d 23h`, `in 3h 12m`.
    Short,
}

/// Format `at` relative to `now` using [`RelativeTimeStyle::Long`].
pub fn format_relative_time(at: OffsetDateTime, now: OffsetDateTime) -> String {
    format_relative_time_styled(at, now, RelativeTimeStyle::Long)
}

/// Format `at` relative to `now` using the requested style.
pub fn format_relative_time_styled(
    at: OffsetDateTime,
    now: OffsetDateTime,
    style: RelativeTimeStyle,
) -> String {
    format_relative_seconds((at - now).whole_seconds(), style)
}

fn format_relative_seconds(seconds: i64, style: RelativeTimeStyle) -> String {
    match u64::try_from(seconds) {
        Ok(seconds) => format!("in {}", format_duration(seconds, style)),
        Err(_) => {
            let ago = seconds.unsigned_abs();
            if ago < 60 {
                "now".to_owned()
            } else {
                format!("{} ago", format_duration(ago, style))
            }
        }
    }
}

fn format_duration(seconds: u64, style: RelativeTimeStyle) -> String {
    if seconds < 60 {
        return match style {
            RelativeTimeStyle::Long => "<1 min".to_owned(),
            RelativeTimeStyle::Short => "<1m".to_owned(),
        };
    }

    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;

    match style {
        RelativeTimeStyle::Long => format_long(days, hours, minutes),
        RelativeTimeStyle::Short => format_short(days, hours, minutes),
    }
}

fn format_long(days: u64, hours: u64, minutes: u64) -> String {
    if days > 0 {
        let days = plural(days, "day");
        if hours == 0 {
            days
        } else {
            format!("{days} {}", plural(hours, "hour"))
        }
    } else if hours > 0 {
        let hours = plural(hours, "hour");
        if minutes == 0 {
            hours
        } else {
            format!("{hours} {minutes} min")
        }
    } else {
        format!("{minutes} min")
    }
}

fn format_short(days: u64, hours: u64, minutes: u64) -> String {
    if days > 0 {
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d {hours}h")
        }
    } else if hours > 0 {
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    } else {
        format!("{minutes}m")
    }
}

fn plural(count: u64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp_at(offset_seconds: i64) -> (OffsetDateTime, OffsetDateTime) {
        let now = OffsetDateTime::from_unix_timestamp(1_780_704_000).expect("valid now");
        let at = now
            .checked_add(time::Duration::seconds(offset_seconds))
            .expect("valid timestamp");
        (at, now)
    }

    #[test]
    fn long_style_keeps_remaining_hours_instead_of_rounding_to_days() {
        let (at, now) = timestamp_at(47 * 3_600);
        assert_eq!(format_relative_time(at, now), "in 1 day 23 hours");
    }

    #[test]
    fn short_style_keeps_remaining_hours_instead_of_rounding_to_days() {
        let (at, now) = timestamp_at(47 * 3_600);
        assert_eq!(
            format_relative_time_styled(at, now, RelativeTimeStyle::Short),
            "in 1d 23h"
        );
    }

    #[test]
    fn long_style_covers_each_unit_boundary() {
        let cases = [
            (0, "in <1 min"),
            (59, "in <1 min"),
            (60, "in 1 min"),
            (3_599, "in 59 min"),
            (3_600, "in 1 hour"),
            (3_660, "in 1 hour 1 min"),
            (13_140, "in 3 hours 39 min"),
            (86_400, "in 1 day"),
            (90_000, "in 1 day 1 hour"),
            (172_800, "in 2 days"),
        ];
        for (seconds, expected) in cases {
            let (at, now) = timestamp_at(seconds);
            assert_eq!(
                format_relative_time(at, now),
                expected,
                "long style for {seconds}s"
            );
        }
    }

    #[test]
    fn short_style_covers_each_unit_boundary() {
        let cases = [
            (0, "in <1m"),
            (59, "in <1m"),
            (60, "in 1m"),
            (3_599, "in 59m"),
            (3_600, "in 1h"),
            (3_660, "in 1h 1m"),
            (13_140, "in 3h 39m"),
            (86_400, "in 1d"),
            (90_000, "in 1d 1h"),
            (172_800, "in 2d"),
        ];
        for (seconds, expected) in cases {
            let (at, now) = timestamp_at(seconds);
            assert_eq!(
                format_relative_time_styled(at, now, RelativeTimeStyle::Short),
                expected,
                "short style for {seconds}s"
            );
        }
    }

    #[test]
    fn past_times_use_ago_except_within_a_minute() {
        let (at, now) = timestamp_at(-30);
        assert_eq!(format_relative_time(at, now), "now");

        let (at, now) = timestamp_at(-(47 * 3_600));
        assert_eq!(format_relative_time(at, now), "1 day 23 hours ago");
        assert_eq!(
            format_relative_time_styled(at, now, RelativeTimeStyle::Short),
            "1d 23h ago"
        );
    }
}
