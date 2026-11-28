//! The two-tone loopback test: real WASAPI, on a machine with a render
//! endpoint (a Windows PC or VM, or a CI runner with a virtual sound card).
//!
//! Each test starts child processes that each play one known tone through
//! WASAPI shared mode on the default output (this test binary, run again
//! with `SIMPLY_AUDIO_TONE_HZ` set), captures, and checks by spectrum which
//! tones arrived: an app tap must hear its app and not the other, per-app
//! channels must keep the two apart, a tap of the default output must hear
//! both, and with nothing playing a device tap must still deliver frames,
//! as digital silence.
//!
//! Ignored by default (they open real devices); on Windows:
//!
//! ```text
//! cargo test -p simply-audio-capture --test windows_tones -- --ignored --test-threads=1
//! ```
#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use simply_audio_capture::{App, AppInfo, AppSinks, AppTap, ErrorKind, Mute, Source};

const RATE: u32 = 48_000;
const A_HZ: f64 = 997.0;
const B_HZ: f64 = 1499.0;
/// The tone each child plays, in dBFS.
const TONE_DB: f64 = -12.0;
/// What counts as heard, and as not heard, in dBFS at the tone.
const HEARD_DB: f64 = -30.0;
const UNHEARD_DB: f64 = -60.0;
/// How long the audio takes to start flowing, and how much is judged.
const SETTLE: Duration = Duration::from_millis(800);
const JUDGED: Duration = Duration::from_millis(1500);

const TONE_ENV: &str = "SIMPLY_AUDIO_TONE_HZ";

/// The tests play into one default output, so they take turns.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// This binary as a tone child: plays `SIMPLY_AUDIO_TONE_HZ` on the default
/// output until killed. Run only by [`Tone::play`]; on its own it returns.
#[test]
#[ignore = "a helper the tone tests run as a child process"]
fn tone_child() {
    let Ok(hz) = std::env::var(TONE_ENV) else { return };
    let hz: f64 = hz.parse().expect("a frequency");
    play_forever(hz);
}

fn play_forever(hz: f64) -> ! {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let device = cpal::default_host().default_output_device().expect("a default output");
    let config = device.default_output_config().expect("an output config");
    let channels = config.channels() as usize;
    let rate = config.sample_rate() as f64;
    let amp = 10f64.powf(TONE_DB / 20.0) as f32;
    let mut phase = 0.0f64;
    let step = std::f64::consts::TAU * hz / rate;
    let stream = device
        .build_output_stream::<f32, _, _>(
            config.config(),
            move |out: &mut [f32], _| {
                for frame in out.chunks_exact_mut(channels) {
                    let v = amp * phase.sin() as f32;
                    phase = (phase + step) % std::f64::consts::TAU;
                    frame.fill(v);
                }
            },
            |e| eprintln!("tone child: {e}"),
            None,
        )
        .expect("an output stream");
    stream.play().expect("the stream plays");
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A child process playing one tone; killed when dropped.
struct Tone {
    child: Child,
}

impl Tone {
    fn play(hz: f64) -> Tone {
        let exe = std::env::current_exe().expect("this test binary");
        let child = Command::new(exe)
            .args(["tone_child", "--exact", "--ignored", "--nocapture"])
            .env(TONE_ENV, hz.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("the tone child starts");
        Tone { child }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a sink heard: mono (the channels averaged), and how many frames.
#[derive(Clone, Default)]
struct Heard(Arc<Mutex<Vec<f32>>>);

impl Heard {
    fn sink(&self, channels: usize) -> impl FnMut(&[f32], u64) + Send + 'static {
        let got = Arc::clone(&self.0);
        move |frames: &[f32], _ns: u64| {
            let mut g = got.lock().unwrap();
            g.extend(frames.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / f.len() as f32));
        }
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn samples(&self) -> Vec<f32> {
        self.0.lock().unwrap().clone()
    }
}

/// Level in dBFS of the `hz` component of `x` at `rate` (Goertzel, Hann
/// window, scaled so a full-scale sine reads 0 dBFS).
fn level_db(x: &[f32], hz: f64, rate: f64) -> f64 {
    if x.is_empty() {
        return f64::NEG_INFINITY;
    }
    let n = x.len();
    let w = std::f64::consts::TAU * hz / rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    let mut window_sum = 0.0;
    for (i, v) in x.iter().enumerate() {
        let hann = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (n - 1).max(1) as f64).cos();
        window_sum += hann;
        let s = *v as f64 * hann + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    let amplitude = 2.0 * power.max(0.0).sqrt() / window_sum;
    20.0 * amplitude.max(1e-12).log10()
}

/// Wait for audio to flow, then judge `JUDGED` of what `heard` collects.
fn listen(heard: &Heard) -> Vec<f32> {
    std::thread::sleep(SETTLE);
    heard.clear();
    std::thread::sleep(JUDGED);
    heard.samples()
}

fn assert_hears(x: &[f32], what: &str, hears_a: bool, hears_b: bool) {
    let (a, b) = (level_db(x, A_HZ, RATE as f64), level_db(x, B_HZ, RATE as f64));
    let say =
        format!("{what}: {} frames, {A_HZ} Hz at {a:.1} dBFS, {B_HZ} Hz at {b:.1} dBFS", x.len());
    println!("{say}");
    assert!(x.len() as f64 > RATE as f64 * JUDGED.as_secs_f64() * 0.5, "too few frames; {say}");
    let check = |hears: bool, db: f64| if hears { db > HEARD_DB } else { db < UNHEARD_DB };
    assert!(check(hears_a, a) && check(hears_b, b), "{say}");
}

#[test]
#[ignore = "opens real WASAPI devices: run on Windows with --ignored"]
fn an_app_tap_hears_its_app_and_not_the_other() {
    let _turn = one_at_a_time();
    let (a, b) = (Tone::play(A_HZ), Tone::play(B_HZ));
    for (pid, hears_a, what) in [(a.pid(), true, "tap of A"), (b.pid(), false, "tap of B")] {
        let heard = Heard::default();
        let tap = AppTap::only([App::Pid(pid)]);
        let capture = simply_audio_capture::start_app(&tap, RATE, 2, heard.sink(2))
            .expect("the app tap starts");
        let x = listen(&heard);
        println!("{what}: {}", capture.status());
        assert_hears(&x, what, hears_a, !hears_a);
    }
}

#[test]
#[ignore = "opens real WASAPI devices: run on Windows with --ignored"]
fn an_app_tap_of_two_apps_mixes_both() {
    let _turn = one_at_a_time();
    let (a, b) = (Tone::play(A_HZ), Tone::play(B_HZ));
    let heard = Heard::default();
    let tap = AppTap::only([App::Pid(a.pid()), App::Pid(b.pid())]);
    let capture =
        simply_audio_capture::start_app(&tap, RATE, 2, heard.sink(2)).expect("the app tap starts");
    let x = listen(&heard);
    println!("tap of A and B: {}", capture.status());
    assert_hears(&x, "tap of A and B", true, true);
}

/// Each channel's frames, by pid.
#[derive(Clone, Default)]
struct Channels(Arc<Mutex<Vec<(i32, Heard)>>>);

impl AppSinks for Channels {
    type Sink = Box<dyn FnMut(&[f32], u64) + Send>;

    fn started(&mut self, app: &AppInfo) -> Self::Sink {
        let heard = Heard::default();
        self.0.lock().unwrap().push((app.pid, heard.clone()));
        Box::new(heard.sink(2))
    }

    fn stopped(&mut self, _app: &AppInfo) {}
}

impl Channels {
    fn of(&self, pid: i32) -> Option<Heard> {
        self.0.lock().unwrap().iter().find(|(p, _)| *p == pid).map(|(_, h)| h.clone())
    }
}

#[test]
#[ignore = "opens real WASAPI devices: run on Windows with --ignored"]
fn per_app_channels_keep_the_apps_apart() {
    let _turn = one_at_a_time();
    let (a, b) = (Tone::play(A_HZ), Tone::play(B_HZ));
    // Let both open their streams, so each has a session that plays.
    std::thread::sleep(Duration::from_millis(500));
    let channels = Channels::default();
    let tap = AppTap::only([App::Pid(a.pid()), App::Pid(b.pid())]);
    let capture = simply_audio_capture::start_app_channels(&tap, RATE, 2, channels.clone())
        .expect("the channels start");
    let until = Instant::now() + Duration::from_secs(5);
    while (channels.of(a.pid()).is_none() || channels.of(b.pid()).is_none())
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("channels: {:?}", capture.status());
    let (ha, hb) = (
        channels.of(a.pid()).expect("A has a channel"),
        channels.of(b.pid()).expect("B has a channel"),
    );
    std::thread::sleep(SETTLE);
    ha.clear();
    hb.clear();
    std::thread::sleep(JUDGED);
    assert_hears(&ha.samples(), "channel of A", true, false);
    assert_hears(&hb.samples(), "channel of B", false, true);
}

#[test]
#[ignore = "opens real WASAPI devices: run on Windows with --ignored"]
fn a_tap_of_the_default_output_hears_both() {
    let _turn = one_at_a_time();
    let (_a, _b) = (Tone::play(A_HZ), Tone::play(B_HZ));
    let heard = Heard::default();
    let capture = simply_audio_capture::start(&Source::Tap(String::new()), RATE, 2, heard.sink(2))
        .expect("the default output tap starts");
    let x = listen(&heard);
    println!("default output tap of {} at {} Hz", capture.device(), capture.device_rate());
    assert_hears(&x, "default output tap", true, true);
    assert_eq!(capture.error(), None);
}

#[test]
#[ignore = "opens real WASAPI devices: run on Windows with --ignored"]
fn a_device_tap_delivers_silence_while_nothing_plays() {
    let _turn = one_at_a_time();
    let devices = simply_audio_capture::list_devices();
    println!("devices: {devices:?}");
    let source = simply_audio_capture::resolve(&devices, "").expect("a default output");
    let heard = Heard::default();
    let capture =
        simply_audio_capture::start(&source, RATE, 2, heard.sink(2)).expect("the device tap");
    let x = listen(&heard);
    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    println!("{}: {} frames of nothing, peak {peak}", capture.device(), x.len());
    assert!(x.len() as f64 > RATE as f64 * JUDGED.as_secs_f64() * 0.5, "{} frames", x.len());
    assert!(peak < 1e-4, "peak {peak}");
    assert_eq!(capture.error(), None);
}

/// Needs no device: what Windows can't do is refused before anything opens.
#[test]
fn a_muted_tap_and_everything_except_are_refused() {
    let muted = AppTap::only([App::Pid(4)]).with_mute(Mute::Muted);
    let e = simply_audio_capture::start_app(&muted, RATE, 2, |_: &[f32], _: u64| {}).err();
    assert_eq!(e.map(|e| e.kind()), Some(ErrorKind::Unsupported));
    let all = AppTap::all_except([App::This]);
    let e = simply_audio_capture::start_app(&all, RATE, 2, |_: &[f32], _: u64| {}).err();
    assert_eq!(e.map(|e| e.kind()), Some(ErrorKind::Unsupported));
}

#[test]
fn goertzel_reads_a_tone_and_not_its_neighbour() {
    let amp = 10f64.powf(TONE_DB / 20.0);
    let x: Vec<f32> = (0..RATE as usize)
        .map(|i| (amp * (std::f64::consts::TAU * A_HZ * i as f64 / RATE as f64).sin()) as f32)
        .collect();
    assert!((level_db(&x, A_HZ, RATE as f64) - TONE_DB).abs() < 0.5);
    assert!(level_db(&x, B_HZ, RATE as f64) < UNHEARD_DB);
}
