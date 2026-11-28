//! App taps on Windows: process loopback, one client per process tree,
//! under the same followers as macOS ([`Follow`], [`Split`]) through a
//! [`Backend`] that reads the audio sessions.
//!
//! A process is an audio client while it has a session on a render
//! endpoint; it plays while one of its sessions is active. A tap of a
//! process takes in its whole tree (a browser's audio helper with the
//! browser), wherever it plays: process loopback has no device to scope
//! it to, so [`Target::OnDevice`] and [`Target::AllExcept`] are refused,
//! and so is a muted tap (the session's mute silences its loopback too).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, S_OK, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, AudioSessionStateActive, AudioSessionStateExpired,
    DEVICE_STATE_ACTIVE, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, IAudioSessionControl2, IAudioSessionManager2,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, PWSTR, Ref, implement};

use super::endpoints::{enumerator, id_of};
use super::{Kind, Tap, Ticker, fail};
use crate::apps::{
    self, AppInfo, AppStatus, AppTap, Backend, Follow, Mute, Proc, Remade, Target, tree_roots,
};
use crate::channels::{AppSinks, ChannelsStatus, Split};
use crate::{Error, ErrorKind, Sink};

/// How long process loopback's activation may take.
const ACTIVATION: Duration = Duration::from_secs(5);

/// Why a muted app tap is refused on Windows.
pub(crate) const NO_MUTE: &str = "a muted app tap is not possible on Windows: muting an \
app's audio session silences what is tapped of it too; tap it unmuted";
/// Why a muted device tap is refused on Windows.
pub(crate) const NO_DEVICE_MUTE: &str = "a muted device tap is not possible on Windows: \
endpoint loopback has no mute; tap it unmuted";
/// Why an "everything except" tap is refused on Windows.
pub(crate) const NO_ALL_EXCEPT: &str = "tapping every app except some is not possible on \
Windows: tap the apps by name, or tap the output they play into";

/// Every process with an audio session on an active render endpoint,
/// merged by pid: playing while a session is active, `devices` the
/// endpoints (by id) it has sessions on.
fn sessions() -> Result<Vec<Proc>, Error> {
    let e = enumerator()?;
    let mut procs: Vec<Proc> = Vec::new();
    // SAFETY: COM runs on this thread; every interface is read while held.
    unsafe {
        let all = e
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| fail("the output list", e))?;
        for i in 0..all.GetCount().unwrap_or(0) {
            let Ok(device) = all.Item(i) else { continue };
            let id = id_of(&device).unwrap_or_default();
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(list) = manager.GetSessionEnumerator() else { continue };
            for j in 0..list.GetCount().unwrap_or(0) {
                let Ok(session) = list.GetSession(j) else { continue };
                let Ok(state) = session.GetState() else { continue };
                let Ok(control) = session.cast::<IAudioSessionControl2>() else { continue };
                if state == AudioSessionStateExpired || control.IsSystemSoundsSession() == S_OK {
                    continue;
                }
                let pid = match control.GetProcessId() {
                    Ok(pid) if pid != 0 => pid,
                    _ => continue,
                };
                let playing = state == AudioSessionStateActive;
                match procs.iter_mut().find(|p| p.object == pid) {
                    Some(p) => {
                        p.playing |= playing;
                        if !p.devices.contains(&id) {
                            p.devices.push(id.clone());
                        }
                    }
                    None => procs.push(Proc {
                        object: pid,
                        pid: pid as i32,
                        bundle: None,
                        exe: exe_of(pid),
                        playing,
                        devices: vec![id.clone()],
                    }),
                }
            }
        }
    }
    Ok(procs)
}

/// A process's executable path, when it may be read.
fn exe_of(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed once read; the buffer outlives the call.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Whether process `pid` runs.
fn alive(pid: u32) -> bool {
    // SAFETY: the handle is closed once waited on.
    unsafe {
        let Ok(h) =
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid)
        else {
            return false;
        };
        let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        let _ = CloseHandle(h);
        running
    }
}

/// Every process's parent, by pid.
fn parents() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    // SAFETY: the snapshot is closed once walked; the entry is sized.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return map };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            map.insert(e.th32ProcessID, e.th32ParentProcessID);
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    map
}

/// Hears process loopback's activation finish.
/// (An `implement` object is agile already, as activation requires.)
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Mutex<Option<mpsc::Sender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        if let Some(tx) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

/// A process-loopback client of `pid`'s process tree, not yet initialized.
pub(crate) unsafe fn activate(pid: u32) -> Result<IAudioClient, Error> {
    let what = format!("tapping pid {pid}");
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let mut prop = PROPVARIANT::default();
    // SAFETY: a blob pointing at `params`, which outlives the activation
    // (waited for below); not cleared, since it owns nothing.
    unsafe {
        let inner = &mut *prop.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
        let (tx, rx) = mpsc::channel();
        let handler: IActivateAudioInterfaceCompletionHandler =
            Activated(Mutex::new(Some(tx))).into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&prop),
            &handler,
        )
        .map_err(|e| fail(&what, e))?;
        if rx.recv_timeout(ACTIVATION).is_err() {
            return Err(Error::new(ErrorKind::Device, format!("{what}: activation timed out")));
        }
        let mut result = HRESULT(0);
        let mut client: Option<IUnknown> = None;
        op.GetActivateResult(&mut result, &mut client).map_err(|e| fail(&what, e))?;
        result.ok().map_err(|e| fail(&what, e))?;
        client
            .ok_or_else(|| Error::new(ErrorKind::Device, format!("{what}: no client")))?
            .cast::<IAudioClient>()
            .map_err(|e| fail(&what, e))
    }
}

/// The real [`Backend`]: audio sessions and process-loopback taps.
struct Sessions {
    rate: u32,
    channels: usize,
}

impl Backend for Sessions {
    type Tap = Tap;

    fn processes(&mut self) -> Vec<Proc> {
        sessions().unwrap_or_default()
    }

    /// A pid is its own object; any running process may be tapped, before
    /// it plays too (its tap starts delivering when it does).
    fn translate(&mut self, pid: i32) -> Option<u32> {
        u32::try_from(pid).ok().filter(|&p| p != 0 && alive(p))
    }

    fn open(
        &mut self,
        targets: &[Target],
        mute: Mute,
        sink: Sink,
    ) -> Result<(Tap, u32), (Error, Sink)> {
        if mute != Mute::Unmuted {
            return Err((Error::new(ErrorKind::Unsupported, NO_MUTE), sink));
        }
        let mut pids = Vec::new();
        for t in targets {
            match t {
                Target::Processes(p) => pids.extend_from_slice(p),
                Target::AllExcept(_) | Target::OnDevice(..) => {
                    return Err((Error::new(ErrorKind::Unsupported, NO_ALL_EXCEPT), sink));
                }
            }
        }
        let parent = parents();
        let roots = tree_roots(&pids, |p| parent.get(&p).copied());
        if roots.is_empty() {
            return Err((Error::new(ErrorKind::NotFound, "no process to tap"), sink));
        }
        let kinds = roots.into_iter().map(Kind::Process).collect();
        Tap::open(kinds, self.rate, self.channels, sink)
    }

    fn close(&mut self, tap: Tap) -> Sink {
        tap.into_sink()
    }

    fn health(&mut self, tap: &Tap, playing: bool) -> Option<String> {
        tap.health(playing)
    }
}

fn this() -> i32 {
    std::process::id() as i32
}

/// Every process with an audio session, playing ones first.
pub(crate) fn list_apps() -> Vec<AppInfo> {
    apps::infos(&sessions().unwrap_or_default(), this())
}

/// A running app follower: its thread owns the [`Follow`] and the tap.
pub(crate) struct Following {
    status: Arc<Mutex<(AppStatus, Option<Remade>)>>,
    _ticker: Ticker,
}

impl Following {
    pub(crate) fn start(
        tap: AppTap,
        rate: u32,
        channels: usize,
        sink: Sink,
    ) -> Result<Following, Error> {
        let status = Arc::new(Mutex::new((AppStatus::Failed { error: "starting".into() }, None)));
        let mut follow = Follow::new(Sessions { rate, channels }, tap, this(), sink);
        let shared = Arc::clone(&status);
        let ticker = Ticker::start("audio app tap", move |now| {
            let s = follow.tick(now).clone();
            *shared.lock().unwrap_or_else(|e| e.into_inner()) =
                (s.clone(), follow.remade().cloned());
            match s {
                AppStatus::Failed { error } => Err(Error::new(ErrorKind::Device, error)),
                _ => Ok(()),
            }
        })?;
        Ok(Following { status, _ticker: ticker })
    }

    pub(crate) fn status(&self) -> AppStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    pub(crate) fn remade(&self) -> Option<Remade> {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).1.clone()
    }
}

/// A running per-app follower: its thread owns the [`Split`], its taps and
/// the caller's [`AppSinks`].
pub(crate) struct Channels {
    status: Arc<Mutex<ChannelsStatus>>,
    _ticker: Ticker,
}

impl Channels {
    pub(crate) fn start<S: AppSinks>(
        tap: AppTap,
        rate: u32,
        channels: usize,
        sinks: S,
    ) -> Result<Channels, Error> {
        let status = Arc::new(Mutex::new(ChannelsStatus::default()));
        let mut split = Split::new(Sessions { rate, channels }, tap, this(), sinks);
        let shared = Arc::clone(&status);
        let ticker = Ticker::start("audio app channels", move |now| {
            let s = split.tick(now).clone();
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = s;
            Ok(())
        })?;
        Ok(Channels { status, _ticker: ticker })
    }

    pub(crate) fn status(&self) -> ChannelsStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
