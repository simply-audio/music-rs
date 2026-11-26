//! The I/O buffer size asked of every tap's private aggregate device
//! ([`crate::set_tap_buffer_frames`]): one process-wide wish, applied to
//! each aggregate as it is made and to the ones already open when the wish
//! changes. macOS keeps a device's buffer size per process, and these
//! aggregates are private to it, so asking touches no other program.

/// A device's buffer size, as the HAL has it.
pub(crate) trait Frames<D> {
    /// Its buffer size now, in frames.
    fn get(&mut self, device: &D) -> Option<u32>;
    /// The sizes it accepts, inclusive.
    fn range(&mut self, device: &D) -> Option<(u32, u32)>;
    /// Ask for `frames`; false when it was refused.
    fn set(&mut self, device: &D, frames: u32) -> bool;
}

/// The wish, and the devices it is applied to.
pub(crate) struct Buffers<D> {
    wanted: Option<u32>,
    /// Each open device, and the size it had when it was made (macOS's
    /// default for it), which no wish brings it back to.
    open: Vec<(D, Option<u32>)>,
}

impl<D: PartialEq> Buffers<D> {
    pub(crate) const fn new() -> Buffers<D> {
        Buffers { wanted: None, open: Vec::new() }
    }

    /// Ask every device for `wanted` from now on: the open ones at once,
    /// each new one as it is made. `None` gives the open ones back the
    /// size they were made with.
    pub(crate) fn wish(&mut self, wanted: Option<u32>, hal: &mut impl Frames<D>) {
        if self.wanted == wanted {
            return;
        }
        self.wanted = wanted;
        for (device, made) in &self.open {
            if let Some(frames) = wanted.or(*made) {
                ask(hal, device, frames);
            }
        }
    }

    /// `device` was just made: remember the size it came with, and ask it
    /// for the wish, if there is one.
    pub(crate) fn opened(&mut self, device: D, hal: &mut impl Frames<D>) {
        let made = hal.get(&device);
        if let Some(frames) = self.wanted {
            ask(hal, &device, frames);
        }
        self.open.push((device, made));
    }

    /// `device` is about to be destroyed.
    pub(crate) fn closed(&mut self, device: &D) {
        self.open.retain(|(d, _)| d != device);
    }
}

/// Ask `device` for `frames`, held to the sizes it accepts, unless it has
/// them already. A refusal leaves it as it was: the tap runs either way.
fn ask<D>(hal: &mut impl Frames<D>, device: &D, frames: u32) {
    let frames = match hal.range(device) {
        Some((lo, hi)) if lo <= hi => frames.clamp(lo, hi),
        _ => frames,
    };
    if hal.get(device) != Some(frames) {
        hal.set(device, frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Devices by number, each with a size and a range; every set recorded.
    #[derive(Default)]
    struct Fake {
        frames: HashMap<u32, u32>,
        range: Option<(u32, u32)>,
        refuse: bool,
        sets: Vec<(u32, u32)>,
    }

    impl Fake {
        fn make(&mut self, device: u32) -> u32 {
            self.frames.insert(device, 512);
            device
        }
    }

    impl Frames<u32> for Fake {
        fn get(&mut self, device: &u32) -> Option<u32> {
            self.frames.get(device).copied()
        }
        fn range(&mut self, _: &u32) -> Option<(u32, u32)> {
            self.range
        }
        fn set(&mut self, device: &u32, frames: u32) -> bool {
            self.sets.push((*device, frames));
            if !self.refuse {
                self.frames.insert(*device, frames);
            }
            !self.refuse
        }
    }

    #[test]
    fn with_no_wish_a_device_keeps_macos_s_default() {
        let (mut hal, mut b) = (Fake::default(), Buffers::new());
        let d = hal.make(1);
        b.opened(d, &mut hal);
        assert!(hal.sets.is_empty());
        assert_eq!(hal.get(&1), Some(512));
    }

    #[test]
    fn a_device_made_after_the_wish_gets_it() {
        let (mut hal, mut b) = (Fake::default(), Buffers::new());
        b.wish(Some(128), &mut hal);
        assert!(hal.sets.is_empty(), "nothing open, nothing asked");
        let d = hal.make(1);
        b.opened(d, &mut hal);
        assert_eq!(hal.get(&1), Some(128));
    }

    #[test]
    fn the_open_devices_follow_the_wish_and_go_back_to_their_own_size() {
        let (mut hal, mut b) = (Fake::default(), Buffers::new());
        for d in [1, 2] {
            let d = hal.make(d);
            b.opened(d, &mut hal);
        }
        b.wish(Some(256), &mut hal);
        assert_eq!((hal.get(&1), hal.get(&2)), (Some(256), Some(256)));
        b.wish(None, &mut hal);
        assert_eq!((hal.get(&1), hal.get(&2)), (Some(512), Some(512)));
    }

    #[test]
    fn the_same_wish_twice_asks_nothing_more() {
        let (mut hal, mut b) = (Fake::default(), Buffers::new());
        let d = hal.make(1);
        b.opened(d, &mut hal);
        b.wish(Some(1024), &mut hal);
        b.wish(Some(1024), &mut hal);
        assert_eq!(hal.sets, [(1, 1024)]);
    }

    #[test]
    fn a_device_already_at_the_size_is_not_asked() {
        let (mut hal, mut b) = (Fake::default(), Buffers::new());
        b.wish(Some(512), &mut hal);
        let d = hal.make(1);
        b.opened(d, &mut hal);
        assert!(hal.sets.is_empty());
    }

    #[test]
    fn the_wish_is_held_to_what_the_device_accepts() {
        let mut hal = Fake { range: Some((64, 512)), ..Fake::default() };
        let mut b = Buffers::new();
        let d = hal.make(1);
        b.opened(d, &mut hal);
        b.wish(Some(4096), &mut hal);
        assert_eq!(hal.get(&1), Some(512), "already at the most it takes");
        b.wish(Some(16), &mut hal);
        assert_eq!(hal.get(&1), Some(64));
    }

    #[test]
    fn a_closed_device_is_not_asked_again() {
        let (mut hal, mut b) = (Fake::default(), Buffers::new());
        let d = hal.make(1);
        b.opened(d, &mut hal);
        b.closed(&1);
        b.wish(Some(128), &mut hal);
        assert!(hal.sets.is_empty());
    }

    #[test]
    fn a_refusal_leaves_the_device_as_it_was() {
        let mut hal = Fake { refuse: true, ..Fake::default() };
        let mut b = Buffers::new();
        b.wish(Some(128), &mut hal);
        let d = hal.make(1);
        b.opened(d, &mut hal);
        assert_eq!(hal.sets, [(1, 128)]);
        assert_eq!(hal.get(&1), Some(512));
    }
}
