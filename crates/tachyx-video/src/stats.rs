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
#[derive(Debug, Default)]
pub(crate) struct Window(VecDeque<u32>);

impl Window {
    pub(crate) fn record(&mut self, elapsed: Duration) {
        if self.0.len() == WINDOW {
            self.0.pop_front();
        }
        self.0
            .push_back(u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX));
    }

    pub(crate) fn summary(&self) -> Latency {
        if self.0.is_empty() {
            return Latency::default();
        }
        let mut sorted: Vec<u32> = self.0.iter().copied().collect();
        sorted.sort_unstable();
        let at = |fraction: usize| sorted[(sorted.len() - 1) * fraction / 100];
        Latency {
            min_us: sorted[0],
            median_us: at(50),
            p95_us: at(95),
            max_us: sorted[sorted.len() - 1],
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
