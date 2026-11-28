//! Device taps on Windows: endpoint loopback of a render endpoint by name,
//! or of the default output, followed when it changes.

use std::sync::{Arc, Mutex};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::core::HSTRING;

use super::{Kind, Tap, Ticker, com, fail};
use crate::default_out::{FollowDefault, Now, Outputs};
use crate::{Device, Error, ErrorKind, Sink};

/// A render endpoint: what Windows calls an output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub id: String,
    pub name: String,
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator, Error> {
    com();
    // SAFETY: COM runs on this thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|e| fail("the audio device list", e))
}

/// An endpoint by id.
pub(crate) fn device(id: &str) -> Result<IMMDevice, Error> {
    let e = enumerator()?;
    // SAFETY: the id outlives the call.
    unsafe { e.GetDevice(&HSTRING::from(id)) }.map_err(|e| fail("the output went away", e))
}

/// An endpoint's id.
pub(crate) fn id_of(device: &IMMDevice) -> Option<String> {
    // SAFETY: the string is ours to free once copied.
    unsafe {
        let p = device.GetId().ok()?;
        let id = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        id
    }
}

/// An endpoint's name, as Settings shows it ("Speakers (Realtek(R) Audio)").
fn name_of(device: &IMMDevice) -> Option<String> {
    // SAFETY: the value is read while held, then cleared.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut v = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let inner = &v.Anonymous.Anonymous;
        let name = (inner.vt == VT_LPWSTR).then(|| inner.Anonymous.pwszVal.to_string().ok());
        let _ = PropVariantClear(&mut v);
        name.flatten()
    }
}

/// Every active render endpoint.
pub(crate) fn renders() -> Result<Vec<Endpoint>, Error> {
    let e = enumerator()?;
    let mut found = Vec::new();
    // SAFETY: COM runs on this thread.
    unsafe {
        let all = e
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| fail("the output list", e))?;
        for i in 0..all.GetCount().map_err(|e| fail("the output list", e))? {
            let Ok(d) = all.Item(i) else { continue };
            if let (Some(id), Some(name)) = (id_of(&d), name_of(&d)) {
                found.push(Endpoint { id, name });
            }
        }
    }
    Ok(found)
}

/// The default output's id (the console role: what Settings calls the
/// default), if there is one.
pub(crate) fn default_render() -> Option<String> {
    let e = enumerator().ok()?;
    // SAFETY: COM runs on this thread.
    let d = unsafe { e.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
    id_of(&d)
}

/// The outputs, as [`crate::list_devices`] merges them with cpal's inputs.
pub(crate) fn list_devices() -> Vec<Device> {
    let default = default_render();
    renders()
        .unwrap_or_default()
        .into_iter()
        .map(|e| Device {
            is_default_output: default.as_deref() == Some(e.id.as_str()),
            name: e.name,
            inputs: false,
            outputs: true,
        })
        .collect()
}

/// A tap of one output, by name.
pub(crate) struct OutputTap {
    pub device: String,
    /// The rate the output's engine runs at (the frames arrive at the rate
    /// asked for: the engine converts).
    pub sample_rate: u32,
    tap: Tap,
}

impl OutputTap {
    pub(crate) fn start(
        name: &str,
        rate: u32,
        channels: usize,
        sink: Sink,
    ) -> Result<OutputTap, Error> {
        let all = renders()?;
        let Some(e) = all.iter().find(|e| e.name == name) else {
            let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("output device {name:?} not found; outputs: {}", names.join(", ")),
            ));
        };
        let (tap, sample_rate) =
            Tap::open(vec![Kind::Endpoint(e.id.clone())], rate, channels, sink)
                .map_err(|(e, _)| e)?;
        Ok(OutputTap { device: e.name.clone(), sample_rate, tap })
    }

    pub(crate) fn health(&self) -> Option<String> {
        // The keepalive makes an endpoint deliver even while silent.
        self.tap.health(true)
    }
}

/// The real [`Outputs`]: endpoint taps of whatever the default output is.
/// Endpoint ids are strings; [`Outputs`] knows them by their index here.
struct Defaults {
    rate: u32,
    channels: usize,
    ids: Vec<String>,
}

impl Defaults {
    fn index(&mut self, id: String) -> u32 {
        match self.ids.iter().position(|i| *i == id) {
            Some(n) => n as u32,
            None => {
                self.ids.push(id);
                (self.ids.len() - 1) as u32
            }
        }
    }
}

impl Outputs for Defaults {
    type Tap = Tap;

    fn default_output(&mut self) -> Option<u32> {
        default_render().map(|id| self.index(id))
    }

    fn open(&mut self, id: u32, sink: Sink) -> Result<(Tap, String, u32), (Error, Sink)> {
        let Some(id) = self.ids.get(id as usize).cloned() else {
            return Err((Error::new(ErrorKind::NotFound, "no such output"), sink));
        };
        let name = match renders() {
            Ok(all) => all.into_iter().find(|e| e.id == id).map(|e| e.name),
            Err(e) => return Err((e, sink)),
        };
        let Some(name) = name else {
            let e = Error::new(ErrorKind::NotFound, "the default output device went away");
            return Err((e, sink));
        };
        let (tap, rate) = Tap::open(vec![Kind::Endpoint(id)], self.rate, self.channels, sink)?;
        Ok((tap, name, rate))
    }

    fn close(&mut self, tap: Tap) -> Sink {
        tap.into_sink()
    }

    fn health(&mut self, tap: &Tap) -> Option<String> {
        tap.health(true)
    }
}

/// A running tap of the default output that follows it to a new device.
pub(crate) struct DefaultTap {
    now: Arc<Mutex<Now>>,
    _ticker: Ticker,
}

impl DefaultTap {
    pub(crate) fn start(rate: u32, channels: usize, sink: Sink) -> Result<DefaultTap, Error> {
        let now = Arc::new(Mutex::new(Now::default()));
        let mut follow = FollowDefault::new(Defaults { rate, channels, ids: Vec::new() }, sink);
        let shared = Arc::clone(&now);
        let ticker = Ticker::start("audio default output tap", move |t| {
            let r = follow.tick(t).map(|_| ());
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = follow.now().clone();
            r
        })?;
        Ok(DefaultTap { now, _ticker: ticker })
    }

    pub(crate) fn now(&self) -> Now {
        self.now.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
