fn main() {
    // SkyLight is a macOS private framework. The default linker search path
    // only covers /System/Library/Frameworks; private frameworks live one
    // level deeper, so we add it explicitly. Used by activate.rs for the
    // cross-Space window focus trick.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-search=framework=/System/Library/PrivateFrameworks");
    }
}
