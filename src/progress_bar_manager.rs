use crate::DlmError;
use async_channel::{Receiver, Sender};
use indicatif::{
    HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState,
    ProgressStyle,
};
use jiff::Zoned;
use std::cmp::{Ordering, min};

const PENDING: &str = "pending";

/// The transfer rate to display, or `None` when there is no credible estimate.
///
/// The rate estimator restarts from zero every time it is reset - once when
/// the resumed offset is primed, once when the first byte arrives - and ramps
/// up over the following samples. The values it reports on the way up are
/// below one byte per second, which renders as `0 B/s` and, worse, turns into
/// an ETA of thousands of years. A rate that rounds down to zero bytes is not
/// a measurement, so it is reported as "not known yet" instead.
fn known_speed(per_sec: f64) -> Option<u64> {
    if per_sec.is_finite() && per_sec >= 1.0 {
        Some(per_sec as u64)
    } else {
        None
    }
}

pub struct ProgressBarManager {
    main_pb: ProgressBar,
    file_pb_count: u64,
    tx: Sender<ProgressBar>,
    rx: Receiver<ProgressBar>,
}

impl ProgressBarManager {
    pub async fn init(max_concurrent_downloads: u32, main_pb_len: u64) -> Self {
        let mp = MultiProgress::new();
        // Refresh terminal 5 times per seconds
        let draw_target = ProgressDrawTarget::stdout_with_hz(5);
        mp.set_draw_target(draw_target);

        // main progress bar
        let main_style = ProgressStyle::default_bar()
            .template("{bar:133} {pos}/{len}")
            .expect("templating should not fail");
        let main_pb = mp.add(ProgressBar::new(0));
        main_pb.set_style(main_style);
        main_pb.set_length(main_pb_len);

        // `file_pb_count` progress bars are shared between the threads at anytime
        let file_pb_count = min(u64::from(max_concurrent_downloads), main_pb_len);

        let (tx, rx): (Sender<ProgressBar>, Receiver<ProgressBar>) =
            async_channel::bounded(file_pb_count as usize);

        let dl_style = ProgressStyle::default_bar()
            .template("{msg} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} (speed:{bytes_per_sec}) (eta:{eta})")
            .expect("templating should not fail")
            // Until the first byte is received the estimator has no data and the
            // default `{bytes_per_sec}`/`{eta}` would show `0/s` and `eta:0s`,
            // which misleadingly reads as "almost done" while the request is
            // still connecting. Render `--` whenever no real speed is known
            // (before the first byte, and during long stalls).
            .with_key("bytes_per_sec", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                match known_speed(state.per_sec()) {
                    Some(per_sec) => write!(w, "{}/s", HumanBytes(per_sec)).unwrap(),
                    None => write!(w, "--").unwrap(),
                }
            })
            .with_key("eta", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                // The ETA is derived from the same estimate, so it is only
                // worth showing when that estimate is.
                match known_speed(state.per_sec()) {
                    Some(_) => write!(w, "{:#}", HumanDuration(state.eta())).unwrap(),
                    None => write!(w, "--").unwrap(),
                }
            })
            .progress_chars("#>-");

        for _ in 0..file_pb_count {
            let file_pb = mp.add(ProgressBar::new(0));
            file_pb.set_style(dl_style.clone());
            file_pb.set_message(Self::message_progress_bar(PENDING));
            tx.send(file_pb).await.expect("channel should not fail");
        }

        Self {
            main_pb,
            file_pb_count,
            tx,
            rx,
        }
    }

    pub async fn finish_all(&self) -> Result<(), DlmError> {
        for _ in 0..self.file_pb_count {
            let pb = self.rx.recv().await?;
            pb.finish_and_clear();
        }
        self.main_pb.finish();
        Ok(())
    }

    pub fn increment_global_progress(&self) {
        self.main_pb.inc(1);
    }

    const PROGRESS_BAR_MSG_WIDTH: usize = 35;

    pub fn message_progress_bar(s: &str) -> String {
        let max = Self::PROGRESS_BAR_MSG_WIDTH;
        let count = s.chars().count();

        match count.cmp(&max) {
            Ordering::Greater => s.chars().take(max).collect(),
            Ordering::Equal => s.to_string(),
            Ordering::Less => format!("{}{}", s, " ".repeat(max - count)),
        }
    }

    pub fn log_above_progress_bars(&self, msg: &str) {
        Self::log_above_progress_bar(&self.main_pb, msg);
    }

    /// Print the end-of-run report, once the progress bars are finished.
    ///
    /// On a terminal it goes through the progress bars so that each line lands
    /// on its own line instead of being appended to the finished main bar.
    /// When the bars draw nowhere - stdout redirected to a file or piped into
    /// another command - indicatif would swallow the report, so it is written
    /// to stdout directly. Unlike the running logs it carries no timestamp:
    /// it describes the whole run, not a moment in it.
    pub fn print_report(&self, lines: &[String]) {
        for line in lines {
            if self.main_pb.is_hidden() {
                println!("{line}");
            } else {
                self.main_pb.println(line);
            }
        }
    }

    fn log_above_progress_bar(pb: &ProgressBar, msg: &str) {
        let now = Zoned::now().strftime("%Y-%m-%d %H:%M:%S");
        pb.println(format!("[{now}] {msg}"));
    }

    pub async fn claim_progress_bar(&self) -> ProgressBar {
        self.rx
            .recv()
            .await
            .expect("claiming progress bar should not fail")
    }

    pub async fn release_progress_bar(&self, pb: ProgressBar) {
        pb.reset();
        pb.set_message(Self::message_progress_bar(PENDING));
        self.tx
            .send(pb)
            .await
            .expect("releasing progress bar should not fail");
    }

    /// Test-only manager that draws nowhere, so unit tests can exercise code
    /// paths that log above the progress bars without touching the terminal.
    #[cfg(test)]
    pub(crate) fn hidden() -> Self {
        let mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let main_pb = mp.add(ProgressBar::hidden());
        let (tx, rx) = async_channel::bounded(1);
        Self {
            main_pb,
            file_pb_count: 0,
            tx,
            rx,
        }
    }
}

#[cfg(test)]
mod speed_display_tests {
    use super::known_speed;

    #[test]
    fn a_real_rate_is_displayed() {
        assert_eq!(known_speed(1.0), Some(1));
        assert_eq!(known_speed(1_048_576.0), Some(1_048_576));
    }

    /// The estimator ramping up after a reset reports rates that round down to
    /// zero bytes; showing them yields `0 B/s` and an ETA of millennia (#444).
    #[test]
    fn a_rate_that_rounds_to_zero_is_not_a_measurement() {
        assert_eq!(known_speed(0.0), None);
        assert_eq!(known_speed(0.9), None);
        assert_eq!(known_speed(1e-9), None);
    }

    #[test]
    fn non_finite_rates_are_rejected() {
        assert_eq!(known_speed(f64::NAN), None);
        assert_eq!(known_speed(f64::INFINITY), None);
        assert_eq!(known_speed(f64::NEG_INFINITY), None);
    }
}

#[cfg(test)]
mod recycling_tests {
    use super::*;

    /// Build a manager holding `count` real (but non-drawing) file progress
    /// bars, mirroring `init` without touching the terminal.
    async fn manager_with_bars(count: usize) -> ProgressBarManager {
        let mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let main_pb = mp.add(ProgressBar::hidden());
        let (tx, rx) = async_channel::bounded(count.max(1));
        for _ in 0..count {
            let pb = mp.add(ProgressBar::hidden());
            pb.set_message(ProgressBarManager::message_progress_bar(PENDING));
            tx.send(pb).await.unwrap();
        }
        ProgressBarManager {
            main_pb,
            file_pb_count: count as u64,
            tx,
            rx,
        }
    }

    /// A bar that already served a (resumed) download must come back from the
    /// recycle pool with no leftover position or speed, so the next download
    /// starts from a clean slate and renders `--` until its own first byte.
    #[tokio::test]
    async fn recycled_bar_carries_no_position_or_speed() {
        let mgr = manager_with_bars(1).await;

        // simulate a full download on the claimed bar
        let pb = mgr.claim_progress_bar().await;
        pb.set_length(1000);
        pb.set_position(400); // resumed offset
        pb.inc(600); // streamed to completion
        assert_eq!(pb.position(), 1000);

        // recycle it back to the pool
        mgr.release_progress_bar(pb).await;

        // the next download claims the same underlying bar
        let pb_next = mgr.claim_progress_bar().await;
        assert_eq!(
            pb_next.position(),
            0,
            "recycled bar must not carry the previous download's position"
        );
        assert_eq!(
            pb_next.per_sec(),
            0.0,
            "recycled bar must report no speed (renders as `--`) until its first byte"
        );
    }

    /// With more downloads than bars, every release must hand back a clean bar
    /// for the queued downloads to claim.
    #[tokio::test]
    async fn bar_stays_clean_across_several_recycles() {
        let mgr = manager_with_bars(1).await;

        for offset in [100_u64, 250, 700] {
            let pb = mgr.claim_progress_bar().await;
            pb.set_length(1000);
            pb.set_position(offset);
            pb.inc(1000 - offset);
            mgr.release_progress_bar(pb).await;

            let pb_check = mgr.claim_progress_bar().await;
            assert_eq!(pb_check.position(), 0);
            assert_eq!(pb_check.per_sec(), 0.0);
            mgr.release_progress_bar(pb_check).await;
        }
    }
}
