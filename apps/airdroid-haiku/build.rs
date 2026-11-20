// The Be API glue (native/host.cpp) is built only for air/OS; elsewhere the
// crate builds without it so its platform-neutral parts still test.
fn main() {
    println!("cargo:rerun-if-changed=native/host.cpp");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("haiku") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .file("native/host.cpp")
        .flag("-std=c++17")
        .flag("-Wno-multichar")
        .flag("-Wno-unused-parameter")
        .flag("-frtti")
        .opt_level(2)
        .compile("airdroid_host");
    for lib in ["be", "tracker", "translation", "media", "stdc++"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
