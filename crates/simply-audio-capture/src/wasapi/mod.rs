//! The Windows taps, through WASAPI loopback: what an output plays
//! (endpoint loopback, [`endpoints`]) and what an application plays
//! (process loopback, [`sessions`]).
//!
//! Both read an `IAudioClient` in loopback mode on a thread of their own
//! ([`Stream`]), asked for 32-bit float at the width and rate the caller
//! wants: the audio engine converts, so no frame is resampled here. A tap
//! of several apps is several clients (one process tree each), summed by
//! [`Mix`]. The followers above them (the default output, the apps, the
//! per-app channels) are the same pure ones macOS runs, ticked by
//! [`Ticker`] every [`TICK`].

mod endpoints;
mod sessions;

pub(crate) use endpoints::{DefaultTap, OutputTap, list_devices};
pub(crate) use sessions::{Channels, Following, NO_ALL_EXCEPT, NO_MUTE, list_apps};

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient,
    IAudioClient, IAudioRenderClient, IMMDevice, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::apps::{STALL, Stall};
use crate::mix::Mix;
use crate::watch::{self, Wake};
use crate::{Error, ErrorKind, Sink};

/// How often a follower looks when nothing woke it: the default output,
/// the audio sessions.
const TICK: Duration = Duration::from_millis(500);

/// The capture buffer asked for. Loopback delivers once per engine period
/// (10 ms); the buffer is the slack the reading thread has.
const BUFFER_HNS: i64 = 1_000_000; // 100 ms, in 100 ns units
/// Process loopback's: the size Microsoft's own sample asks for.
const PROCESS_BUFFER_HNS: i64 = 200_000; // 20 ms
/// How long the reading thread waits for the engine's event before it
/// looks anyway (an event not signalled must not stop the reading).
const WAIT_MS: u32 = 10;
/// Silent frames handed on per sink call when a buffer is flagged silent.
const ZERO_FRAMES: usize = 1024;

thread_local! {
    static COM: Cell<bool> = const { Cell::new(false) };
}

/// COM on this thread (multithreaded), once. A thread COM already runs on
/// (single-threaded, a GUI's) keeps what it has; the calls work there too.
pub(crate) fn com() {
    COM.with(|c| {
        if !c.get() {
            // SAFETY: no reserved pointer; called once per thread.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            c.set(true);
        }
    });
}

/// A WASAPI failure as this crate's error, saying what was being done.
pub(crate) fn fail(what: &str, e: windows::core::Error) -> Error {
    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        return Error::new(ErrorKind::Device, format!("{what}: the device went away"));
    }
    Error::new(ErrorKind::Device, format!("{what}: {e}"))
}

/// What a [`Stream`] reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// What a render endpoint (by id) plays.
    Endpoint(String),
    /// What a process tree (by its root's pid) plays, wherever it plays.
    Process(u32),
}

/// What a stream's thread tells its owner.
#[derive(Default)]
struct State {
    /// Buffers read so far.
    packets: AtomicU64,
    /// Why it stopped reading, if it did.
    error: Mutex<Option<String>>,
}

type Push = Box<dyn FnMut(&[f32], u64) + Send>;

/// One loopback client, read on its own thread until dropped.
struct Stream {
    stop: Arc<AtomicBool>,
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl Stream {
    /// Open `kind` for `channels`-wide float frames at `rate`, read into
    /// `push`. Returns once it reads, with the rate its device runs at.
    fn start(
        kind: Kind,
        rate: u32,
        channels: usize,
        mut push: Push,
    ) -> Result<(Stream, u32), Error> {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(State::default());
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, Error>>();
        let thread = {
            let (stop, state) = (Arc::clone(&stop), Arc::clone(&state));
            std::thread::Builder::new()
                .name("audio loopback".into())
                .spawn(move || {
                    com();
                    // SAFETY: the COM calls run on this thread, in COM.
                    let opened = unsafe { Opened::open(&kind, rate, channels) };
                    let o = match opened {
                        Ok(o) => {
                            let _ = ready_tx.send(Ok(o.device_rate));
                            o
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    // SAFETY: as above; the buffers are read while held.
                    if let Err(e) = unsafe { o.pump(channels, rate, &stop, &state, &mut push) } {
                        let mut slot = state.error.lock().unwrap_or_else(|e| e.into_inner());
                        slot.get_or_insert_with(|| e.to_string());
                    }
                })
                .map_err(|e| {
                    Error::new(ErrorKind::Device, format!("cannot start a loopback thread: {e}"))
                })?
        };
        let mut stream = Stream { stop, state, thread: Some(thread) };
        match ready_rx.recv() {
            Ok(Ok(device_rate)) => Ok((stream, device_rate)),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                stream.join();
                Err(Error::new(ErrorKind::Device, "the loopback thread died before reporting"))
            }
        }
    }

    fn join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Why it no longer reads, if it doesn't.
    fn problem(&self) -> Option<String> {
        if let Some(e) = self.state.error.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Some(e);
        }
        self.thread
            .as_ref()
            .is_some_and(|t| t.is_finished())
            .then(|| "the loopback stream stopped".to_owned())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.join();
    }
}

/// An event handle, closed when dropped.
struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: made by CreateEventW, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// An open loopback client, and what keeps it delivering.
struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    keepalive: Option<Keepalive>,
    device_rate: u32,
}

impl Opened {
    unsafe fn open(kind: &Kind, rate: u32, channels: usize) -> Result<Opened, Error> {
        let format = float_format(rate, channels);
        let fmt = &format.Format as *const WAVEFORMATEX;
        let convert = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let loopback = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        unsafe {
            let event =
                Event(CreateEventW(None, false, false, None).map_err(|e| fail("an event", e))?);
            match kind {
                Kind::Endpoint(id) => {
                    let device = endpoints::device(id)?;
                    let client: IAudioClient = device
                        .Activate(CLSCTX_ALL, None)
                        .map_err(|e| fail("opening the output for loopback", e))?;
                    let device_rate = mix_rate(&client)?;
                    client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            loopback | convert,
                            BUFFER_HNS,
                            0,
                            fmt,
                            None,
                        )
                        .map_err(|e| fail("starting loopback of the output", e))?;
                    // Endpoint loopback delivers nothing while nothing
                    // plays: a silent stream of our own keeps it going.
                    let keepalive = Keepalive::start(&device)?;
                    Opened::finish(client, event, Some(keepalive), device_rate)
                }
                Kind::Process(pid) => {
                    // The client takes the format it is given (it has no
                    // mix format: GetMixFormat says E_NOTIMPL). Asked as is
                    // first, then converted by the engine, on a new client
                    // (one whose Initialize failed is not reused).
                    let client = sessions::activate(*pid)?;
                    let client = match client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        loopback,
                        PROCESS_BUFFER_HNS,
                        0,
                        fmt,
                        None,
                    ) {
                        Ok(()) => client,
                        Err(_) => {
                            let client = sessions::activate(*pid)?;
                            client
                                .Initialize(
                                    AUDCLNT_SHAREMODE_SHARED,
                                    loopback | convert,
                                    PROCESS_BUFFER_HNS,
                                    0,
                                    fmt,
                                    None,
                                )
                                .map_err(|e| fail(&format!("starting loopback of pid {pid}"), e))?;
                            client
                        }
                    };
                    Opened::finish(client, event, None, rate)
                }
            }
        }
    }

    unsafe fn finish(
        client: IAudioClient,
        event: Event,
        keepalive: Option<Keepalive>,
        device_rate: u32,
    ) -> Result<Opened, Error> {
        unsafe {
            client.SetEventHandle(event.0).map_err(|e| fail("the loopback event", e))?;
            let capture: IAudioCaptureClient =
                client.GetService().map_err(|e| fail("the loopback capture client", e))?;
            client.Start().map_err(|e| fail("starting the loopback stream", e))?;
            Ok(Opened { client, capture, event, keepalive, device_rate })
        }
    }

    /// Read every buffer into `push` until `stop`; the first failure ends
    /// it. Buffers flagged silent are handed on as zeros, never read.
    unsafe fn pump(
        &self,
        channels: usize,
        rate: u32,
        stop: &AtomicBool,
        state: &State,
        push: &mut Push,
    ) -> Result<(), Error> {
        let zeros = vec![0.0f32; ZERO_FRAMES * channels];
        let frame_ns = 1e9 / rate as f64;
        let result = loop {
            if stop.load(Ordering::Acquire) {
                break Ok(());
            }
            unsafe {
                let _ = WaitForSingleObject(self.event.0, WAIT_MS);
                if let Some(k) = &self.keepalive
                    && let Err(e) = k.top_up()
                {
                    break Err(fail("the loopback keepalive", e));
                }
                if let Err(e) = self.drain(channels, frame_ns, &zeros, state, push) {
                    break Err(fail("reading the loopback stream", e));
                }
            }
        };
        unsafe {
            let _ = self.client.Stop();
            if let Some(k) = &self.keepalive {
                let _ = k.client.Stop();
            }
        }
        result
    }

    unsafe fn drain(
        &self,
        channels: usize,
        frame_ns: f64,
        zeros: &[f32],
        state: &State,
        push: &mut Push,
    ) -> windows::core::Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                // The QPC position is in 100 ns units: the host clock.
                let ns = qpc.saturating_mul(100);
                let len = frames as usize * channels;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    let mut done = 0;
                    while done < len {
                        let n = (len - done).min(zeros.len());
                        let at = if ns == 0 {
                            0
                        } else {
                            ns + ((done / channels) as f64 * frame_ns) as u64
                        };
                        push(&zeros[..n], at);
                        done += n;
                    }
                } else {
                    // SAFETY: the engine hands `frames` frames of the float
                    // format asked for, valid until ReleaseBuffer.
                    push(std::slice::from_raw_parts(data as *const f32, len), ns);
                }
                self.capture.ReleaseBuffer(frames)?;
                state.packets.fetch_add(1, Ordering::AcqRel);
            }
        }
        Ok(())
    }
}

/// A silent render stream on the tapped endpoint: endpoint loopback hands
/// out buffers only while something renders.
struct Keepalive {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
}

impl Keepalive {
    unsafe fn start(device: &IMMDevice) -> Result<Keepalive, Error> {
        let what = "the loopback keepalive";
        unsafe {
            let client: IAudioClient =
                device.Activate(CLSCTX_ALL, None).map_err(|e| fail(what, e))?;
            let mix = client.GetMixFormat().map_err(|e| fail(what, e))?;
            let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, mix, None);
            CoTaskMemFree(Some(mix as *const _));
            r.map_err(|e| fail(what, e))?;
            let frames = client.GetBufferSize().map_err(|e| fail(what, e))?;
            let render: IAudioRenderClient = client.GetService().map_err(|e| fail(what, e))?;
            let k = Keepalive { client, render, frames };
            k.top_up().map_err(|e| fail(what, e))?;
            k.client.Start().map_err(|e| fail(what, e))?;
            Ok(k)
        }
    }

    /// Fill what the engine has played with silence.
    unsafe fn top_up(&self) -> windows::core::Result<()> {
        unsafe {
            let free = self.frames.saturating_sub(self.client.GetCurrentPadding()?);
            if free > 0 {
                self.render.GetBuffer(free)?;
                self.render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
        }
        Ok(())
    }
}

/// The rate an endpoint's engine runs at.
unsafe fn mix_rate(client: &IAudioClient) -> Result<u32, Error> {
    unsafe {
        let mix = client.GetMixFormat().map_err(|e| fail("the output's format", e))?;
        let rate = std::ptr::addr_of!((*mix).nSamplesPerSec).read_unaligned();
        CoTaskMemFree(Some(mix as *const _));
        Ok(rate)
    }
}

/// Interleaved 32-bit float, `channels` wide, at `rate`: a plain
/// `WAVEFORMATEX` for mono and stereo, the extensible form for more.
fn float_format(rate: u32, channels: usize) -> WAVEFORMATEXTENSIBLE {
    let ch = channels.clamp(1, 32) as u16;
    let extensible = ch > 2;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: if extensible { WAVE_FORMAT_EXTENSIBLE } else { WAVE_FORMAT_IEEE_FLOAT }
                as u16,
            nChannels: ch,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * ch as u32 * 4,
            nBlockAlign: ch * 4,
            wBitsPerSample: 32,
            cbSize: if extensible { 22 } else { 0 },
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: 0,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// A running tap: one loopback stream or several, summed into one sink.
pub(crate) struct Tap {
    streams: Vec<Stream>,
    mix: Arc<Mutex<Mix<Sink>>>,
    stall: Mutex<Stall>,
}

impl Tap {
    /// Open `kinds` (one stream each) into `sink`, as `channels`-wide
    /// frames at `rate`. Returns with the rate the first one's device runs
    /// at; on failure the sink comes back.
    pub(crate) fn open(
        kinds: Vec<Kind>,
        rate: u32,
        channels: usize,
        sink: Sink,
    ) -> Result<(Tap, u32), (Error, Sink)> {
        let mix = Arc::new(Mutex::new(Mix::new(sink, kinds.len(), channels, rate)));
        let mut streams = Vec::with_capacity(kinds.len());
        let mut device_rate = rate;
        for (i, kind) in kinds.into_iter().enumerate() {
            let m = Arc::clone(&mix);
            let push: Push = Box::new(move |frames: &[f32], ns: u64| {
                m.lock().unwrap_or_else(|e| e.into_inner()).push(i, frames, ns, Instant::now())
            });
            match Stream::start(kind, rate, channels, push) {
                Ok((s, r)) => {
                    if i == 0 {
                        device_rate = r;
                    }
                    streams.push(s);
                }
                Err(e) => {
                    drop(streams);
                    return Err((e, sink_of(mix)));
                }
            }
        }
        let tap = Tap { streams, mix, stall: Mutex::new(Stall::new(Instant::now())) };
        Ok((tap, device_rate))
    }

    /// Stop every stream; the sink comes back.
    pub(crate) fn into_sink(self) -> Sink {
        let Tap { streams, mix, .. } = self;
        // Each stream's thread holds the mix; joined, it lets go.
        drop(streams);
        sink_of(mix)
    }

    /// Why it no longer delivers, if it doesn't: a stream failed, or
    /// (while what it taps is `playing`) no buffer came for [`STALL`].
    pub(crate) fn health(&self, playing: bool) -> Option<String> {
        if let Some(p) = self.streams.iter().find_map(Stream::problem) {
            return Some(p);
        }
        let count: u64 = self.streams.iter().map(|s| s.state.packets.load(Ordering::Acquire)).sum();
        let now = Instant::now();
        let mut stall = self.stall.lock().unwrap_or_else(|e| e.into_inner());
        if !playing {
            stall.rest(count, now);
            return None;
        }
        stall
            .stalled(count, now)
            .then(|| format!("the tap stopped delivering audio for {} s", STALL.as_secs()))
    }
}

/// The sink out of a mix no stream holds any more.
fn sink_of(mix: Arc<Mutex<Mix<Sink>>>) -> Sink {
    match Arc::try_unwrap(mix) {
        Ok(m) => m.into_inner().unwrap_or_else(|e| e.into_inner()).into_sink(),
        // Not reached: every holder was joined. A sink that drops what it
        // gets is better than a panic.
        Err(_) => Box::new(|_: &[f32], _: u64| {}),
    }
}

/// A thread that ticks a follower: once at the start (its error is the
/// start's), then every [`TICK`], until dropped.
pub(crate) struct Ticker {
    wake: Arc<Wake>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    pub(crate) fn start<T>(name: &str, mut tick: T) -> Result<Ticker, Error>
    where
        T: FnMut(Instant) -> Result<(), Error> + Send + 'static,
    {
        let wake = Arc::new(Wake::new(TICK));
        let thread = watch::spawn(name, Arc::clone(&wake), move |now| {
            com();
            tick(now)
        })?;
        Ok(Ticker { wake, thread: Some(thread) })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.wake.poke(true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
