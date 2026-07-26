use std::time::Duration;

const KILOBYTE: f64 = 1024.0;
const MEGABYTE: f64 = KILOBYTE * KILOBYTE;
const GIGABYTE: f64 = KILOBYTE * MEGABYTE;

const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MINUTE;

pub fn pretty_bytes_size(len: u64) -> String {
    let float_len = len as f64;
    let (unit, value) = if float_len >= GIGABYTE {
        ("GiB", float_len / GIGABYTE)
    } else if float_len >= MEGABYTE {
        ("MiB", float_len / MEGABYTE)
    } else if float_len >= KILOBYTE {
        ("KiB", float_len / KILOBYTE)
    } else {
        ("B", float_len)
    };
    format!("{value:.2}{unit}")
}

/// Human readable duration for the end-of-run report, e.g. `45s`, `3m 12s`,
/// `1h 02m 03s`. Sub-second runs render as `0s`.
pub fn pretty_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / SECS_PER_HOUR;
    let minutes = (total_secs % SECS_PER_HOUR) / SECS_PER_MINUTE;
    let seconds = total_secs % SECS_PER_MINUTE;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::{pretty_bytes_size, pretty_duration};
    use std::time::Duration;

    #[test]
    fn pretty_size_gb() {
        let size: u64 = 1_200_000_000;
        assert_eq!(pretty_bytes_size(size), "1.12GiB");
    }

    #[test]
    fn pretty_size_mb() {
        let size: u64 = 1_200_000;
        assert_eq!(pretty_bytes_size(size), "1.14MiB");
    }

    #[test]
    fn pretty_size_kb() {
        let size: u64 = 1_200;
        assert_eq!(pretty_bytes_size(size), "1.17KiB");
    }

    #[test]
    fn pretty_duration_seconds_only() {
        assert_eq!(pretty_duration(Duration::from_secs(45)), "45s");
        assert_eq!(pretty_duration(Duration::from_secs(0)), "0s");
        // sub-second runs are floored to zero rather than rendering "0.4s"
        assert_eq!(pretty_duration(Duration::from_millis(400)), "0s");
    }

    #[test]
    fn pretty_duration_minutes() {
        assert_eq!(pretty_duration(Duration::from_secs(72)), "1m 12s");
        // seconds are zero padded so consecutive runs line up in the report
        assert_eq!(pretty_duration(Duration::from_secs(65)), "1m 05s");
        assert_eq!(pretty_duration(Duration::from_secs(59 * 60)), "59m 00s");
    }

    #[test]
    fn pretty_duration_hours() {
        assert_eq!(pretty_duration(Duration::from_secs(3600)), "1h 00m 00s");
        assert_eq!(pretty_duration(Duration::from_secs(3723)), "1h 02m 03s");
        // hours are not capped at 24 - a long run stays readable
        assert_eq!(
            pretty_duration(Duration::from_secs(100 * 3600)),
            "100h 00m 00s"
        );
    }
}
