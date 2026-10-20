use std::env;
use std::path::Path;

fn main() {
    // Declared so `#[cfg(have_libsetjmp)]` in wasi_stubs.rs doesn't trip the
    // unexpected-cfgs lint on toolchains that check it.
    println!("cargo:rustc-check-cfg=cfg(have_libsetjmp)");

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "wasm32" {
        return;
    }

    // pdfium's wasm build links libsetjmp.a for __c_longjmp, but that archive's
    // single object also defines the __wasm_setjmp/__wasm_longjmp helpers that
    // Rust's own codegen already emits. The definitions are identical LLVM SjLj
    // helpers, so allow the linker to keep the first and resolve the otherwise
    // missing __c_longjmp from the same archive. Must live here (the final
    // cdylib) because rustc-link-arg is not propagated from dependency crates.
    println!("cargo:rustc-link-arg=--allow-multiple-definition");

    // pdfium-sys (links = "pdfium") exports the resolved pdfium lib dir as
    // DEP_PDFIUM_LIB_PATH. Newer pdfium wasm releases ship a standalone
    // libsetjmp.a providing the *real* __wasm_longjmp, which throws the
    // __c_longjmp WASM exception tag so FreeType's setjmp error recovery can
    // unwind. When that archive is linked we set `have_libsetjmp` so the
    // fallback setjmp/longjmp stubs in wasi_stubs.rs are dropped — otherwise the
    // trapping __wasm_longjmp stub shadows the real one (link order +
    // --allow-multiple-definition keep the first definition) and any FreeType
    // longjmp aborts the whole module. Older pdfium builds fold setjmp into
    // libc.a with no libsetjmp.a; there the stubs remain the only providers.
    if let Ok(lib_path) = env::var("DEP_PDFIUM_LIB_PATH") {
        if Path::new(&lib_path).join("libsetjmp.a").exists() {
            println!("cargo:rustc-cfg=have_libsetjmp");
        }
    }
}
