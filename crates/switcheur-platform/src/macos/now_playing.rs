//! Bridge to MediaRemote.framework's "now playing" info — the same data the
//! macOS Control Center / Lock Screen widgets read from. Used by the
//! "Currently Playing" feature to identify *which browser tab* is producing
//! audio when the source PID is a browser helper renderer.
//!
//! MediaRemote is a private framework. We `dlopen` it and call
//! `MRMediaRemoteGetNowPlayingInfo` directly. macOS 14.2-15.3 honours the
//! call from any process; macOS 15.4+ added a daemon-side entitlement check
//! that rejects callers without `com.apple.*` bundle ids. When that happens
//! the callback never fires (or fires with a nil dict) and we return `None`,
//! falling back to the active-tab heuristic in `browser::audible_tab_for`.

use std::ffi::c_void;
use std::os::raw::{c_long, c_ulong};
use std::time::Duration;

use block2::RcBlock;
use crossbeam_channel::bounded;
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use switcheur_core::PlaybackState;

// Minimal dlopen/dlsym shim — avoids pulling the `libc` crate just for
// these two symbols. Both are in libSystem and always linked.
const RTLD_NOW: c_long = 2;
extern "C" {
    fn dlopen(filename: *const i8, flag: c_long) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
}

/// Subset of the MediaRemote "now playing" dictionary we care about. The
/// real dict carries 20+ keys (artwork, duration, playback rate, …); for
/// tab matching we only need title / artist / album, plus the optional
/// bundle id that some sources (mostly Apple Music) include.
#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub bundle_id: Option<String>,
    /// Playback state derived from `kMRMediaRemoteNowPlayingInfoPlaybackRate`:
    /// rate > 0 = Playing, rate == 0 = Paused, key absent = Unknown.
    pub state: PlaybackState,
}

impl NowPlaying {
    /// True when none of the fields carry useful matching data. Returned
    /// as `None` from [`current_now_playing`].
    pub fn is_blank(&self) -> bool {
        self.title.as_deref().map_or(true, str::is_empty)
            && self.artist.as_deref().map_or(true, str::is_empty)
            && self.album.as_deref().map_or(true, str::is_empty)
    }
}

#[link(name = "System", kind = "dylib")]
extern "C" {
    fn dispatch_get_global_queue(identifier: c_long, flags: c_ulong) -> *mut c_void;
}

/// `QOS_CLASS_DEFAULT` from `<dispatch/queue.h>`.
const QOS_CLASS_DEFAULT: c_long = 0x15;

/// Probe MediaRemote for the system "now playing" item. Returns `None` on
/// every failure path (framework load failed, symbol missing, callback
/// timed out, blank dict, daemon refused). `timeout` caps how long we wait
/// for the async callback.
pub fn current_now_playing(timeout: Duration) -> Option<NowPlaying> {
    unsafe {
        let handle = dlopen(
            c"/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote".as_ptr(),
            RTLD_NOW,
        );
        if handle.is_null() {
            return None;
        }

        let sym = dlsym(handle, c"MRMediaRemoteGetNowPlayingInfo".as_ptr());
        if sym.is_null() {
            return None;
        }

        type GetInfoFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
        let func: GetInfoFn = std::mem::transmute(sym);

        let queue = dispatch_get_global_queue(QOS_CLASS_DEFAULT, 0);
        if queue.is_null() {
            return None;
        }

        let (tx, rx) = bounded::<Option<NowPlaying>>(1);
        let block = RcBlock::new(move |info: *mut NSDictionary<NSString, NSString>| {
            let np = if info.is_null() {
                None
            } else {
                parse_dict(&*info)
            };
            let _ = tx.send(np);
        });

        // block2's RcBlock owns a heap-allocated block; the C callee copies
        // it (Block_copy semantics) so dropping our RcBlock after the call
        // doesn't invalidate the captured closure.
        let block_ptr = (&*block) as *const _ as *mut c_void;
        func(queue, block_ptr);

        rx.recv_timeout(timeout).ok().flatten().filter(|np| !np.is_blank())
    }
}

unsafe fn parse_dict(info: &NSDictionary<NSString, NSString>) -> Option<NowPlaying> {
    let title = get_string(info, "kMRMediaRemoteNowPlayingInfoTitle");
    let artist = get_string(info, "kMRMediaRemoteNowPlayingInfoArtist");
    let album = get_string(info, "kMRMediaRemoteNowPlayingInfoAlbum");
    let bundle_id = get_string(info, "kMRMediaRemoteNowPlayingInfoBundleIdentifier");
    let state = match get_double(info, "kMRMediaRemoteNowPlayingInfoPlaybackRate") {
        Some(rate) if rate > 0.0 => PlaybackState::Playing,
        Some(_) => PlaybackState::Paused,
        None => PlaybackState::Unknown,
    };
    if title.is_none() && artist.is_none() && album.is_none() {
        return None;
    }
    Some(NowPlaying {
        title,
        artist,
        album,
        bundle_id,
        state,
    })
}

unsafe fn get_double(
    dict: &NSDictionary<NSString, NSString>,
    key: &str,
) -> Option<f64> {
    use objc2::runtime::AnyObject;
    let key_ns = NSString::from_str(key);
    let dict_any: &NSDictionary<NSString, AnyObject> = std::mem::transmute(dict);
    let value = dict_any.objectForKey(&key_ns)?;
    let n = value.downcast_ref::<NSNumber>()?;
    Some(n.as_f64())
}

unsafe fn get_string(
    dict: &NSDictionary<NSString, NSString>,
    key: &str,
) -> Option<String> {
    use objc2::runtime::AnyObject;
    let key_ns = NSString::from_str(key);
    // `NSDictionary<NSString, NSString>` is a hint for the type system; the
    // real dict holds heterogeneous values (NSString, NSNumber, NSData), so
    // we ask for AnyObject and downcast on the read side.
    let dict_any: &NSDictionary<NSString, AnyObject> = std::mem::transmute(dict);
    let value = dict_any.objectForKey(&key_ns)?;
    let s = value.downcast_ref::<NSString>()?;
    Some(s.to_string())
}
