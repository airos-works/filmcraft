//! air/OS (Haiku) only: builds `native/media_kit.cpp`, which loads the Media Kit's hardware
//! decoder add-ons and feeds them (src/media_kit.rs). Every other target builds nothing here.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native/media_kit.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("haiku") {
        return;
    }
    // The decoder add-on interface (`DecoderPlugin.h`) is among the SDK's private headers; `=` is the
    // compiler's sysroot.
    cc::Build::new().cpp(true).file("native/media_kit.cpp").flag("-isystem=/boot/system/develop/headers/private/media").flag("-Wno-multichar").flag("-Wno-unused-parameter").opt_level(2).compile("filmcraft_platform_media_kit");
    for lib in ["media", "be", "stdc++"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
