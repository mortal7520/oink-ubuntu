fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    println!("cargo:rerun-if-changed=native/linux_x11.c");
    let mut build = cc::Build::new();
    build.file("native/linux_x11.c").flag("-std=c11");
    for library in ["x11", "xext", "xrender", "xfixes", "pangocairo"] {
        let found = pkg_config::Config::new()
            .probe(library)
            .expect("Linux overlay development libraries are required");
        for include in found.include_paths {
            build.include(include);
        }
    }
    build.compile("oink_locator_x11");
}
