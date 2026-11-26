//! What wakes a follower thread, kept free of the HAL so it is tested with
//! fakes: [`Wake`] (poked by listeners, or its tick passing), the thread
//! that ticks on it ([`spawn`]), and [`Listened`], which keeps listeners on
//! each process and reads again only what one of them, or the follower's
//! own listeners, said changed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::apps::{Backend, Mute, Proc, Target};
use crate::{Error, ErrorKind, Sink};

/// What a listener said changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Said {
    /// Something that needs a look but no process read: a device list.
    Look,
    /// Processes came or went: the process list.
    List,
    /// A property of this process object.
    Process(u32),
}

/// What listeners said since it was last [taken](Wake::take).
#[derive(Debug, Default)]
pub(crate) struct Heard {
    /// The process list changed.
    pub list: bool,
    /// These process objects changed.
    pub objects: BTreeSet<u32>,
}

#[derive(Default)]
struct Flags {
    pending: bool,
    stop: bool,
    heard: Heard,
}

/// Wakes a follower: from HAL listeners (a process came or went or moved,
/// a device list changed) or from its owner (stop); else every `tick`.
pub(crate) struct Wake {
    flags: Mutex<Flags>,
    ready: Condvar,
    tick: Duration,
    /// How long a listener's wake waits for more to come before the look:
    /// a burst of changes costs one look.
    settle: Duration,
    /// How many times it was poked.
    pokes: AtomicU64,
    /// One of the follower's own listeners could not be registered, so a
    /// change it would have announced may go unannounced.
    unheard: AtomicBool,
}

impl Wake {
    pub(crate) fn new(tick: Duration) -> Wake {
        Wake {
            flags: Mutex::new(Flags::default()),
            ready: Condvar::new(),
            tick,
            settle: Duration::ZERO,
            pokes: AtomicU64::new(0),
            unheard: AtomicBool::new(false),
        }
    }

    /// A wake that waits `settle` after a listener's wake before the look.
    pub(crate) fn settling(self, settle: Duration) -> Wake {
        Wake { settle, ..self }
    }

    fn flags(&self) -> std::sync::MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wake the follower (to stop, with `stop`), saying nothing changed.
    pub(crate) fn poke(&self, stop: bool) {
        self.pokes.fetch_add(1, Ordering::AcqRel);
        let mut f = self.flags();
        f.pending = true;
        f.stop |= stop;
        self.ready.notify_all();
    }

    /// Wake the follower: a listener said `said`.
    pub(crate) fn heard(&self, said: Said) {
        {
            let mut f = self.flags();
            match said {
                Said::Look => {}
                Said::List => f.heard.list = true,
                Said::Process(object) => {
                    f.heard.objects.insert(object);
                }
            }
        }
        self.poke(false);
    }

    /// What listeners said since the last take.
    pub(crate) fn take(&self) -> Heard {
        std::mem::take(&mut self.flags().heard)
    }

    /// How many times it was poked so far.
    #[cfg(test)]
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

    /// Until poked (then `settle` longer, unless told to stop) or the tick
    /// passes; true = stop.
    fn wait(&self) -> bool {
        let mut f = self.flags();
        if !f.pending {
            f = self.ready.wait_timeout(f, self.tick).unwrap_or_else(|e| e.into_inner()).0;
        }
        if f.pending && !self.settle.is_zero() {
            let until = Instant::now() + self.settle;
            while !f.stop {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                f = self.ready.wait_timeout(f, left).unwrap_or_else(|e| e.into_inner()).0;
            }
        }
        f.pending = false;
        f.stop
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
    /// Start telling the follower ([`Said::Process`]) when `object`'s
    /// `prop` changes. False: the listener could not be registered.
    fn add(&mut self, object: u32, prop: Prop) -> bool;
    /// Stop (the process is gone, or the follower is).
    fn remove(&mut self, object: u32, prop: Prop);
}

/// What every listed process is listened to for.
const PROPS: [Prop; 2] = [Prop::Playing, Prop::Devices];

/// The processes as last read, and the pid translations made since the
/// process list last changed.
struct Seen {
    /// When the fallback last read what may have changed unannounced.
    at: Instant,
    procs: BTreeMap<u32, Proc>,
    pids: HashMap<i32, Option<u32>>,
}

/// A [`Backend`] whose listed processes each carry [`Listen`]ers on every
/// [`Prop`], kept in line with the list and removed with the follower. Both
/// are needed even by a tap that never reads the devices a process plays
/// into: a process whose input keeps it running as its output stops is
/// announced only by its devices changing.
///
/// It keeps the processes it read, and reads again only what a listener
/// said changed ([`Wake::take`]): a process whose property changed, or (the
/// process list changed) the list and the processes new on it; a look in
/// between costs the HAL nothing. A process is listened to before it is
/// read, so a change between the two is not missed.
///
/// Where a listener could not be registered it falls back to reading again
/// once `fallback` has passed: the processes that refused one (and their
/// listeners are tried again), or, where one on the system object was
/// ([`Wake::set_unheard`]), everything.
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

    /// Listen to `object` for every [`Prop`] it is not listened to for yet.
    fn listen_to(&mut self, object: u32) {
        let on = self.on.entry(object).or_default();
        for prop in PROPS {
            if !on.contains(&prop) && self.listen.add(object, prop) {
                on.insert(prop);
            }
        }
    }

    fn forget(&mut self, object: u32) {
        for prop in self.on.remove(&object).unwrap_or_default() {
            self.listen.remove(object, prop);
        }
    }

    /// `object` listened to, then read; `None` (and no listeners): gone.
    fn read(&mut self, object: u32) -> Option<Proc> {
        self.listen_to(object);
        let p = self.inner.process(object);
        if p.is_none() {
            self.forget(object);
        }
        p
    }

    /// Read every listed process.
    fn read_all(&mut self, now: Instant) {
        let objects = self.inner.objects();
        let gone: Vec<u32> = self.on.keys().filter(|o| !objects.contains(o)).copied().collect();
        for object in gone {
            self.forget(object);
        }
        let procs = objects.into_iter().filter_map(|o| self.read(o).map(|p| (o, p))).collect();
        self.seen = Some(Seen { at: now, procs, pids: HashMap::new() });
    }

    /// Read the process list: drop the processes gone from it, read the
    /// ones new on it (returned).
    fn read_list(&mut self, seen: &mut Seen) -> BTreeSet<u32> {
        let objects = self.inner.objects();
        let gone: Vec<u32> = seen.procs.keys().filter(|o| !objects.contains(o)).copied().collect();
        for object in gone {
            seen.procs.remove(&object);
            self.forget(object);
        }
        let new: BTreeSet<u32> =
            objects.into_iter().filter(|o| !seen.procs.contains_key(o)).collect();
        for &object in &new {
            if let Some(p) = self.read(object) {
                seen.procs.insert(object, p);
            }
        }
        seen.pids.clear();
        new
    }

    /// Read `object` again, if it is one of the processes kept.
    fn refresh(&mut self, seen: &mut Seen, object: u32) {
        if !seen.procs.contains_key(&object) {
            return;
        }
        match self.read(object) {
            Some(p) => seen.procs.insert(object, p),
            None => seen.procs.remove(&object),
        };
    }

    /// Bring the kept processes in line with what listeners said (and, once
    /// `fallback` has passed, what one that is missing would have).
    fn update(&mut self, now: Instant) {
        let heard = self.wake.take();
        let Some(mut seen) = self.seen.take() else {
            return self.read_all(now);
        };
        let due = now.saturating_duration_since(seen.at) >= self.fallback;
        if due && self.wake.unheard() {
            return self.read_all(now);
        }
        let new = if heard.list { self.read_list(&mut seen) } else { BTreeSet::new() };
        let mut again = heard.objects;
        if due {
            let deaf =
                seen.procs.keys().filter(|o| self.on.get(o).is_none_or(|p| p.len() < PROPS.len()));
            let deaf: Vec<u32> = deaf.copied().collect();
            if !deaf.is_empty() {
                again.extend(deaf);
                seen.at = now;
            }
        }
        for object in again.difference(&new) {
            self.refresh(&mut seen, *object);
        }
        self.seen = Some(seen);
    }
}

impl<B: Backend, L: Listen> Backend for Listened<B, L> {
    type Tap = B::Tap;
    fn processes(&mut self) -> Vec<Proc> {
        self.update(Instant::now());
        self.seen.as_ref().map(|s| s.procs.values().cloned().collect()).unwrap_or_default()
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
                w.heard(Said::Process(object));
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
            unreachable!("a listened backend reads the list and its processes apart")
        }
        fn objects(&mut self) -> Vec<u32> {
            let mut s = self.0.lock().unwrap();
            s.reads += 1;
            s.procs.iter().map(|p| p.object).collect()
        }
        fn process(&mut self, object: u32) -> Option<Proc> {
            let mut s = self.0.lock().unwrap();
            s.proc_reads += 1;
            s.procs.iter().find(|p| p.object == object).cloned()
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
        /// How many times the process list was read, a process was read,
        /// and a pid translated.
        reads: usize,
        proc_reads: usize,
        translations: usize,
    }

    impl Shared {
        /// (list reads, process reads) so far.
        fn cost(&self) -> (usize, usize) {
            let s = self.0.lock().unwrap();
            (s.reads, s.proc_reads)
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
    fn a_look_reads_only_what_a_listener_said_changed() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        let music = proc(3, 5, MUSIC, "/x/Music");
        backend.0.lock().unwrap().procs =
            vec![Proc { playing: false, ..music.clone() }, proc(10, 412, DJAY, DJAY_EXE)];
        // No fallback wait at all: it would read on every look if it
        // thought a change could go unannounced.
        let mut follow = follower(&backend, &table, &wake, vec![], Duration::ZERO);
        follow.tick(Instant::now());
        // The list and each process, each listened to before it was read.
        assert_eq!(backend.cost(), (1, 2));
        assert_eq!(table.listened(Prop::Playing), [3, 10]);
        let translations = backend.0.lock().unwrap().translations;
        for _ in 0..20 {
            follow.tick(Instant::now());
        }
        assert_eq!(backend.cost(), (1, 2), "a quiet look read the HAL");
        assert_eq!(backend.0.lock().unwrap().translations, translations);

        // Music starts playing, and the HAL says so: the next look reads
        // music, and only music.
        backend.0.lock().unwrap().procs[0] = music;
        table.changed(3, Prop::Playing);
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (1, 3));
        assert_eq!(backend.0.lock().unwrap().opened.last().unwrap().0, [Target::AllExcept(vec![])]);
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (1, 3));
    }

    #[test]
    fn a_process_list_change_reads_only_the_processes_that_came() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        backend.0.lock().unwrap().procs = vec![proc(3, 5, MUSIC, "/x/Music")];
        let mut follow = follower(&backend, &table, &wake, vec![], NO_FALLBACK);
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (1, 1));

        // djay comes: the list and djay are read, music is not.
        backend.0.lock().unwrap().procs.push(proc(10, 412, DJAY, DJAY_EXE));
        wake.heard(Said::List);
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (2, 2));
        assert_eq!(table.listened(Prop::Devices), [3, 10]);

        // Music goes: the list is read, no process, and its listeners go.
        backend.0.lock().unwrap().procs.remove(0);
        wake.heard(Said::List);
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (3, 2));
        assert_eq!(table.listened(Prop::Devices), [10]);
        assert_eq!(table.listened(Prop::Playing), [10]);

        // A device list change wakes it, and reads nothing.
        wake.heard(Said::Look);
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (3, 2));
    }

    /// The HAL reads one event costs among ten processes, as (list reads,
    /// process reads); printed for the record (`--nocapture`).
    #[test]
    fn an_event_among_ten_processes_reads_the_one_it_names() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        backend.0.lock().unwrap().procs =
            (1..=10).map(|o| proc(o, o as i32 + 100, MUSIC, "/x/Music")).collect();
        let mut follow = follower(&backend, &table, &wake, vec![], NO_FALLBACK);
        follow.tick(Instant::now());
        let mut at = backend.cost();
        let mut cost = |what: &str| {
            follow.tick(Instant::now());
            follow.tick(Instant::now());
            let now = backend.cost();
            let c = (now.0 - at.0, now.1 - at.1);
            eprintln!("{what}: {} list, {} process reads", c.0, c.1);
            at = now;
            c
        };
        table.changed(4, Prop::Playing);
        assert_eq!(cost("one process starts playing"), (0, 1));
        table.changed(4, Prop::Devices);
        assert_eq!(cost("one process moves"), (0, 1));
        backend.0.lock().unwrap().procs.push(proc(11, 111, MUSIC, "/x/Music"));
        wake.heard(Said::List);
        assert_eq!(cost("one process comes"), (1, 1));
        backend.0.lock().unwrap().procs.retain(|p| p.object != 11);
        wake.heard(Said::List);
        assert_eq!(cost("one process goes"), (1, 0));
    }

    #[test]
    fn a_burst_of_changes_costs_one_look() {
        let wake =
            Arc::new(Wake::new(Duration::from_secs(600)).settling(Duration::from_millis(250)));
        let looks = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&looks);
        let thread = spawn("test settle", Arc::clone(&wake), move |_| {
            counted.fetch_add(1, Ordering::AcqRel);
            Ok(())
        })
        .unwrap();
        assert_eq!(looks.load(Ordering::Acquire), 1);
        for object in 0..10 {
            wake.heard(Said::Process(object));
            std::thread::sleep(Duration::from_millis(5));
        }
        soon("no look after the burst", || looks.load(Ordering::Acquire) == 2);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(looks.load(Ordering::Acquire), 2, "a burst cost more than one look");
        assert_eq!(wake.take().objects.len(), 10);
        // Stopping does not wait out the settle.
        let t = Instant::now();
        wake.heard(Said::Process(1));
        wake.poke(true);
        thread.join().unwrap();
        assert!(t.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn a_process_that_refuses_a_listener_is_read_again_after_the_fallback() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        backend.0.lock().unwrap().procs =
            vec![proc(3, 5, MUSIC, "/x/Music"), proc(10, 412, DJAY, DJAY_EXE)];
        let fallback = Duration::from_millis(50);
        let mut follow = follower(&backend, &table, &wake, vec![3], fallback);
        follow.tick(Instant::now());
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (1, 2), "read again before the fallback passed");
        std::thread::sleep(fallback);
        // Only the process that refused is read again (and asked again).
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (1, 3));
        follow.tick(Instant::now());
        assert_eq!(backend.cost(), (1, 3));
        assert_eq!(table.listened(Prop::Playing), [10]);
    }

    #[test]
    fn a_follower_whose_own_listener_failed_reads_everything_after_the_fallback() {
        let (backend, table, wake) = (Shared::default(), Table::default(), no_poll());
        backend.0.lock().unwrap().procs = vec![proc(3, 5, MUSIC, "/x/Music")];
        wake.set_unheard();
        let mut follow = follower(&backend, &table, &wake, vec![], Duration::ZERO);
        for _ in 0..5 {
            follow.tick(Instant::now());
        }
        assert_eq!(backend.cost(), (5, 5));
    }
}
