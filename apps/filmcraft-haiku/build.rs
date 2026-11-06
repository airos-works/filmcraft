//! Builds the native host (`native/host.cpp`, the Media Kit sound in `native/audio.cpp` and the
//! Media Kit file decoding in `native/media_source.cpp`)
//! against the Be API when the target is Haiku (air/OS is a Haiku distribution); elsewhere the
//! crate is only the platform-neutral rasteriser, key tables and menu layout, and its binary says
//! it needs Haiku.

fn main() {
    for file in ["native/host.cpp", "native/audio.cpp", "native/media_source.cpp"] {
        println!("cargo:rerun-if-changed={file}");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("haiku") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .file("native/host.cpp")
        .file("native/audio.cpp")
        .file("native/media_source.cpp")
        .flag("-Wno-multichar")
        .flag("-Wno-unused-parameter")
        .opt_level(2)
        .compile("filmcraft_haiku_host");
    // game: BDirectWindow, which lets frames go straight into the screen; media: the sound player
    // and recorder.
    for lib in ["be", "game", "media", "tracker", "translation", "stdc++"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
