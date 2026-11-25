//! What wakes a follower thread, kept free of the HAL so it is tested with
//! fakes: [`Wake`] (poked by listeners, or its tick passing), the thread
//! that ticks on it ([`spawn`]), and [`Listened`], which keeps listeners on
//! each process and reads the process list again only when one of them, or
//! the follower's own listeners, said something changed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::apps::{Backend, Mute, Proc, Target};
use crate::{Error, ErrorKind, Sink};

/// Wakes a follower: from HAL listeners (a process came or went or moved,
/// a device list changed) or from its owner (stop); else every `tick`.
pub(crate) struct Wake {
    flags: Mutex<(bool, bool)>, // (pending, stop)
    ready: Condvar,
    tick: Duration,
    /// How many times it was poked: a count that moved since the process
    /// list was read means the list may be out of date.
    pokes: AtomicU64,
    /// One of the follower's own listeners could not be registered, so a
    /// change it would have announced may go unannounced.
    unheard: AtomicBool,
}

impl Wake {
    pub(crate) fn new(tick: Duration) -> Wake {
        Wake {
            flags: Mutex::new((false, false)),
            ready: Condvar::new(),
            tick,
            pokes: AtomicU64::new(0),
            unheard: AtomicBool::new(false),
        }
    }

    pub(crate) fn poke(&self, stop: bool) {
        self.pokes.fetch_add(1, Ordering::AcqRel);
        let mut f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        f.0 = true;
        f.1 |= stop;
        self.ready.notify_all();
    }

    /// How many times it was poked so far.
    pub(crate) fn pokes(&self) -> u64 {
        self.pokes.load(Ordering::Acquire)
    }

    /// A listener the follower relies on could not be registered.
    pub(crate) fn set_unheard(&self) {
        self.unheard.store(true, Ordering::Release);
    }

    fn unheard(&self) -> bool {
        self.unheard.load(Ordering::Acquire)
    }

    /// Until poked or the tick passes; true = stop.
    fn wait(&self) -> bool {
        let mut f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        if !f.0 {
            f = self.ready.wait_timeout(f, self.tick).unwrap_or_else(|e| e.into_inner()).0;
        }
        f.0 = false;
        f.1
    }
}

/// Run `tick` on a thread called `name`: once, then on every poke of
/// `wake` (or its tick), until it is poked to stop. Returns after the
/// first tick: its error is returned (and the thread ends); later ones are
/// the follower's own to report. Dropping `tick` at the end drops the
/// follower, on its own thread.
pub(crate) fn spawn<T>(name: &str, wake: Arc<Wake>, mut tick: T) -> Result<JoinHandle<()>, Error>
where
    T: FnMut(Instant) -> Result<(), Error> + Send + 'static,
{
    let (first_tx, first_rx) = mpsc::channel::<Result<(), Error>>();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let first = tick(Instant::now());
            let failed = first.is_err();
            let _ = first_tx.send(first);
            if failed {
                return;
            }
            while !wake.wait() {
                let _ = tick(Instant::now());
            }
        })
        .map_err(|e| {
            Error::new(ErrorKind::Device, format!("cannot start the {name} thread: {e}"))
        })?;
    match first_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::new(ErrorKind::Device, format!("the {name} thread died before reporting")))
        }
    }
}

/// A process object property whose change wakes an app follower.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Prop {
    /// It started or stopped its IO (`kAudioProcessPropertyIsRunning`): a
    /// new app's channel starts at once.
    Playing,
    /// It moved to other devices, or started or stopped using one
    /// (`kAudioProcessPropertyDevices`).
    Devices,
}

/// Listeners on process object properties: on the HAL, the real ones; in
/// the tests, a fake.
pub(crate) trait Listen {
    /// Start poking the follower when `object`'s `prop` changes. False:
    /// the listener could not be registered.
    fn add(&mut self, object: u32, prop: Prop) -> bool;
    /// Stop (the process is gone, or the follower is).
    fn remove(&mut self, object: u32, prop: Prop);
}

/// What every listed process is listened to for.
const PROPS: [Prop; 2] = [Prop::Playing, Prop::Devices];

/// The process list as last read, and the pid translations made since.
struct Seen {
    /// [`Wake::pokes`] when it was read; `None`: read it again next look
    /// (listeners were just added, and a change before they were may have
    /// gone unannounced).
    pokes: Option<u64>,
    at: Instant,
    procs: Vec<Proc>,
    pids: HashMap<i32, Option<u32>>,
}

/// A [`Backend`] whose listed processes each carry [`Listen`]ers on every
/// [`Prop`], kept in line with the list and removed with the follower. Both
/// are needed even by a tap that never reads the devices a process plays
/// into: a process whose input keeps it running as its output stops is
/// announced only by its devices changing.
///
/// It reads the process list (and translates pids) again only when the
/// follower's [`Wake`] was poked since the last read: a look in between
/// costs the HAL nothing. Where a listener could not be registered, on a
/// process or on the system object ([`Wake::set_unheard`]), it also reads
/// again once `fallback` has passed since the last read.
pub(crate) struct Listened<B, L: Listen> {
    inner: B,
    listen: L,
    wake: Arc<Wake>,
    fallback: Duration,
    /// The listeners registered, by process object.
    on: BTreeMap<u32, BTreeSet<Prop>>,
    seen: Option<Seen>,
}

impl<B: Backend, L: Listen> Listened<B, L> {
    pub(crate) fn new(inner: B, listen: L, wake: Arc<Wake>, fallback: Duration) -> Listened<B, L> {
        Listened { inner, listen, wake, fallback, on: BTreeMap::new(), seen: None }
    }

    /// Some change may go unannounced: a listener is missing.
    fn unheard(&self) -> bool {
        self.wake.unheard() || self.on.values().any(|p| p.len() < PROPS.len())
    }

    fn stale(&self, now: Instant) -> bool {
        let Some(seen) = &self.seen else { return true };
        seen.pokes != Some(self.wake.pokes())
            || (self.unheard() && now.saturating_duration_since(seen.at) >= self.fallback)
    }

    /// Bring the listeners in line with `procs`, retrying any that could
    /// not be registered before. True if one was added.
    fn sync(&mut self, procs: &[Proc]) -> bool {
        let now: BTreeSet<u32> = procs.iter().map(|p| p.object).collect();
        let gone: Vec<u32> = self.on.keys().filter(|o| !now.contains(o)).copied().collect();
        for object in gone {
            for prop in self.on.remove(&object).unwrap_or_default() {
                self.listen.remove(object, prop);
            }
        }
        let mut added = false;
        for object in now {
            let on = self.on.entry(object).or_default();
            for prop in PROPS {
                if !on.contains(&prop) && self.listen.add(object, prop) {
                    on.insert(prop);
                    added = true;
                }
            }
        }
        added
    }
}

impl<B: Backend, L: Listen> Backend for Listened<B, L> {
    type Tap = B::Tap;
    fn processes(&mut self) -> Vec<Proc> {
        let now = Instant::now();
        if self.stale(now) {
            let pokes = self.wake.pokes();
            let procs = self.inner.processes();
            let added = self.sync(&procs);
            let pokes = (!added).then_some(pokes);
            self.seen = Some(Seen { pokes, at: now, procs, pids: HashMap::new() });
        }
        self.seen.as_ref().map(|s| s.procs.clone()).unwrap_or_default()
    }
    fn translate(&mut self, pid: i32) -> Option<u32> {
        let Listened { inner, seen, .. } = self;
        match seen {
            Some(s) => *s.pids.entry(pid).or_insert_with(|| inner.translate(pid)),
            None => inner.translate(pid),
        }
    }
    fn open(
        &mut self,
        targets: &[Target],
        mute: Mute,
        sink: Sink,
    ) -> Result<(B::Tap, u32), (Error, Sink)> {
        self.inner.open(targets, mute, sink)
    }
    fn close(&mut self, tap: B::Tap) -> Sink {
        self.inner.close(tap)
    }
    fn health(&mut self, tap: &B::Tap, playing: bool) -> Option<String> {
        self.inner.health(tap, playing)
    }
}

impl<B, L: Listen> Drop for Listened<B, L> {
    fn drop(&mut self) {
        for (&object, props) in &self.on {
            for &prop in props {
                self.listen.remove(object, prop);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::tests::*;
    use crate::apps::{App, AppTap, Follow};
    use std::collections::BTreeMap;

    /// The fake HAL's listener table: whom to wake when a process's
    /// property changes. Shared between threads, as the HAL's is.
    #[derive(Clone, Default)]
    struct Table(Arc<Mutex<Listeners>>);

    /// Whom each process's property wakes.
    type Listeners = BTreeMap<(u32, Prop), Arc<Wake>>;

    struct FakeListen {
        table: Table,
        wake: Arc<Wake>,
        /// Processes that refuse a listener.
        refuse: Vec<u32>,
    }

    impl FakeListen {
        fn new(table: &Table, wake: &Arc<Wake>) -> FakeListen {
            FakeListen { table: table.clone(), wake: Arc::clone(wake), refuse: Vec::new() }
        }
    }

    impl Listen for FakeListen {
        fn add(&mut self, object: u32, prop: Prop) -> bool {
            if self.refuse.contains(&object) {
                return false;
            }
            self.table.0.lock().unwrap().insert((object, prop), Arc::clone(&self.wake));
            true
        }
        fn remove(&mut self, object: u32, prop: Prop) {
            self.table.0.lock().unwrap().remove(&(object, prop));
        }
    }

    impl Table {
        /// The HAL says `object`'s `prop` changed.
        fn changed(&self, object: u32, prop: Prop) {
            if let Some(w) = self.0.lock().unwrap().get(&(object, prop)) {
                w.poke(false);
            }
        }
        /// The processes listened to on `prop`.
        fn listened(&self, prop: Prop) -> Vec<u32> {
            self.0.lock().unwrap().keys().filter(|k| k.1 == prop).map(|k| k.0).collect()
        }
    }

    /// The fake backend crosses to the follower thread; its script stays
    /// readable here.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Script>>);

    impl Backend for Shared {
        type Tap = u32;
        fn processes(&mut self) -> Vec<Proc> {
            let mut s = self.0.lock().unwrap();
            s.reads += 1;
            s.procs.clone()
        }
        fn translate(&mut self, pid: i32) -> Option<u32> {
            let mut s = self.0.lock().unwrap();
            s.translations += 1;
            s.procs.iter().find(|p| p.pid == pid).map(|p| p.object)
        }
        fn open(
            &mut self,
            t: &[Target],
            mute: Mute,
            sink: Sink,
        ) -> Result<(u32, u32), (Error, Sink)> {
            let mut s = self.0.lock().unwrap();
            s.opened.push((t.to_vec(), mute));
            s.next += 1;
            s.held = Some(sink);
            Ok((s.next, 48_000))
        }
        fn close(&mut self, _: u32) -> Sink {
            self.0.lock().unwrap().held.take().expect("a tap holds the sink")
        }
        fn health(&mut self, _: &u32, _: bool) -> Option<String> {
            None
        }
    }

    #[derive(Default)]
    struct Script {
        procs: Vec<Proc>,
        opened: Vec<(Vec<Target>, Mute)>,
        next: u32,
        held: Option<Sink>,
        /// How many times the process list was read, and pids translated.
        reads: usize,
        translations: usize,
    }

    impl Shared {
        fn reads(&self) -> usize {
            self.0.lock().unwrap().reads
        }
    }

    /// A fallback far longer than a test may take.
    const NO_FALLBACK: Duration = Duration::from_secs(600);

    fn tussen_tap() -> AppTap {
        AppTap::all_except([App::This, App::PlayingInto(TUSSEN.into())])
    }

    /// A poll far longer than a test may take: only a listener can wake
    /// the follower in time.
    fn no_poll() -> Arc<Wake> {
        Arc::new(Wake::new(Duration::from_secs(600)))
    }

    /// Until `done` holds, or fail with `why` after 5 s.
    fn soon(why: &str, done: impl Fn() -> bool) {
        let t = Instant::now();
        while !done() {
            assert!(t.elapsed() < Duration::from_secs(5), "{why}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_process_moving_to_a_tussen_device_wakes_the_follower_without_waiting_for_the_poll() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        let music = proc(3, 5, MUSIC, "/x/Music");
        backend.0.lock().unwrap().procs = vec![proc(10, 412, DJAY, DJAY_EXE), music.clone()];
        let listen = FakeListen::new(&table, &wake);
        let listened = Listened::new(backend.clone(), listen, Arc::clone(&wake), NO_FALLBACK);
        let mut follow = Follow::new(listened, tussen_tap(), ME, Box::new(|_: &[f32], _: u64| {}));
        let thread = spawn("test follower", Arc::clone(&wake), move |now| {
            follow.tick(now);
            Ok(())
        })
        .unwrap();
        assert_eq!(table.listened(Prop::Devices), [3, 10]);
        assert_eq!(backend.0.lock().unwrap().opened.len(), 1);

        // Music moves to Tussen In 1, and the HAL says so.
        backend.0.lock().unwrap().procs =
            vec![proc(10, 412, DJAY, DJAY_EXE), plays(music, &[TUSSEN_IN_1])];
        table.changed(3, Prop::Devices);
        soon("the follower was not woken", || backend.0.lock().unwrap().opened.len() == 2);
        assert_eq!(
            backend.0.lock().unwrap().opened.last().unwrap().0,
            [Target::AllExcept(vec![3])]
        );

        // Music exits: its listeners go with it (the next look sees it).
        backend.0.lock().unwrap().procs = vec![proc(10, 412, DJAY, DJAY_EXE)];
        table.changed(3, Prop::Devices);
        soon("music's listeners stayed", || {
            table.listened(Prop::Devices) == [10] && table.listened(Prop::Playing) == [10]
        });
        // Stopping drops the follower on its thread: every listener goes.
        wake.poke(true);
        thread.join().unwrap();
        assert!(table.0.lock().unwrap().is_empty());
    }

    #[test]
    fn an_app_that_starts_playing_gets_its_channel_without_waiting_for_the_poll() {
        use crate::channels::{AppSinks, Split};
        struct Quiet;
        impl AppSinks for Quiet {
            type Sink = fn(&[f32], u64);
            fn started(&mut self, _: &crate::AppInfo) -> Self::Sink {
                |_, _| {}
            }
            fn stopped(&mut self, _: &crate::AppInfo) {}
        }
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        let music = proc(3, 5, MUSIC, "/x/Music");
        backend.0.lock().unwrap().procs = vec![Proc { playing: false, ..music.clone() }];
        let tap = AppTap::all_except([App::This]);
        let listen = FakeListen::new(&table, &wake);
        let listened = Listened::new(backend.clone(), listen, Arc::clone(&wake), NO_FALLBACK);
        let mut split = Split::new(listened, tap, ME, Quiet);
        let thread = spawn("test channels", Arc::clone(&wake), move |now| {
            split.tick(now);
            Ok(())
        })
        .unwrap();
        // Listened to for both, though nothing reads its devices.
        assert_eq!(table.listened(Prop::Playing), [3]);
        assert_eq!(table.listened(Prop::Devices), [3]);
        assert!(backend.0.lock().unwrap().opened.is_empty());

        // Music starts playing, and the HAL says so.
        backend.0.lock().unwrap().procs = vec![music];
        table.changed(3, Prop::Playing);
        soon("the follower was not woken", || backend.0.lock().unwrap().opened.len() == 1);
        assert_eq!(backend.0.lock().unwrap().opened[0].0, [Target::Processes(vec![3])]);
        wake.poke(true);
        thread.join().unwrap();
        assert!(table.0.lock().unwrap().is_empty());
    }

    /// A follower over `backend` whose listeners refuse `refuse`, falling
    /// back after `fallback`; ticked by hand.
    fn follower(
        backend: &Shared,
        table: &Table,
        wake: &Arc<Wake>,
        refuse: Vec<u32>,
        fallback: Duration,
    ) -> Follow<Listened<Shared, FakeListen>> {
        let tap = AppTap::all_except([App::This]);
        let listen = FakeListen { refuse, ..FakeListen::new(table, wake) };
        let listened = Listened::new(backend.clone(), listen, Arc::clone(wake), fallback);
        Follow::new(listened, tap, ME, Box::new(|_: &[f32], _: u64| {}))
    }

    #[test]
    fn a_look_reads_the_process_list_only_after_a_listener_spoke() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        let music = proc(3, 5, MUSIC, "/x/Music");
        backend.0.lock().unwrap().procs = vec![Proc { playing: false, ..music.clone() }];
        // No fallback wait at all: it would read on every look if it
        // thought a change could go unannounced.
        let mut follow = follower(&backend, &table, &wake, vec![], Duration::ZERO);
        follow.tick(Instant::now());
        // Listeners were just added: the next look reads once more, in
        // case music changed before they were.
        follow.tick(Instant::now());
        assert_eq!(backend.reads(), 2);
        let translations = backend.0.lock().unwrap().translations;
        for _ in 0..20 {
            follow.tick(Instant::now());
        }
        assert_eq!(backend.reads(), 2, "a quiet look read the process list");
        assert_eq!(backend.0.lock().unwrap().translations, translations);

        // Music starts playing, and the HAL says so: the next look sees it.
        backend.0.lock().unwrap().procs = vec![music];
        table.changed(3, Prop::Playing);
        follow.tick(Instant::now());
        assert_eq!(backend.reads(), 3);
        follow.tick(Instant::now());
        assert_eq!(backend.reads(), 3);
    }

    #[test]
    fn a_process_that_refuses_a_listener_is_read_again_after_the_fallback() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        backend.0.lock().unwrap().procs = vec![proc(3, 5, MUSIC, "/x/Music")];
        let fallback = Duration::from_millis(50);
        let mut follow = follower(&backend, &table, &wake, vec![3], fallback);
        follow.tick(Instant::now());
        follow.tick(Instant::now());
        assert_eq!(backend.reads(), 1, "read again before the fallback passed");
        std::thread::sleep(fallback);
        follow.tick(Instant::now());
        assert_eq!(backend.reads(), 2);
        assert!(table.listened(Prop::Playing).is_empty());
    }

    #[test]
    fn a_follower_whose_own_listener_failed_reads_again_after_the_fallback() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        backend.0.lock().unwrap().procs = vec![proc(3, 5, MUSIC, "/x/Music")];
        wake.set_unheard();
        let mut follow = follower(&backend, &table, &wake, vec![], Duration::ZERO);
        for _ in 0..5 {
            follow.tick(Instant::now());
        }
        assert_eq!(backend.reads(), 5);
    }
}
