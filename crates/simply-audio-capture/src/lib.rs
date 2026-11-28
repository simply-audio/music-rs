//! Capture any audio device by name, inputs and outputs alike, through one
//! call.
//!
//! The rule ([`resolve`]):
//!
//! * a device with **input channels** is captured as an input, through cpal;
//! * an **output-only** device (speakers, headphones, a display, a
//!   Multi-Output Device) is captured by tapping what the Mac plays on it: a
//!   CoreAudio process tap of every process **but this one** (a program
//!   never records its own playback), mixed to stereo and **not muted**
//!   unless [`start_with_mute`] asks for it, inside a private aggregate
//!   device this process reads like an input (macOS 14.2+);
//! * `""` is the current default output, tapped, and **followed**: when the
//!   Mac's default output changes mid-tap the tap moves to the new device
//!   (a clean restart, the same sink) and the stream carries on.
//!   [`resolve`] names the default it finds, which is a tap of that one
//!   device; pass `Source::Tap("")` to [`start`] to follow the default.
//!
//! Either way the device runs at its own rate (nothing on the machine is
//! retuned) and the frames are resampled to the rate asked for, if it
//! differs, before they reach the sink.
//!
//! ```no_run
//! let devices = simply_audio_capture::list_devices();
//! let source = simply_audio_capture::resolve(&devices, "")?; // the default output
//! let capture = simply_audio_capture::start(&source, 48_000, 2, |frames: &[f32], _ns: u64| {
//!     // interleaved stereo at 48 kHz; runs on the audio thread
//! })?;
//! std::thread::sleep(std::time::Duration::from_secs(2));
//! drop(capture); // stops; the tap and its aggregate device are destroyed
//! # Ok::<(), simply_audio_capture::Error>(())
//! ```
//!
//! # Tapping applications
//!
//! [`start_app`] taps **applications** rather than a device, so an app's
//! audio reaches this process without Loopback or BlackHole (macOS 14.2+):
//!
//! * [`Apps::Only`]: these apps (by bundle id, executable name or pid; one
//!   or several), mixed to stereo, wherever they play;
//! * [`Apps::AllExcept`]: every process except these (leave yourself out,
//!   [`App::This`], so your own output doesn't feed back).
//!
//! [`App::PlayingInto`] names processes by the output device they play
//! into (a device UID prefix), and is re-read the moment one moves (a HAL
//! listener on each process's devices): a program with its own virtual
//! devices leaves out what already reaches it through them, so the tap
//! doesn't carry it twice. In [`Apps::AllExcept`] only that output is left
//! out: an app playing into such a device and into the speakers at once is
//! still heard from the speakers, through a tap of its own scoped to them.
//!
//! The capture **follows** the apps. While none of them runs it reports
//! [`AppStatus::Waiting`] ("waiting for djay Pro"); it attaches when one
//! starts, and makes a new tap when one restarts (a new process is a new
//! HAL process object). A tapped app that exits, a clock device that
//! disappears, or a tap that stops calling back is reported in
//! [`AppCapture::status`] and [`AppCapture::error`], never as silence. The
//! tap is clocked by [`set_app_tap_clock`]'s device, else by the default
//! output, at the moment it is made, and runs at that device's rate.
//! [`Mute`] chooses whether the app stays audible while tapped.
//! [`list_apps`] lists what can be tapped, for a picker.
//!
//! ```no_run
//! use simply_audio_capture::{App, AppTap, Mute};
//! let tap = AppTap::only([App::parse("com.algoriddim.djay-iphone-free")]).with_mute(Mute::Unmuted);
//! let capture = simply_audio_capture::start_app(&tap, 48_000, 2, |frames: &[f32], _ns: u64| {
//!     // the same sink as `start`'s: interleaved stereo at 48 kHz, host-clock ns
//! })?;
//! println!("{}", capture.status()); // "waiting for com.algoriddim.djay-iphone-free", or "tapping djay Pro (pid 412) at 48000 Hz"
//! # Ok::<(), simply_audio_capture::Error>(())
//! ```
//!
//! # Each app on its own channel
//!
//! [`start_app_channels`] takes the same [`AppTap`] but keeps the apps
//! apart: each process it selects gets a tap of its own, from when it
//! starts playing until it exits or has been quiet for [`CHANNEL_HOLD`].
//! An [`AppSinks`] hears each channel start (and hands back its sink) and
//! stop; [`AppChannels::status`] lists them. See its docs for an example.
//!
//! # Windows
//!
//! The same calls tap through WASAPI loopback (Windows 11, x64 and ARM64):
//!
//! * an output by name, or `""` for the default output, followed when it
//!   changes, is **endpoint loopback** of that render endpoint. A silent
//!   stream of this process's own plays into it meanwhile, so the tap
//!   delivers (digital silence) while nothing else plays;
//! * [`start_app`] and [`start_app_channels`] are **process loopback**:
//!   one client per process *tree* (a browser's audio helper comes with
//!   the browser), wherever it plays; several apps in one [`start_app`]
//!   are summed here. An app is its executable ([`App::Executable`], with
//!   or without `.exe`) or its pid; Windows has no bundle ids.
//!
//! What Windows can't do is refused with [`ErrorKind::Unsupported`]: a
//! muted tap ([`Mute::Muted`], [`Mute::MutedWhenTapped`]: muting an app's
//! session silences its loopback too) and [`Apps::AllExcept`]. Loopback
//! needs no consent ([`permission`] is granted). The audio engine converts
//! to the width and rate asked for, and delivers once per engine period
//! (10 ms), so [`set_tap_buffer_frames`] and [`set_app_tap_clock`] do
//! nothing there. Apps that play through ASIO or WASAPI exclusive mode
//! bypass the engine and can't be tapped at all.
//!
//! # The sink
//!
//! `sink(frames, capture_ns)` gets interleaved `channels`-wide frames at the
//! requested rate, and the host time (nanoseconds, the
//! `mach_absolute_time` clock on macOS, `QueryPerformanceCounter` on
//! Windows) at which the slice's first frame was captured, or 0 when the
//! device did not say. It runs on the device's
//! real-time thread: it must not allocate, lock, block or log. Copy into a
//! lock-free ring and return.
//!
//! # Cleanup
//!
//! Dropping the [`Capture`] (or [`AppCapture`]) stops it: once the drop returns the sink is never
//! called again, and a tap's aggregate device and tap are destroyed. A
//! private tap and aggregate also belong to the process that made them, so
//! coreaudiod removes them when the process dies without dropping (a
//! signal). A program that wants them gone *before* it exits on a signal
//! drops its capture in its own signal handler.
//!
//! # Permission
//!
//! Taps need *System Audio Recording* consent. Without it the tap is created
//! and delivers silence, so [`permission`] reads the state up front;
//! [`start`] refuses a tap when it is known to be denied.
//!
//! This crate knows nothing of the program that uses it, so it can be copied
//! between programs verbatim.

// Only the macOS and Windows taps drive these (Windows not all of them);
// their types are the API everywhere, and their tests (against fakes) run
// everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod apps;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod buffers;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod channels;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod default_out;
#[cfg_attr(not(windows), allow(dead_code))]
mod mix;
mod resample;
#[cfg(target_os = "macos")]
mod tap;
#[cfg(windows)]
mod wasapi;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod watch;

/// The OS's taps, one API: CoreAudio process taps on macOS, WASAPI
/// loopback on Windows.
#[cfg(target_os = "macos")]
use tap as os;
#[cfg(windows)]
use wasapi as os;

use resample::Resampler;

pub use apps::{App, AppInfo, AppStatus, AppTap, Apps, Mute, Remade};
pub use channels::{AppChannel, AppSinks, CHANNEL_HOLD, ChannelState, ChannelsStatus};

use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// How to grant the tap permission, for a message a person reads.
pub const GRANT_HINT: &str = "grant it in System Settings ▸ Privacy & Security ▸ \
Screen & System Audio Recording (the \"System Audio Recording Only\" list) to the \
app running this program (the terminal, or whatever launched it), then restart that app";

/// One audio device as [`resolve`] sees it. Devices sharing a name (a USB
/// microphone's input and output halves) are merged: [`resolve`] goes by
/// name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub inputs: bool,
    pub outputs: bool,
    pub is_default_output: bool,
}

/// How a device is captured: what [`resolve`] decides and [`start`] does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Read the device's inputs.
    Input(String),
    /// Tap what is played on this output device (`""` = the default
    /// output, followed when it changes).
    Tap(String),
}

impl Source {
    /// The device's name.
    pub fn device(&self) -> &str {
        match self {
            Source::Input(n) | Source::Tap(n) => n,
        }
    }
}

/// What went wrong, by what the caller can do about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// No device by that name (the message lists the ones there are).
    NotFound,
    /// macOS refused: Microphone for an input, System Audio Recording for a
    /// tap. Only a person can fix it.
    PermissionDenied,
    /// Not possible here: a tap off macOS and Windows or before macOS
    /// 14.2; on Windows, a muted app tap or one of every app except some.
    Unsupported,
    /// The device or the OS failed.
    Device,
}

#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error { kind, message: message.into() }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<cpal::Error> for Error {
    fn from(e: cpal::Error) -> Error {
        let kind = match e.kind() {
            cpal::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Device,
        };
        Error::new(kind, format!("audio device error: {e}"))
    }
}

/// The System Audio Recording (TCC) state of this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Granted,
    Denied,
    /// Never asked (the first tap prompts), not readable, or not macOS.
    Unknown,
}

/// Read (never request or change) whether this process may tap outputs.
/// Always granted on Windows, where loopback asks no one's consent.
pub fn permission() -> Permission {
    #[cfg(target_os = "macos")]
    return tap::permission();
    #[cfg(windows)]
    return Permission::Granted;
    #[cfg(not(any(target_os = "macos", windows)))]
    return Permission::Unknown;
}

/// Every audio device, merged by name (see [`Device`]). On Windows, the
/// render endpoints (tappable outputs) and cpal's inputs; elsewhere off
/// macOS, cpal's inputs only.
pub fn list_devices() -> Vec<Device> {
    let mut found: Vec<Device> = Vec::new();
    #[cfg(target_os = "macos")]
    for d in tap::list_devices().unwrap_or_default() {
        match found.iter_mut().find(|f| f.name == d.name) {
            Some(f) => {
                f.inputs |= d.inputs;
                f.outputs |= d.outputs;
                f.is_default_output |= d.is_default_output;
            }
            None => found.push(Device {
                name: d.name,
                inputs: d.inputs,
                outputs: d.outputs,
                is_default_output: d.is_default_output,
            }),
        }
    }
    #[cfg(windows)]
    found.extend(wasapi::list_devices());
    #[cfg(not(target_os = "macos"))]
    for d in cpal::default_host().input_devices().into_iter().flatten() {
        if let Some(name) = device_name(&d) {
            match found.iter_mut().find(|f| f.name == name) {
                Some(f) => f.inputs = true,
                None => found.push(Device {
                    name,
                    inputs: true,
                    outputs: false,
                    is_default_output: false,
                }),
            }
        }
    }
    found
}

/// The rule: `""` taps the default output; a named device with inputs is
/// read as an input; an output-only one is tapped. Names match exactly.
pub fn resolve(devices: &[Device], wanted: &str) -> Result<Source, Error> {
    if wanted.is_empty() {
        return match devices.iter().find(|d| d.is_default_output) {
            Some(d) => Ok(Source::Tap(d.name.clone())),
            None => Err(Error::new(ErrorKind::NotFound, "there is no default output device")),
        };
    }
    match devices.iter().find(|d| d.name == wanted) {
        Some(d) if d.inputs => Ok(Source::Input(d.name.clone())),
        Some(d) => Ok(Source::Tap(d.name.clone())),
        None => {
            let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
            Err(Error::new(
                ErrorKind::NotFound,
                format!("audio device {wanted:?} not found; available: {}", names.join(", ")),
            ))
        }
    }
}

/// A running capture. Dropping it stops it (see the crate docs).
pub struct Capture {
    source: Source,
    error: Arc<Mutex<Option<String>>>,
    running: Running,
}

enum Running {
    /// The cpal stream lives on its own thread (a stream is not `Send` on
    /// every host); closing the channel ends it.
    Input { stop: Option<mpsc::Sender<()>>, thread: Option<JoinHandle<()>>, device_rate: u32 },
    #[cfg(any(target_os = "macos", windows))]
    Tap(os::OutputTap),
    /// A tap of the default output, moved to each new default.
    #[cfg(any(target_os = "macos", windows))]
    Default(os::DefaultTap),
}

impl Capture {
    /// What is being captured; a tap of `""` names the device it found when
    /// it started (see [`device`](Capture::device) for the one now).
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The device captured right now. The same as [`source`](Capture::source)'s
    /// except for a tap of the default output, which moves with the default.
    pub fn device(&self) -> String {
        match &self.running {
            #[cfg(any(target_os = "macos", windows))]
            Running::Default(t) => t.now().device,
            _ => self.source.device().to_owned(),
        }
    }

    /// The rate the device runs at, before resampling (for a tap of the
    /// default output, the one it taps now).
    pub fn device_rate(&self) -> u32 {
        match &self.running {
            Running::Input { device_rate, .. } => *device_rate,
            #[cfg(any(target_os = "macos", windows))]
            Running::Tap(t) => t.sample_rate,
            #[cfg(any(target_os = "macos", windows))]
            Running::Default(t) => t.now().device_rate,
        }
    }

    /// The first error the stream reported while running (device unplugged,
    /// …), if any. For a tap: the tapped device disappeared, or the tap
    /// stopped delivering audio for two seconds. It stays set, except for a
    /// tap of the default output: that one is the problem right now (the
    /// new default's tap could not be made, and is retried; the tap's
    /// health), and clears when the tap recovers.
    pub fn error(&self) -> Option<String> {
        #[cfg(any(target_os = "macos", windows))]
        if let Running::Default(t) = &self.running {
            return t.now().error;
        }
        #[cfg_attr(not(any(target_os = "macos", windows)), allow(unused_mut))]
        let mut slot = self.error.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(any(target_os = "macos", windows))]
        if let (None, Running::Tap(t)) = (&*slot, &self.running) {
            *slot = t.health();
        }
        slot.clone()
    }
}

impl Drop for Capture {
    // Off macOS and Windows a device's inputs are the only way to run.
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(irrefutable_let_patterns))]
    fn drop(&mut self) {
        if let Running::Input { stop, thread, .. } = &mut self.running {
            drop(stop.take());
            if let Some(t) = thread.take() {
                let _ = t.join();
            }
        }
        // A tap stops and destroys itself when `running` drops (a followed
        // one after its thread has stopped).
    }
}

/// Start capturing `source` as `channels`-wide frames at `rate` into `sink`
/// (see the crate docs). Returns once audio is flowing, or with why not.
/// A tap is not muted: [`start_with_mute`] with [`Mute::Unmuted`].
pub fn start<F>(source: &Source, rate: u32, channels: u16, sink: F) -> Result<Capture, Error>
where
    F: FnMut(&[f32], u64) + Send + 'static,
{
    start_with_mute(source, Mute::Unmuted, rate, channels, sink)
}

/// [`start`], with whether a tap mutes what it taps, as an app tap's
/// [`AppTap::mute`] does: [`Mute::Muted`] silences every process the tap
/// hears on that device while it is captured, so they reach this process
/// and not the speakers. This process is never tapped, so it is never
/// muted either: what it plays on the device is still heard. An input
/// can't be muted, so it takes only [`Mute::Unmuted`].
pub fn start_with_mute<F>(
    source: &Source,
    mute: Mute,
    rate: u32,
    channels: u16,
    sink: F,
) -> Result<Capture, Error>
where
    F: FnMut(&[f32], u64) + Send + 'static,
{
    if mute != Mute::Unmuted && matches!(source, Source::Input(_)) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!("{:?} is read as an input, which can't be muted", source.device()),
        ));
    }
    let channels = channels.max(1);
    let rate = rate.max(1);
    let error = Arc::new(Mutex::new(None));
    match source {
        Source::Input(name) => {
            let (device_rate, stop, thread) =
                start_input(name, rate, channels, Box::new(sink), Arc::clone(&error))?;
            Ok(Capture {
                source: source.clone(),
                error,
                running: Running::Input { stop: Some(stop), thread: Some(thread), device_rate },
            })
        }
        #[cfg(target_os = "macos")]
        Source::Tap(name) => {
            if tap::permission() == Permission::Denied {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!(
                        "tapping an output needs System Audio Recording permission, which is denied: {GRANT_HINT}"
                    ),
                ));
            }
            if name.is_empty() {
                let t = tap::DefaultTap::start(mute, rate, channels as usize, Box::new(sink))?;
                return Ok(Capture {
                    source: Source::Tap(t.now().device),
                    error,
                    running: Running::Default(t),
                });
            }
            let t = tap::OutputTap::start(name, mute, rate, channels as usize, Box::new(sink))?;
            Ok(Capture { source: Source::Tap(t.device.clone()), error, running: Running::Tap(t) })
        }
        // No consent to check: loopback needs none on Windows.
        #[cfg(windows)]
        Source::Tap(name) => {
            if name.is_empty() {
                let t = wasapi::DefaultTap::start(rate, channels as usize, Box::new(sink))?;
                return Ok(Capture {
                    source: Source::Tap(t.now().device),
                    error,
                    running: Running::Default(t),
                });
            }
            let t = wasapi::OutputTap::start(name, rate, channels as usize, Box::new(sink))?;
            Ok(Capture { source: Source::Tap(t.device.clone()), error, running: Running::Tap(t) })
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        Source::Tap(_) => Err(Error::new(
            ErrorKind::Unsupported,
            "tapping an output needs macOS 14.2+ or Windows; capture an input (a loopback device) instead",
        )),
    }
}

type Sink = Box<dyn FnMut(&[f32], u64) + Send>;

/// Every process the HAL knows plays (or can play) audio, playing ones
/// first: what an app picker shows. [`AppInfo::app`] is what to tap it by.
/// Needs no permission. On Windows, every process with an audio session.
/// Empty off macOS and Windows.
pub fn list_apps() -> Vec<AppInfo> {
    #[cfg(any(target_os = "macos", windows))]
    return os::list_apps();
    #[cfg(not(any(target_os = "macos", windows)))]
    return Vec::new();
}

/// A running app tap (see [`start_app`]). Dropping it stops it: once the
/// drop returns the sink is never called again, and the tap and its
/// aggregate device are destroyed.
pub struct AppCapture {
    tap: AppTap,
    #[cfg(any(target_os = "macos", windows))]
    following: os::Following,
}

impl AppCapture {
    /// What was asked for.
    pub fn tap(&self) -> &AppTap {
        &self.tap
    }

    /// Tapping, waiting for the app, or failed (and retrying). Cheap: read
    /// it as often as a UI repaints.
    pub fn status(&self) -> AppStatus {
        #[cfg(any(target_os = "macos", windows))]
        return self.following.status();
        #[cfg(not(any(target_os = "macos", windows)))]
        unreachable!("an AppCapture is only made on macOS and Windows")
    }

    /// The last time its running tap went bad and was made again (it
    /// stopped calling back while an app played, as after the Mac sleeps,
    /// or its clock device disappeared), and how many times so far. The
    /// status goes from tapping to tapping meanwhile: this is what says it
    /// reconnected. None until it has.
    pub fn remade(&self) -> Option<Remade> {
        #[cfg(any(target_os = "macos", windows))]
        return self.following.remade();
        #[cfg(not(any(target_os = "macos", windows)))]
        unreachable!("an AppCapture is only made on macOS and Windows")
    }

    /// The problem right now, if there is one: the tapped app exited (and
    /// is waited for), its tap failed, or its clock device disappeared.
    /// None while tapping, and while waiting for an app that was never
    /// there. Unlike [`Capture::error`] it clears when the tap recovers.
    pub fn error(&self) -> Option<String> {
        match self.status() {
            AppStatus::Tapping { .. } | AppStatus::Waiting { reason: None, .. } => None,
            AppStatus::Waiting { reason: Some(r), .. } => Some(r),
            AppStatus::Failed { error } => Some(error),
        }
    }
}

/// Tap applications rather than a device: `tap` names them (or the ones to
/// leave out), and the capture follows them. `channels`-wide frames at
/// `rate` into `sink`, exactly as [`start`]'s (see the crate docs).
///
/// Returns once it has looked: tapping, or waiting for an app that isn't
/// running. Fails when System Audio Recording is known to be denied, when
/// [`Apps::Only`] names nothing, off macOS and Windows, or when the first
/// tap could not be made. On Windows also for what Windows can't do: a
/// muted tap, and [`Apps::AllExcept`].
pub fn start_app<F>(tap: &AppTap, rate: u32, channels: u16, sink: F) -> Result<AppCapture, Error>
where
    F: FnMut(&[f32], u64) + Send + 'static,
{
    may_tap_apps(tap)?;
    #[cfg(any(target_os = "macos", windows))]
    {
        let following = os::Following::start(
            tap.clone(),
            rate.max(1),
            channels.max(1) as usize,
            Box::new(sink),
        )?;
        Ok(AppCapture { tap: tap.clone(), following })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (rate, channels, sink);
        unreachable!("may_tap_apps refuses off macOS and Windows")
    }
}

/// Clock every app tap ([`start_app`], [`start_app_channels`]) by output
/// `device` from now on, by name (`None`: the default output when each tap
/// is made, as before). A tap runs at its clock device's rate: clocked by
/// the device the audio is played to, it runs at that device's rate, not
/// at the default output's, and is resampled once, not twice. A tap
/// already running on another clock is made again on this one at its
/// next look (within half a second). A device that isn't there falls back
/// to the default output. Off macOS: nothing (on Windows a process tap has
/// no clock device: the audio engine paces it).
pub fn set_app_tap_clock(device: Option<&str>) {
    #[cfg(target_os = "macos")]
    tap::set_app_clock(device.map(str::to_owned));
    #[cfg(not(target_os = "macos"))]
    let _ = device;
}

/// Ask every tap's private aggregate device for an I/O buffer of `frames`
/// (`kAudioDevicePropertyBufferFrameSize`), process-wide, from now on:
/// device taps ([`start`] of an output, the followed default output) and
/// app taps ([`start_app`], [`start_app_channels`]) alike. `None` leaves
/// each aggregate at macOS's default (512 frames, measured on macOS 26).
///
/// A tap made from now on (a new one, or one made again on a new clock, a
/// new default output or a restarted app) asks for it as it is made. The
/// taps already running are asked at once, from the calling thread, without
/// being made again: the HAL changes a running device's buffer size between
/// two I/O cycles, and the taps' callback takes any frame count. `None`
/// gives them back the size they were made with. A size the aggregate
/// doesn't take is held to the nearest it does; a refusal leaves it as it
/// was. macOS keeps a buffer size per process and device, and the
/// aggregates are private to this process, so no other program is touched.
/// Off macOS: nothing (Windows taps deliver once per audio engine period,
/// 10 ms).
pub fn set_tap_buffer_frames(frames: Option<u32>) {
    #[cfg(target_os = "macos")]
    tap::set_buffer_frames(frames);
    #[cfg(not(target_os = "macos"))]
    let _ = frames;
}

/// Why `tap` can't be followed here, if it can't: it names no app,
/// System Audio Recording is known to be denied, it asks Windows for what
/// Windows can't do, or this is neither macOS nor Windows.
fn may_tap_apps(tap: &AppTap) -> Result<(), Error> {
    if matches!(&tap.apps, Apps::Only(a) if a.is_empty()) {
        return Err(Error::new(ErrorKind::NotFound, "an app tap needs at least one app"));
    }
    #[cfg(windows)]
    {
        if tap.mute != Mute::Unmuted {
            return Err(Error::new(ErrorKind::Unsupported, wasapi::NO_MUTE));
        }
        if matches!(tap.apps, Apps::AllExcept(_)) {
            return Err(Error::new(ErrorKind::Unsupported, wasapi::NO_ALL_EXCEPT));
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    if tap::permission() == Permission::Denied {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "tapping an app needs System Audio Recording permission, which is denied: {GRANT_HINT}"
            ),
        ));
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    return Err(Error::new(ErrorKind::Unsupported, "tapping an app needs macOS 14.2+ or Windows"));
    #[cfg(target_os = "macos")]
    Ok(())
}

/// A running per-app capture (see [`start_app_channels`]). Dropping it
/// stops it: every channel's tap is destroyed, its sink dropped and
/// [`AppSinks::stopped`] called, before the drop returns.
pub struct AppChannels {
    tap: AppTap,
    #[cfg(any(target_os = "macos", windows))]
    channels: os::Channels,
}

impl AppChannels {
    /// What was asked for.
    pub fn tap(&self) -> &AppTap {
        &self.tap
    }

    /// The channels now, and the apps asked for that aren't running.
    /// Cheap: read it as often as a UI repaints.
    pub fn status(&self) -> ChannelsStatus {
        #[cfg(any(target_os = "macos", windows))]
        return self.channels.status();
        #[cfg(not(any(target_os = "macos", windows)))]
        unreachable!("an AppChannels is only made on macOS and Windows")
    }
}

/// Tap applications as [`start_app`] does, but **each app on its own
/// channel** rather than one mix: every process `tap` selects gets its own
/// process tap, from when it starts playing until it exits, leaves the
/// selection (it now plays only into an [`App::PlayingInto`] device; one
/// that plays there and elsewhere keeps its channel, for the elsewhere) or
/// has been quiet
/// for [`CHANNEL_HOLD`]. `sinks` hears each channel start and stop and
/// gives each its sink: `channels`-wide frames at `rate`, exactly as
/// [`start`]'s (see the crate docs).
///
/// [`Apps::Only`] gives a channel to each running process of the apps
/// named; [`Apps::AllExcept`] to every playing process not excluded. A
/// channel is one process: an app's helper processes (a browser's) are
/// channels of their own, each telling its bundle id and pid.
///
/// Each channel's tap is clocked by [`set_app_tap_clock`]'s device, else by
/// the default output, when it is made, and runs on its own real-time thread; the sinks' host-clock times are
/// what lines channels up.
///
/// Returns after the first look, with the apps playing now already
/// started. Fails when System Audio Recording is known to be denied, when
/// [`Apps::Only`] names nothing, or off macOS; a channel whose tap cannot
/// be made is reported in its [`ChannelState`] and retried.
///
/// ```no_run
/// use simply_audio_capture::{App, AppInfo, AppSinks, AppTap};
///
/// struct Meters;
/// impl AppSinks for Meters {
///     type Sink = Box<dyn FnMut(&[f32], u64) + Send>;
///     fn started(&mut self, app: &AppInfo) -> Self::Sink {
///         println!("+ {} (pid {})", app.name, app.pid);
///         Box::new(|frames: &[f32], _ns: u64| {
///             // this app's interleaved stereo at 48 kHz, on its own audio thread
///         })
///     }
///     fn stopped(&mut self, app: &AppInfo) {
///         println!("- {} (pid {})", app.name, app.pid);
///     }
/// }
///
/// let tap = AppTap::all_except([App::This]);
/// let capture = simply_audio_capture::start_app_channels(&tap, 48_000, 2, Meters)?;
/// for c in capture.status().channels {
///     println!("{} {:?}", c.app.name, c.state);
/// }
/// # Ok::<(), simply_audio_capture::Error>(())
/// ```
pub fn start_app_channels<S: AppSinks>(
    tap: &AppTap,
    rate: u32,
    channels: u16,
    sinks: S,
) -> Result<AppChannels, Error> {
    may_tap_apps(tap)?;
    #[cfg(any(target_os = "macos", windows))]
    {
        let channels =
            os::Channels::start(tap.clone(), rate.max(1), channels.max(1) as usize, sinks)?;
        Ok(AppChannels { tap: tap.clone(), channels })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (rate, channels, sinks);
        unreachable!("may_tap_apps refuses off macOS and Windows")
    }
}

fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

fn start_input(
    name: &str,
    rate: u32,
    channels: u16,
    sink: Sink,
    error: Arc<Mutex<Option<String>>>,
) -> Result<(u32, mpsc::Sender<()>, JoinHandle<()>), Error> {
    let wanted = name.to_owned();
    let (ok_tx, ok_rx) = mpsc::channel::<Result<u32, Error>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("audio capture".into())
        .spawn(move || {
            let build = move || -> Result<(cpal::Stream, u32), Error> {
                let host = cpal::default_host();
                let device = host
                    .input_devices()?
                    .find(|d| device_name(d).as_deref() == Some(wanted.as_str()))
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::NotFound,
                            format!("input device {wanted:?} not found"),
                        )
                    })?;
                let device_rate = device.default_input_config()?.sample_rate();
                let mut feed = Feed::new(sink, channels as usize, device_rate, rate);
                let config = cpal::StreamConfig {
                    channels,
                    sample_rate: device_rate,
                    buffer_size: cpal::BufferSize::Default,
                };
                let stream = device.build_input_stream::<f32, _, _>(
                    config,
                    move |data: &[f32], info: &cpal::InputCallbackInfo| {
                        feed.push(data, info.timestamp().capture.as_nanos() as u64)
                    },
                    move |e| {
                        if let Ok(mut slot) = error.lock() {
                            slot.get_or_insert_with(|| e.to_string());
                        }
                    },
                    None,
                )?;
                stream.play()?;
                Ok((stream, device_rate))
            };
            match build() {
                Ok((stream, device_rate)) => {
                    let _ = ok_tx.send(Ok(device_rate));
                    // Until the Capture drops its sender.
                    let _ = stop_rx.recv();
                    drop(stream);
                }
                Err(e) => {
                    let _ = ok_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| {
            Error::new(ErrorKind::Device, format!("cannot start the capture thread: {e}"))
        })?;
    match ok_rx.recv() {
        Ok(Ok(device_rate)) => Ok((device_rate, stop_tx, thread)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::new(ErrorKind::Device, "the capture thread died before reporting"))
        }
    }
}

/// Output frames handed to the sink per call when resampling.
const CHUNK_FRAMES: usize = 4096;

/// What a device callback pushes into: the sink, behind a resampler when
/// the device runs at another rate. Allocates only in `new`.
pub(crate) struct Feed {
    sink: Sink,
    channels: usize,
    from_hz: f64,
    resampler: Option<Resampler>,
    out: Vec<f32>,
}

impl Feed {
    pub(crate) fn new(sink: Sink, channels: usize, from_hz: u32, to_hz: u32) -> Feed {
        let resampler =
            (from_hz != to_hz).then(|| Resampler::new(from_hz as f64, to_hz as f64, channels));
        let out = if resampler.is_some() { vec![0.0; CHUNK_FRAMES * channels] } else { Vec::new() };
        Feed { sink, channels, from_hz: from_hz as f64, resampler, out }
    }

    /// The sink back, for the next tap (an app tap re-made after a restart).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn into_sink(self) -> Sink {
        self.sink
    }

    /// `frames`: interleaved, `channels` wide, at the device's rate; `ns`:
    /// capture time of the first one (0 = unknown).
    pub(crate) fn push(&mut self, frames: &[f32], ns: u64) {
        let Feed { sink, channels, from_hz, resampler, out } = self;
        let Some(r) = resampler else {
            sink(frames, ns);
            return;
        };
        let ch = *channels;
        let (mut filled, mut chunk_ns) = (0usize, 0u64);
        // Input frame `first` is this buffer's first, captured at `ns`.
        let first = r.fed() as f64;
        for frame in frames.chunks_exact(ch) {
            r.feed(frame, |o, pos| {
                if filled == 0 && ns != 0 {
                    // Output frame at input position `pos` (it may lie in
                    // the buffer before: the filter runs behind).
                    let at = ns as f64 + (pos - first) * 1e9 / *from_hz;
                    chunk_ns = at.max(1.0) as u64;
                }
                out[filled * ch..(filled + 1) * ch].copy_from_slice(o);
                filled += 1;
                if filled == CHUNK_FRAMES {
                    sink(&out[..], chunk_ns);
                    filled = 0;
                }
            });
        }
        if filled > 0 {
            sink(&out[..filled * ch], chunk_ns);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, inputs: bool, outputs: bool) -> Device {
        Device { name: name.into(), inputs, outputs, is_default_output: false }
    }

    #[test]
    fn a_device_with_inputs_is_captured_and_an_output_only_one_is_tapped() {
        let mut devices = vec![
            dev("BlackHole 2ch", true, true),
            dev("Scarlett 4i4 USB", true, true),
            dev("MacBook Pro Microphone", true, false),
            dev("MacBook Pro Speakers", false, true),
            dev("Multi-Output Device", false, true),
        ];
        devices[3].is_default_output = true;
        let path = |n| resolve(&devices, n).unwrap();
        assert_eq!(path(""), Source::Tap("MacBook Pro Speakers".into()));
        assert_eq!(path("BlackHole 2ch"), Source::Input("BlackHole 2ch".into()));
        assert_eq!(path("Scarlett 4i4 USB"), Source::Input("Scarlett 4i4 USB".into()));
        assert_eq!(path("MacBook Pro Microphone"), Source::Input("MacBook Pro Microphone".into()));
        assert_eq!(path("MacBook Pro Speakers"), Source::Tap("MacBook Pro Speakers".into()));
        assert_eq!(path("Multi-Output Device"), Source::Tap("Multi-Output Device".into()));
        let err = resolve(&devices, "Nope").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(err.to_string().contains("MacBook Pro Speakers"), "{err}");
        devices[3].is_default_output = false;
        assert_eq!(resolve(&devices, "").unwrap_err().kind(), ErrorKind::NotFound);
    }

    #[test]
    fn an_input_can_t_be_muted() {
        let input = Source::Input("Scarlett 4i4 USB".into());
        let err = start_with_mute(&input, Mute::Muted, 48_000, 2, |_: &[f32], _| {}).err();
        assert_eq!(err.map(|e| e.kind()), Some(ErrorKind::Unsupported));
    }

    /// Everything `feed` hands its sink, at `from` → `to`.
    fn resample(from: u32, to: u32, input: &[f32], channels: usize) -> (Vec<f32>, Vec<u64>) {
        let got = Arc::new(Mutex::new((Vec::new(), Vec::new())));
        let sink = {
            let got = Arc::clone(&got);
            Box::new(move |f: &[f32], ns: u64| {
                let mut g = got.lock().unwrap();
                g.0.extend_from_slice(f);
                g.1.push(ns);
            })
        };
        let mut feed = Feed::new(sink, channels, from, to);
        feed.push(input, 1_000_000_000);
        drop(feed);
        Arc::try_unwrap(got).ok().unwrap().into_inner().unwrap()
    }

    #[test]
    fn the_same_rate_passes_straight_through() {
        let input = [0.1f32, 0.2, 0.3, 0.4];
        let (out, ns) = resample(48_000, 48_000, &input, 2);
        assert_eq!(out, input);
        assert_eq!(ns, vec![1_000_000_000]);
    }

    /// Every output frame whose taps have arrived (all but the last 16
    /// input frames' worth: the filter runs that far behind), a constant
    /// exactly.
    #[test]
    fn resampling_44k1_to_48k_yields_the_right_frame_count_and_keeps_dc() {
        let input: Vec<f32> = (0..441).flat_map(|_| [0.25f32, -0.5]).collect();
        let (out, _) = resample(44_100, 48_000, &input, 2);
        let frames = out.len() / 2;
        assert_eq!(frames, ((441.0 - 16.0) * 48_000.0 / 44_100.0f64).ceil() as usize);
        assert!(out.chunks(2).all(|f| f == [0.25, -0.5]));
    }

    /// Each sink call is stamped with its first frame's place in the
    /// input: the first output frame is the first input frame (the same
    /// instant), and one the filter made from the buffer before is
    /// stamped back in it.
    #[test]
    fn resampled_frames_are_stamped_where_they_lie_in_the_input() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let got = Arc::clone(&got);
            Box::new(move |f: &[f32], ns: u64| got.lock().unwrap().push((f.len(), ns)))
        };
        let mut feed = Feed::new(sink, 1, 44_100, 48_000);
        let at = |frame: f64| 1e9 + frame * 1e9 / 44_100.0;
        feed.push(&[0.0; 512], at(0.0) as u64);
        feed.push(&[0.0; 512], at(512.0) as u64);
        let got = got.lock().unwrap();
        assert_eq!(got[0].1, 1_000_000_000);
        // The second buffer's first output frame: the first's last were
        // made from it, so it lies 16 input frames back, in the first.
        let pos = got[0].0 as f64 * 44_100.0 / 48_000.0;
        assert!(pos < 512.0 && pos > 512.0 - 17.0, "{pos}");
        assert!((got[1].1 as f64 - at(pos)).abs() <= 1.0, "{} vs {}", got[1].1, at(pos));
    }

    #[test]
    fn a_long_buffer_reaches_the_sink_in_chunks_with_advancing_times() {
        let input = vec![0.0f32; 48_000];
        let (out, ns) = resample(48_000, 24_000, &input, 1);
        // All but the 32 input frames the filter (twice as wide converting
        // down by two) runs behind.
        assert_eq!(out.len(), 24_000 - 16);
        assert!(ns.len() > 1 && ns.windows(2).all(|w| w[1] > w[0]), "{ns:?}");
        // The second chunk starts CHUNK_FRAMES output frames (= 2× input) later.
        let expect = 1e9 + (2 * CHUNK_FRAMES) as f64 * 1e9 / 48_000.0;
        assert!((ns[1] as f64 - expect).abs() < 1e5, "{} vs {expect}", ns[1]);
    }
}
