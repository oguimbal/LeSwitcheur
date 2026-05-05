//! Probe the CoreAudio process API: print what `current_audio_source` reports.
//!
//! Run:
//!   cargo run -p switcheur-platform --example probe_audio
//!
//! Useful while testing the "Currently Playing" feature — start audio in
//! Spotify / YouTube / Music / Safari, then run this to see which app/PID
//! the detection lands on (and whether browser-helper PIDs were correctly
//! resolved to the parent app).

#[cfg(target_os = "macos")]
fn main() {
    use std::time::Duration;
    use switcheur_platform::macos::{audio, now_playing};

    use switcheur_platform::macos::media_apps;

    println!("supported = {}", audio::is_supported());
    let sources = audio::current_audio_sources();
    if sources.is_empty() {
        println!("no audio source detected");
    } else {
        for (i, s) in sources.iter().enumerate() {
            println!("audio[{}]: {:#?}", i, s);
        }
    }
    let media = media_apps::probe_all();
    if media.is_empty() {
        println!("media_apps: none");
    } else {
        for (i, m) in media.iter().enumerate() {
            println!("media_apps[{}]: {:#?}", i, m);
        }
    }
    let np = now_playing::current_now_playing(Duration::from_millis(800));
    match &np {
        None => println!("now_playing: none"),
        Some(np) => println!("now_playing: {:#?}", np),
    }
    use switcheur_platform::macos::browser;
    use switcheur_core::Browser;
    let title = np.as_ref().and_then(|np| np.title.as_deref());
    let artist = np.as_ref().and_then(|np| np.artist.as_deref());
    let album = np.as_ref().and_then(|np| np.album.as_deref());
    for b in [Browser::Chrome, Browser::Safari] {
        match browser::audible_tab_for(b, title, artist, album) {
            None => println!("audible_tab_for({:?}): none", b),
            Some(t) => println!(
                "audible_tab_for({:?}): wid={} idx={} title={:?} host={:?}",
                b, t.window_id, t.tab_index, t.title, t.host()
            ),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macOS only");
}
