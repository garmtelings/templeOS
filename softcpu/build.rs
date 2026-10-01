//! Compiles Bochs's CPU core (the `bochs` submodule, release 3.1, unmodified)
//! with our configuration (`include/`) and glue (`glue/`). Bochs's other
//! parts (devices, GUI, debugger) are not built: the board is TempleOS.exe's.
//!
//! The source list follows Bochs's own build for this configuration: every
//! source in cpu/, cpu/decoder, cpu/fpu, cpu/cpudb/{intel,amd} and
//! cpu/softfloat3e (with 8086-SSE), but not cpu/avx (AVX is off); plus
//! pc_system.cc and gui/paramtree.cc.

use std::path::{Path, PathBuf};

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "cc"))
        .collect();
    v.sort();
    v
}

fn main() {
    // Paths relative to this crate (Cargo runs build scripts from it): Bochs
    // puts __FILE__ in messages, and relative paths keep the build folder
    // out of the binary, so the exe stays reproducible.
    let here = PathBuf::new();
    let bochs = here.join("bochs/bochs");
    let cpu = bochs.join("cpu");
    if !cpu.join("cpu.cc").exists() {
        panic!(
            "softcpu/bochs is empty: fetch the Bochs sources with\n  git submodule update --init --depth 1 softcpu/bochs"
        );
    }
    assert!(Path::new("bochs").exists(), "build scripts run from the crate directory");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed=glue");
    println!("cargo:rerun-if-changed=bochs");

    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    let base = |b: &mut cc::Build| {
        b.cpp(true).warnings(false).opt_level(2).debug(false);
        b.define("_FILE_OFFSET_BITS", "64").define("_LARGE_FILES", None);
        if windows {
            b.define("WIN32", None).define("_CRT_SECURE_NO_WARNINGS", None);
        }
        if msvc {
            // /Brepro: no timestamps in the objects either.
            b.flag("/EHsc").flag("/Brepro");
        } else {
            b.flag("-w");
        }
        // The C++ runtime is linked once, below.
        b.cpp_link_stdlib(None);
    };

    // Softfloat, with the defines Bochs's Makefile gives it.
    let sf = cpu.join("softfloat3e");
    let mut b = cc::Build::new();
    base(&mut b);
    b.include(here.join("include")).include(&sf).include(sf.join("include")).include(&bochs);
    for d in ["SOFTFLOAT_FAST_INT64", "SOFTFLOAT_FAST_DIV32TO16", "SOFTFLOAT_FAST_DIV64TO32"] {
        b.define(d, None);
    }
    b.define("INLINE_LEVEL", "5");
    b.files(sources(&sf)).files(sources(&sf.join("8086-SSE")));
    b.compile("bochs_softfloat");

    // The CPU core, its support files and the glue. include/ comes first,
    // so its config.h and cpudb.h are the ones found.
    let mut b = cc::Build::new();
    base(&mut b);
    b.include(here.join("include"))
        .include(&bochs)
        .include(&cpu)
        .include(bochs.join("iodev"))
        .include(bochs.join("instrument/stubs"))
        .include(here.join("glue"));
    for d in ["", "decoder", "fpu", "cpudb/intel", "cpudb/amd"] {
        b.files(sources(&cpu.join(d)));
    }
    b.file(bochs.join("pc_system.cc")).file(bochs.join("gui/paramtree.cc"));
    b.files(sources(&here.join("glue")));
    b.compile("bochs_cpu");

    // The C++ runtime: MSVC's comes with the (static) CRT; MinGW's is linked
    // statically, so TempleOS.exe needs no libstdc++ DLL.
    if windows && !msvc {
        println!("cargo:rustc-link-lib=static:-bundle=stdc++");
    } else if !msvc {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
