//! Several captures into one sink, where the OS doesn't mix them: on
//! Windows one process-loopback client takes one process tree, so a tap of
//! several apps is several clients, summed here.
//!
//! Each client pushes from its own thread; [`Mix::push`] queues what it
//! pushed and hands the sink every frame all the live sources have
//! delivered, summed. A source that stops delivering (its app paused, its
//! client gone) is left out after [`LIVE`], so it never holds the others
//! up, and no queue grows past [`MAX_QUEUED`]. With one source, frames pass
//! straight through. Pure: tested off Windows.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a source counts as live after it last delivered.
pub(crate) const LIVE: Duration = Duration::from_millis(50);
/// The most a source may run ahead of the slowest live one before its
/// oldest frames are dropped.
pub(crate) const MAX_QUEUED: Duration = Duration::from_millis(100);

struct Source {
    queue: VecDeque<f32>,
    /// Host time of the queue's first frame (0 = unknown).
    ns: u64,
    last: Option<Instant>,
}

pub(crate) struct Mix<S: FnMut(&[f32], u64)> {
    sink: S,
    channels: usize,
    rate: u32,
    sources: Vec<Source>,
    /// The most frames a queue holds.
    max: usize,
    out: Vec<f32>,
}

impl<S: FnMut(&[f32], u64)> Mix<S> {
    /// `sources` captures, each `channels` wide at `rate`, into `sink`.
    pub(crate) fn new(sink: S, sources: usize, channels: usize, rate: u32) -> Mix<S> {
        let channels = channels.max(1);
        let max = (rate as f64 * MAX_QUEUED.as_secs_f64()).ceil() as usize;
        let queued = if sources > 1 { max * channels } else { 0 };
        Mix {
            sink,
            channels,
            rate,
            sources: (0..sources.max(1))
                .map(|_| Source { queue: VecDeque::with_capacity(queued), ns: 0, last: None })
                .collect(),
            max,
            out: vec![0.0; queued],
        }
    }

    /// The sink back.
    pub(crate) fn into_sink(self) -> S {
        self.sink
    }

    /// Source `i` captured `frames` (interleaved), the first at host time
    /// `ns` (0 = unknown), and it is `now`.
    pub(crate) fn push(&mut self, i: usize, frames: &[f32], ns: u64, now: Instant) {
        if self.sources.len() == 1 {
            (self.sink)(frames, ns);
            return;
        }
        let ch = self.channels;
        let frame_ns = 1e9 / self.rate as f64;
        let Some(src) = self.sources.get_mut(i) else { return };
        if src.queue.is_empty() {
            src.ns = ns;
        }
        src.queue.extend(frames.iter().copied());
        let over = (src.queue.len() / ch).saturating_sub(self.max);
        if over > 0 {
            src.queue.drain(..over * ch);
            if src.ns != 0 {
                src.ns += (over as f64 * frame_ns) as u64;
            }
        }
        src.last = Some(now);

        let live = |s: &Source| s.last.is_some_and(|t| now.saturating_duration_since(t) < LIVE);
        let ready =
            self.sources.iter().filter(|s| live(s)).map(|s| s.queue.len() / ch).min().unwrap_or(0);
        if ready == 0 {
            return;
        }
        let at = self.sources.iter().find(|s| live(s) && s.ns != 0).map_or(0, |s| s.ns);
        let out = &mut self.out[..ready * ch];
        out.fill(0.0);
        for s in &mut self.sources {
            let take = (s.queue.len() / ch).min(ready);
            for (o, v) in out.iter_mut().zip(s.queue.drain(..take * ch)) {
                *o += v;
            }
            if s.ns != 0 {
                s.ns += (take as f64 * frame_ns) as u64;
            }
        }
        (self.sink)(out, at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Got = Rc<RefCell<Vec<(Vec<f32>, u64)>>>;

    fn mix(sources: usize) -> (Mix<impl FnMut(&[f32], u64)>, Got) {
        let got: Got = Default::default();
        let g = Rc::clone(&got);
        let sink = move |f: &[f32], ns: u64| g.borrow_mut().push((f.to_vec(), ns));
        (Mix::new(sink, sources, 1, 1000), got)
    }

    #[test]
    fn one_source_passes_straight_through() {
        let (mut m, got) = mix(1);
        m.push(0, &[0.5, 0.25], 7, Instant::now());
        assert_eq!(*got.borrow(), vec![(vec![0.5, 0.25], 7)]);
    }

    #[test]
    fn two_sources_are_summed_once_both_have_delivered() {
        let (mut m, got) = mix(2);
        let t = Instant::now();
        m.push(0, &[0.5, 0.5, 0.5], 1_000_000_000, t);
        // Only one has delivered (the other isn't live yet): straight on.
        assert_eq!(got.borrow().len(), 1);
        m.push(1, &[0.25, 0.25], 1_000_000_000, t);
        m.push(0, &[0.5, 0.5], 1_003_000_000, t);
        m.push(1, &[0.25, 0.25], 1_002_000_000, t);
        let got = got.borrow();
        let summed: Vec<f32> = got[1..].iter().flat_map(|(f, _)| f.clone()).collect();
        assert!(summed.iter().all(|v| *v == 0.75 || *v == 0.5), "{summed:?}");
        assert!(summed.contains(&0.75));
    }

    #[test]
    fn a_source_that_stops_delivering_does_not_hold_the_others_up() {
        let (mut m, got) = mix(2);
        let handed = || got.borrow().iter().map(|(f, _)| f.len()).sum::<usize>();
        let t = Instant::now();
        m.push(0, &[0.5; 10], 0, t);
        m.push(1, &[0.25; 10], 0, t);
        m.push(0, &[0.5; 10], 0, t + LIVE / 2);
        assert_eq!(handed(), 20);
        // Source 1 has gone quiet: while it is live, 0 waits for it…
        m.push(0, &[0.5; 10], 0, t + LIVE / 2);
        assert_eq!(handed(), 20);
        // …and once it isn't, 0 carries on alone.
        m.push(0, &[0.5; 10], 0, t + LIVE * 2);
        assert_eq!(handed(), 40);
    }

    #[test]
    fn no_queue_grows_past_its_limit() {
        let (mut m, got) = mix(2);
        let t = Instant::now();
        m.push(1, &[0.0; 1], 0, t);
        m.push(1, &[], 0, t);
        // Source 1 is live with nothing queued: source 0 queues, capped.
        for _ in 0..50 {
            m.push(0, &[0.5; 10], 0, t);
        }
        let cap = (1000.0 * MAX_QUEUED.as_secs_f64()) as usize;
        assert!(m.sources[0].queue.len() <= cap, "{}", m.sources[0].queue.len());
        drop(got);
    }

    #[test]
    fn the_stamp_is_the_first_frame_handed_on() {
        let (mut m, got) = mix(2);
        let t = Instant::now();
        m.push(0, &[0.1; 4], 2_000_000_000, t);
        m.push(1, &[0.1; 2], 2_001_000_000, t);
        let got = got.borrow();
        // Source 0 alone handed on its 4 frames first, stamped as captured.
        assert_eq!(got[0].1, 2_000_000_000);
    }
}
