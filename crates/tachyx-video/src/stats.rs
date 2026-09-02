use std::collections::VecDeque;
use std::time::Duration;

/// How many recent frames a summary is taken over.
const WINDOW: usize = 512;

/// A latency distribution over the last few hundred frames, in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Latency {
    /// The fastest frame in the window.
    pub min_us: u32,
    /// The middle frame in the window.
    pub median_us: u32,
    /// The frame at the ninety-fifth percentile of the window.
    pub p95_us: u32,
    /// The slowest frame in the window.
    pub max_us: u32,
}

/// What the present path has done since the pipeline started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Access units handed to the decoder.
    pub pushed: u64,
    /// Frames the decoder produced as textures.
    pub decoded: u64,
    /// Frames handed to Flutter.
    pub presented: u64,
    /// Frames a newer one replaced before Flutter asked for a texture.
    pub superseded: u64,
    /// From `push` to the frame leaving the pipeline.
    pub decode: Latency,
    /// From `push` to the frame being handed to Flutter's rasteriser.
    pub present: Latency,
}

/// A rolling window of one measurement.
///
/// The samples are held in two orders at once. Every number a summary reports
/// is an order statistic, so filing each sample where it belongs as it arrives
/// makes the summary four lookups instead of a sort of the whole window. The
/// frame that arrives pays for it, on the thread that decoded it, rather than
/// the thread that draws paying every time it puts a number on the screen.
#[derive(Debug, Default)]
pub(crate) struct Window {
    /// Oldest first, which is what says which sample leaves next.
    arrival: VecDeque<u32>,
    /// The same samples, smallest first.
    sorted: Vec<u32>,
}

impl Window {
    pub(crate) fn record(&mut self, elapsed: Duration) {
        let sample = u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX);
        if self.arrival.len() == WINDOW {
            let oldest = self
                .arrival
                .pop_front()
                .expect("a full window holds a sample");
            // Equal samples are interchangeable, so any of them is the one that
            // left.
            let filed = self
                .sorted
                .binary_search(&oldest)
                .expect("every sample in the window is filed");
            self.sorted.remove(filed);
        }
        self.arrival.push_back(sample);
        let at = self.sorted.partition_point(|filed| *filed < sample);
        self.sorted.insert(at, sample);
    }

    pub(crate) fn summary(&self) -> Latency {
        let Some(last) = self.sorted.len().checked_sub(1) else {
            return Latency::default();
        };
        Latency {
            min_us: self.sorted[0],
            median_us: self.sorted[last * 50 / 100],
            p95_us: self.sorted[last * 95 / 100],
            max_us: self.sorted[last],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Latency, WINDOW, Window};
    use std::time::Duration;

    #[test]
    fn an_empty_window_summarises_to_zero() {
        assert_eq!(Window::default().summary(), Latency::default());
    }

    #[test]
    fn a_summary_reports_the_ends_and_the_middle() {
        let mut window = Window::default();
        for micros in 1..=100 {
            window.record(Duration::from_micros(micros));
        }
        let summary = window.summary();
        assert_eq!(summary.min_us, 1);
        assert_eq!(summary.max_us, 100);
        assert_eq!(summary.median_us, 50);
        assert_eq!(summary.p95_us, 95);
    }

    #[test]
    fn the_filed_order_follows_what_the_window_evicts() {
        // The two orders disagree on which sample leaves: the window drops the
        // oldest, and the file has to lose that one and not the smallest.
        let length = u64::try_from(WINDOW).expect("a window of a few hundred");
        let mut window = Window::default();
        for micros in 1..=length + 100 {
            window.record(Duration::from_micros(micros));
        }
        let summary = window.summary();
        assert_eq!(summary.min_us, 101, "the first hundred were forgotten");
        assert_eq!(u64::from(summary.max_us), length + 100);
        assert_eq!(
            u64::from(summary.median_us),
            101 + (length - 1) * 50 / 100,
            "the middle of what is left, not of what was recorded"
        );
    }

    #[test]
    fn a_window_forgets_everything_older_than_its_length() {
        let mut window = Window::default();
        for _ in 0..WINDOW {
            window.record(Duration::from_micros(9999));
        }
        for _ in 0..WINDOW {
            window.record(Duration::from_micros(10));
        }
        assert_eq!(window.summary().max_us, 10);
    }
}
