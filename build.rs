fn main() {
    const ICON: &str = "LCDSirPlus.ico";
    const CONFIG: &str = "lcdsirplus.txt";
    println!("cargo:rerun-if-changed={ICON}");
    println!("cargo:rerun-if-changed={CONFIG}");

    if std::env::var_os("CARGO_CFG_TARGET_OS").as_deref() == Some("windows".as_ref()) {
        winres::WindowsResource::new()
            .set_icon_with_id(ICON, "1")
            .compile()
            .expect("failed to compile LCDSirPlus.ico as Windows resource ID 1");
    }

    let out_dir = std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").expect("Cargo did not provide OUT_DIR"),
    );
    let build_dir = out_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("unexpected Cargo OUT_DIR layout");
    assert_eq!(
        build_dir.file_name(),
        Some(std::ffi::OsStr::new("build")),
        "unexpected Cargo OUT_DIR layout"
    );
    let profile_dir = build_dir
        .parent()
        .expect("Cargo OUT_DIR has no profile directory");
    let destination = profile_dir.join(CONFIG);
    std::fs::copy(CONFIG, &destination).unwrap_or_else(|error| {
        panic!(
            "failed to copy {CONFIG} to {}: {error}",
            destination.display()
        )
    });
    println!("cargo:rerun-if-changed={}", destination.display());
}
