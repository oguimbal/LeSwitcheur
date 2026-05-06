// Embeds the LeSwitcheur logo as icon resource ID 1 in the .exe.
// GPUI's Windows backend (`gpui_windows::platform::load_icon`) calls
// `LoadImageW(module, MAKEINTRESOURCE(1), IMAGE_ICON, ...)` to set the
// window-class icon, so resource ID 1 is what we need to ship.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    #[cfg(windows)]
    embed_windows_icon();
}

#[cfg(windows)]
fn embed_windows_icon() {
    use std::fs;
    use std::path::PathBuf;

    let png_path = "../../brand/logo-256.png";
    println!("cargo:rerun-if-changed={png_path}");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out_dir.join("app.ico");

    let img = image::open(png_path).expect("read brand/logo-256.png");
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &size in &[16u32, 32, 48, 64, 128, 256] {
        let resized = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);
        let rgba = resized.to_rgba8();
        let entry_img = ico::IconImage::from_rgba_data(size, size, rgba.into_raw());
        let entry = ico::IconDirEntry::encode(&entry_img).expect("encode ico entry");
        dir.add_entry(entry);
    }
    let f = fs::File::create(&ico_path).expect("create app.ico");
    dir.write(f).expect("write app.ico");

    let rc_path = out_dir.join("app.rc");
    let ico_for_rc = ico_path.to_string_lossy().replace('\\', "/");
    fs::write(&rc_path, format!("1 ICON \"{ico_for_rc}\"\n"))
        .expect("write app.rc");

    let result = embed_resource::compile(&rc_path, embed_resource::NONE);
    result.manifest_required().expect("embed icon resource");
}
