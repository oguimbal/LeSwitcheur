//! Cargo doesn't automatically track the YAML locale files read by rust-i18n's
//! proc macro, so edits there wouldn't trigger a rebuild of this crate (and
//! the translations that get embedded into the binary). Explicitly tell Cargo
//! to rerun when anything under `locales/` changes.

fn main() {
    println!("cargo:rerun-if-changed=locales");
}
