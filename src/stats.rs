use crate::utils::{pretty_bytes_size, pretty_duration};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Counters gathered over a whole run. They feed the end-of-run report and
/// decide the process exit code.
///
/// The counters are `Relaxed` atomics: concurrent downloads only ever
/// increment them and nothing branches on one counter to interpret another,
/// so no ordering between them is needed. They are read once at the end, when
/// every download has finished.
pub struct RunStats {
    started_at: Instant,
    completed: AtomicU64,
    skipped: AtomicU64,
    failed: AtomicU64,
    downloaded_bytes: AtomicU64,
}

impl RunStats {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            completed: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            downloaded_bytes: AtomicU64::new(0),
        }
    }

    /// A file was fetched and moved to its final name during this run.
    pub fn record_completed(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    /// The destination file was already there, so nothing was fetched.
    pub fn record_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// A link could not be downloaded, retries included.
    pub fn record_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Bytes received from the network during this run.
    ///
    /// This is transfer volume, not the size of the resulting files: bytes
    /// already present in a resumed `.part` are excluded, while bytes from an
    /// attempt that later failed (and got retried) are included, because they
    /// did travel over the wire.
    pub fn add_downloaded_bytes(&self, bytes: u64) {
        self.downloaded_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn failed_count(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Links that reached a verdict - the sum of the three outcomes.
    pub fn processed_count(&self) -> u64 {
        self.completed.load(Ordering::Relaxed)
            + self.skipped.load(Ordering::Relaxed)
            + self.failed.load(Ordering::Relaxed)
    }

    /// The end-of-run report, one string per line to print.
    ///
    /// The outcome counters are always spelled out, including zeros, so that
    /// "everything worked" and "some downloads failed" look different at a
    /// glance instead of both showing a full progress bar.
    ///
    /// `interrupted` reports a run cut short by `ctrl-c`: the counters then
    /// cover only what had finished, the downloads still in flight being
    /// neither completed nor failed - they are left as `.part` files to resume.
    pub fn summary_lines(&self, interrupted: bool) -> Vec<String> {
        Self::format_summary(
            self.completed.load(Ordering::Relaxed),
            self.skipped.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
            self.downloaded_bytes.load(Ordering::Relaxed),
            self.started_at.elapsed(),
            interrupted,
        )
    }

    /// Rendering of the report, kept free of the clock and the counters so it
    /// can be checked against exact durations.
    fn format_summary(
        completed: u64,
        skipped: u64,
        failed: u64,
        bytes: u64,
        elapsed: Duration,
        interrupted: bool,
    ) -> Vec<String> {
        // an interrupted run did not finish, and saying so beats a report that
        // reads like a clean ending
        let opening = if interrupted {
            format!("Interrupted after {}", pretty_duration(elapsed))
        } else {
            format!("Finished in {}", pretty_duration(elapsed))
        };
        let mut lines = vec![format!(
            "{opening} - {completed} completed, {skipped} skipped, {failed} failed"
        )];

        // Nothing transferred (everything skipped or failed) makes the volume
        // and speed line pure noise.
        if bytes > 0 {
            lines.push(format!(
                "Downloaded {} at an average speed of {}",
                pretty_bytes_size(bytes),
                Self::average_speed(bytes, elapsed),
            ));
        }
        lines
    }

    /// Average transfer speed over the whole run, wall clock based: it covers
    /// idle stretches such as HEAD requests and retry backoff, so it reads
    /// lower than the per-file speeds shown by the progress bars.
    fn average_speed(bytes: u64, elapsed: Duration) -> String {
        let secs = elapsed.as_secs_f64();
        if secs > 0.0 {
            // a rate in bytes/s is positive and stays far below u64::MAX
            let per_sec = (bytes as f64 / secs) as u64;
            format!("{}/s", pretty_bytes_size(per_sec))
        } else {
            // a run too short to measure would divide by zero
            "--".to_string()
        }
    }
}

#[cfg(test)]
mod run_stats_tests {
    use super::*;

    #[test]
    fn counters_start_at_zero() {
        let stats = RunStats::new();
        assert_eq!(stats.failed_count(), 0);
        assert_eq!(stats.processed_count(), 0);
        assert_eq!(stats.downloaded_bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn processed_count_sums_every_outcome() {
        let stats = RunStats::new();
        stats.record_completed();
        stats.record_completed();
        stats.record_skipped();
        stats.record_failed();
        assert_eq!(stats.processed_count(), 4);
        assert_eq!(stats.failed_count(), 1);
    }

    #[test]
    fn downloaded_bytes_accumulate_across_downloads() {
        let stats = RunStats::new();
        stats.add_downloaded_bytes(600);
        stats.add_downloaded_bytes(424);
        assert_eq!(stats.downloaded_bytes.load(Ordering::Relaxed), 1024);
    }

    /// The counters reach the rendered report - the wiring `summary_lines`
    /// does on top of `format_summary`. The duration is left out because it
    /// comes from the real clock here.
    #[test]
    fn summary_lines_render_the_live_counters() {
        let stats = RunStats::new();
        stats.record_completed();
        stats.record_skipped();
        stats.add_downloaded_bytes(2048);

        let lines = stats.summary_lines(false);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].ends_with("1 completed, 1 skipped, 0 failed"),
            "unexpected summary: {}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("Downloaded 2.00KiB at an average speed of"),
            "unexpected summary: {}",
            lines[1]
        );
    }

    /// The report must not claim success for a run that had failures - the
    /// point of issue #470, where a full progress bar hid two failed links.
    #[test]
    fn summary_spells_out_failures() {
        let lines = RunStats::format_summary(53, 0, 2, 0, Duration::from_secs(72), false);
        assert_eq!(
            lines[0],
            "Finished in 1m 12s - 53 completed, 0 skipped, 2 failed"
        );
    }

    #[test]
    fn summary_reports_volume_and_average_speed() {
        let lines =
            RunStats::format_summary(1, 0, 0, 6 * 1024 * 1024, Duration::from_secs(3), false);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1],
            "Downloaded 6.00MiB at an average speed of 2.00MiB/s"
        );
    }

    /// A run where every link was skipped transferred nothing, so the volume
    /// line is dropped rather than reporting "0B at --".
    #[test]
    fn summary_omits_volume_line_when_nothing_was_transferred() {
        let lines = RunStats::format_summary(0, 1, 0, 0, Duration::from_secs(5), false);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "Finished in 5s - 0 completed, 1 skipped, 0 failed"
        );
    }

    /// A run too short for the clock to resolve must not divide by zero.
    #[test]
    fn summary_speed_is_unknown_for_a_zero_length_run() {
        let lines = RunStats::format_summary(1, 0, 0, 1024, Duration::ZERO, false);
        assert_eq!(lines[1], "Downloaded 1.00KiB at an average speed of --");
    }

    /// A run cut short by ctrl-c must not read like a clean ending, while
    /// still accounting for the downloads that did finish and the bytes
    /// already written for the ones left to resume.
    #[test]
    fn summary_of_an_interrupted_run_says_so() {
        let lines =
            RunStats::format_summary(12, 1, 0, 3 * 1024 * 1024, Duration::from_secs(6), true);
        assert_eq!(
            lines[0],
            "Interrupted after 6s - 12 completed, 1 skipped, 0 failed"
        );
        assert_eq!(
            lines[1],
            "Downloaded 3.00MiB at an average speed of 512.00KiB/s"
        );
    }
}
