use std::env;

fn main() {
    // Get the pdfium lib path from the pdfium-sys link metadata. The hayro
    // backend links no native PDF library, so it is normally absent.
    let lib_path = env::var("DEP_PDFIUM_LIB_PATH").ok();

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Tell the linker to set @loader_path as an rpath on macOS,
    // or $ORIGIN on Linux, so the .so can find libpdfium next to itself.
    match target_os.as_str() {
        "macos" => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
        }
        "linux" => {
            println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        }
        _ => {}
    }

    // Emit the lib path so maturin/CI scripts can find and bundle libpdfium
    if let Some(lib_path) = lib_path {
        println!("cargo:rustc-env=PDFIUM_LIB_DIR={lib_path}");
    }
}
