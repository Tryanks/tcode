use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone as _};

#[derive(Debug, PartialEq, Eq)]
pub enum ScheduleError {
    Usage,
    EmptyMessage,
}

pub fn parse_schedule_args(
    args: &str,
    now: DateTime<Local>,
) -> Result<(SystemTime, String, bool), ScheduleError> {
    let mut args = args.trim_start();
    let mut steer = true;

    let (first, rest) = take_token(args).ok_or(ScheduleError::Usage)?;
    if matches!(first, "-q" | "--queue") {
        steer = false;
        args = rest.trim_start();
    }

    let (when, rest) = take_token(args).ok_or(ScheduleError::Usage)?;
    let (fire_at, message) = if let Ok(date) = NaiveDate::parse_from_str(when, "%Y-%m-%d") {
        let (time, rest) = take_token(rest.trim_start()).ok_or(ScheduleError::Usage)?;
        let time = parse_time(time).ok_or(ScheduleError::Usage)?;
        let fire_at = local_datetime(date.and_time(time)).ok_or(ScheduleError::Usage)?;
        if fire_at < now {
            return Err(ScheduleError::Usage);
        }
        (fire_at.into(), rest)
    } else if let Some(duration) = parse_duration(when) {
        let fire_at = SystemTime::from(now)
            .checked_add(duration)
            .ok_or(ScheduleError::Usage)?;
        (fire_at, rest)
    } else if let Some(time) = parse_time(when) {
        let today = local_datetime(now.date_naive().and_time(time)).ok_or(ScheduleError::Usage)?;
        let fire_at = if today <= now {
            let tomorrow = now.date_naive().succ_opt().ok_or(ScheduleError::Usage)?;
            local_datetime(tomorrow.and_time(time)).ok_or(ScheduleError::Usage)?
        } else {
            today
        };
        (fire_at.into(), rest)
    } else {
        return Err(ScheduleError::Usage);
    };

    let message = message.trim();
    if message.is_empty() {
        return Err(ScheduleError::EmptyMessage);
    }
    Ok((fire_at, message.to_string(), steer))
}

pub fn format_fire_time(fire_at: SystemTime) -> String {
    let fire_at = DateTime::<Local>::from(fire_at);
    if fire_at.date_naive() == Local::now().date_naive() {
        fire_at.format("%H:%M").to_string()
    } else {
        fire_at.format("%m-%d %H:%M").to_string()
    }
}

pub fn format_countdown(fire_at: SystemTime, now: SystemTime) -> String {
    let seconds = fire_at.duration_since(now).unwrap_or_default().as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if hours == 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{hours}:{minutes:02}:{seconds:02}")
    }
}

fn take_token(value: &str) -> Option<(&str, &str)> {
    let value = value.trim_start();
    if value.is_empty() {
        return None;
    }
    let end = value.find(char::is_whitespace).unwrap_or(value.len());
    Some((&value[..end], &value[end..]))
}

fn parse_duration(value: &str) -> Option<Duration> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut seconds = 0_u64;
    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if number_start == index || index == bytes.len() {
            return None;
        }
        let number = value[number_start..index].parse::<u64>().ok()?;
        let multiplier = match bytes[index] {
            b's' => 1,
            b'm' => 60,
            b'h' => 60 * 60,
            b'd' => 24 * 60 * 60,
            _ => return None,
        };
        seconds = seconds.checked_add(number.checked_mul(multiplier)?)?;
        index += 1;
    }
    (index > 0).then(|| Duration::from_secs(seconds))
}

fn parse_time(value: &str) -> Option<NaiveTime> {
    match value.matches(':').count() {
        1 => NaiveTime::parse_from_str(value, "%H:%M").ok(),
        2 => NaiveTime::parse_from_str(value, "%H:%M:%S").ok(),
        _ => None,
    }
}

fn local_datetime(value: NaiveDateTime) -> Option<DateTime<Local>> {
    Local.from_local_datetime(&value).earliest()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike as _;

    fn now() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 7, 25, 12, 0, 0)
            .single()
            .unwrap()
    }

    fn parsed(args: &str) -> (DateTime<Local>, String, bool) {
        let (fire_at, message, steer) = parse_schedule_args(args, now()).unwrap();
        (fire_at.into(), message, steer)
    }

    #[test]
    fn parses_each_relative_duration_unit() {
        for (args, seconds) in [
            ("30s seconds", 30),
            ("10m minutes", 600),
            ("2h hours", 7_200),
            ("1d days", 86_400),
        ] {
            let (fire_at, message, steer) = parsed(args);
            assert_eq!(fire_at, now() + chrono::Duration::seconds(seconds));
            assert_eq!(message, args.split_once(' ').unwrap().1);
            assert!(steer);
        }
    }

    #[test]
    fn parses_combined_duration() {
        let (fire_at, message, steer) = parsed("1h30m  hello world  ");
        assert_eq!(fire_at, now() + chrono::Duration::minutes(90));
        assert_eq!(message, "hello world");
        assert!(steer);
    }

    #[test]
    fn parses_future_time_of_day_with_and_without_seconds() {
        let (minutes, _, _) = parsed("18:30 message");
        assert_eq!(minutes.time(), NaiveTime::from_hms_opt(18, 30, 0).unwrap());
        assert_eq!(minutes.date_naive(), now().date_naive());

        let (seconds, _, _) = parsed("18:30:45 message");
        assert_eq!(seconds.time(), NaiveTime::from_hms_opt(18, 30, 45).unwrap());
        assert_eq!(seconds.date_naive(), now().date_naive());
    }

    #[test]
    fn past_or_equal_time_of_day_rolls_to_tomorrow() {
        for value in ["11:59", "12:00:00"] {
            let (fire_at, _, _) = parsed(&format!("{value} message"));
            assert_eq!(fire_at.date_naive(), now().date_naive().succ_opt().unwrap());
        }
    }

    #[test]
    fn parses_date_and_time_with_and_without_seconds() {
        let (minutes, message, _) = parsed("2026-07-26 18:30 dated message");
        assert_eq!(
            minutes.naive_local(),
            NaiveDate::from_ymd_opt(2026, 7, 26)
                .unwrap()
                .and_hms_opt(18, 30, 0)
                .unwrap()
        );
        assert_eq!(message, "dated message");

        let (seconds, _, _) = parsed("2026-07-26 18:30:45 message");
        assert_eq!(seconds.second(), 45);
    }

    #[test]
    fn queue_flags_are_only_recognized_in_leading_position() {
        assert!(!parsed("-q 10m message").2);
        assert!(!parsed("--queue 10m message").2);

        let (_, message, steer) = parsed("10m -q remains message text");
        assert!(steer);
        assert_eq!(message, "-q remains message text");
    }

    #[test]
    fn rejects_missing_or_invalid_when() {
        for args in [
            "",
            "   ",
            "-q",
            "soon message",
            "1h30 message",
            "10x message",
            "25:00 message",
            "2026-02-30 12:00 message",
            "2026-07-26 nope message",
        ] {
            assert_eq!(
                parse_schedule_args(args, now()),
                Err(ScheduleError::Usage),
                "{args:?}"
            );
        }
    }

    #[test]
    fn rejects_past_explicit_date() {
        assert_eq!(
            parse_schedule_args("2026-07-24 18:30 message", now()),
            Err(ScheduleError::Usage)
        );
    }

    #[test]
    fn rejects_empty_message_for_every_when_shape() {
        for args in ["10m", "18:30", "2026-07-26 18:30", "--queue 1h   "] {
            assert_eq!(
                parse_schedule_args(args, now()),
                Err(ScheduleError::EmptyMessage),
                "{args:?}"
            );
        }
    }

    #[test]
    fn rejects_overflowing_duration() {
        assert_eq!(
            parse_schedule_args("18446744073709551615d message", now()),
            Err(ScheduleError::Usage)
        );
    }

    #[test]
    fn formats_today_and_other_dates() {
        let today = Local::now().date_naive().and_hms_opt(9, 5, 0).unwrap();
        let today = local_datetime(today).unwrap();
        assert_eq!(format_fire_time(today.into()), "09:05");

        let other = Local::now()
            .date_naive()
            .succ_opt()
            .unwrap()
            .and_hms_opt(9, 5, 0)
            .unwrap();
        let other = local_datetime(other).unwrap();
        assert_eq!(
            format_fire_time(other.into()),
            other.format("%m-%d %H:%M").to_string()
        );
    }

    #[test]
    fn countdown_clamps_zero_and_negative_remaining() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(format_countdown(now, now), "0:00");
        assert_eq!(format_countdown(now - Duration::from_secs(1), now), "0:00");
    }

    #[test]
    fn countdown_formats_sub_minute() {
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(format_countdown(now + Duration::from_secs(42), now), "0:42");
    }

    #[test]
    fn countdown_formats_minute_boundary() {
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(format_countdown(now + Duration::from_secs(60), now), "1:00");
        assert_eq!(
            format_countdown(now + Duration::from_secs(3_599), now),
            "59:59"
        );
    }

    #[test]
    fn countdown_formats_hour_boundary() {
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(
            format_countdown(now + Duration::from_secs(3_600), now),
            "1:00:00"
        );
        assert_eq!(
            format_countdown(now + Duration::from_secs(3_723), now),
            "1:02:03"
        );
    }
}
