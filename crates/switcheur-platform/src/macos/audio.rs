//! Per-process audio activity detection via CoreAudio's process-object API
//! (public on macOS 14.2+). When a process is producing audio output, we
//! resolve its PID — walking up `pbi_ppid` for renderer subprocesses (Chrome
//! Helper, com.apple.WebKit.GPU) until we land on the user-facing app — and
//! map it to a [`Browser`] when the bundle id matches.
//!
//! Below 14.2 the API doesn't exist (`AudioObjectGetPropertyDataSize` returns
//! `kAudioHardwareUnknownPropertyError`). [`is_supported`] gates the entry
//! point so the feature degrades silently on older systems.

use std::os::raw::c_void;
use std::sync::OnceLock;

use coreaudio_sys::{
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyIsRunningOutput,
    kAudioProcessPropertyPID, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, OSStatus, UInt32,
};
use objc2_app_kit::NSRunningApplication;
use switcheur_core::Browser;

/// One detected audio source — either an ordinary app, or a browser whose
/// audio renderer subprocess we mapped back to the main app. The `helper_pid`
/// is the PID CoreAudio reported (typically Chrome Helper / Chromium Helper /
/// com.apple.WebKit.GPU); `app_pid` is the responsible app PID we focus.
#[derive(Debug, Clone)]
pub enum AudioSource {
    App {
        pid: i32,
        name: String,
        bundle_id: Option<String>,
    },
    Browser {
        browser: Browser,
        app_pid: i32,
        app_name: String,
        bundle_id: Option<String>,
        helper_pid: i32,
    },
}

/// Probe CoreAudio for currently-audible processes and resolve each to an
/// app. Returns an empty vec when nothing is producing output, the API is
/// unavailable, or the lookup fails. Order follows CoreAudio's
/// `kAudioHardwarePropertyProcessObjectList` enumeration. Duplicate
/// responsible PIDs (e.g. multiple Chrome helpers feeding audio) collapse
/// to a single source — keyed by `app_pid`.
pub fn current_audio_sources() -> Vec<AudioSource> {
    if !is_supported() {
        return Vec::new();
    }
    let mut out: Vec<AudioSource> = Vec::new();
    for helper_pid in audio_active_pids() {
        let Some(app_pid) = responsible_pid(helper_pid) else {
            continue;
        };
        if out.iter().any(|s| match s {
            AudioSource::App { pid, .. } => *pid == app_pid,
            AudioSource::Browser { app_pid: a, .. } => *a == app_pid,
        }) {
            continue;
        }
        let Some((name, bundle_id)) = app_metadata(app_pid) else {
            continue;
        };
        if let Some(browser) = bundle_to_browser(bundle_id.as_deref()) {
            out.push(AudioSource::Browser {
                browser,
                app_pid,
                app_name: name,
                bundle_id,
                helper_pid,
            });
        } else {
            out.push(AudioSource::App {
                pid: app_pid,
                name,
                bundle_id,
            });
        }
    }
    out
}

/// macOS 14.2+ check, cached. Below this the CoreAudio process-object API is
/// missing entirely (the symbols exist in headers but the daemon returns
/// `kAudioHardwareUnknownPropertyError`).
pub fn is_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        use objc2_foundation::NSProcessInfo;
        let info = NSProcessInfo::processInfo();
        let v = info.operatingSystemVersion();
        (v.majorVersion, v.minorVersion) >= (14, 2)
    })
}

/// Enumerate CoreAudio process objects whose `IsRunningOutput` flag is set.
/// We use `IsRunningOutput` rather than `IsRunning` because the latter is
/// also true for microphone-only producers (e.g. a meeting app capturing
/// audio without playing any), which would surface as false positives in
/// the "Currently Playing" row.
fn audio_active_pids() -> Vec<i32> {
    let list_addr = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyProcessObjectList,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut size: UInt32 = 0;
    let status: OSStatus = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject as AudioObjectID,
            &list_addr,
            0,
            std::ptr::null(),
            &mut size,
        )
    };
    if status != 0 || size == 0 {
        return Vec::new();
    }

    let count = (size as usize) / std::mem::size_of::<AudioObjectID>();
    let mut ids: Vec<AudioObjectID> = vec![0; count];
    let mut io_size = size;
    let status: OSStatus = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            &list_addr,
            0,
            std::ptr::null(),
            &mut io_size,
            ids.as_mut_ptr().cast::<c_void>(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    let actual = (io_size as usize) / std::mem::size_of::<AudioObjectID>();
    ids.truncate(actual);

    let mut pids = Vec::new();
    for obj in ids {
        if !is_running_output(obj) {
            continue;
        }
        if let Some(pid) = process_pid(obj) {
            if pid > 0 {
                pids.push(pid);
            }
        }
    }
    pids
}

fn is_running_output(obj: AudioObjectID) -> bool {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioProcessPropertyIsRunningOutput,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: UInt32 = 0;
    let mut size: UInt32 = std::mem::size_of::<UInt32>() as UInt32;
    let status: OSStatus = unsafe {
        AudioObjectGetPropertyData(
            obj,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut UInt32).cast::<c_void>(),
        )
    };
    status == 0 && value != 0
}

fn process_pid(obj: AudioObjectID) -> Option<i32> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioProcessPropertyPID,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut value: i32 = 0;
    let mut size: UInt32 = std::mem::size_of::<i32>() as UInt32;
    let status: OSStatus = unsafe {
        AudioObjectGetPropertyData(
            obj,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut i32).cast::<c_void>(),
        )
    };
    if status == 0 { Some(value) } else { None }
}

/// Walk `pbi_ppid` up from `pid` until we hit a process that has an
/// `NSRunningApplication` entry — i.e. a foreground GUI app. CoreAudio
/// reports the audio-rendering subprocess (Chrome Helper, com.apple.WebKit.GPU,
/// avconvd, …); the row should focus the user-facing parent.
///
/// Capped at 8 hops so a corrupt parent chain can't loop. Returns the original
/// PID if it already maps to a GUI app, or `None` if no ancestor does.
fn responsible_pid(pid: i32) -> Option<i32> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;

    let mut current = pid;
    for _ in 0..8 {
        if NSRunningApplication::runningApplicationWithProcessIdentifier(current).is_some() {
            return Some(current);
        }
        let Ok(info) = pidinfo::<BSDInfo>(current, 0) else {
            return None;
        };
        let parent = info.pbi_ppid as i32;
        if parent <= 1 || parent == current {
            return None;
        }
        current = parent;
    }
    None
}

fn app_metadata(pid: i32) -> Option<(String, Option<String>)> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    let bundle_id = app.bundleIdentifier().map(|s| s.to_string());
    Some((name, bundle_id))
}

fn bundle_to_browser(bundle_id: Option<&str>) -> Option<Browser> {
    match bundle_id? {
        "com.google.Chrome" => Some(Browser::Chrome),
        "com.apple.Safari" => Some(Browser::Safari),
        _ => None,
    }
}
