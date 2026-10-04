//! macOS only: give the dylib a relocatable install name.
//!
//! rustc's default `LC_ID_DYLIB` for a cdylib is the absolute path it was
//! linked at (`<checkout>/target/<profile>/deps/libastra_hotkey.dylib`) — a
//! path from the build machine baked into a shipped binary. The Astra daemon
//! `dlopen`s the library by full path from beside its own executable, so the
//! install name is never used for lookup; `@rpath/…` keeps it neutral and
//! correct for anyone who does link against it.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/libastra_hotkey.dylib");
    }
}
