//! Builds the C++ shim and tells cargo how to link F-Stack and DPDK.
//!
//! Everything is emitted as `rustc-link-lib` / `rustc-link-search`, which
//! cargo propagates to every crate that (transitively) depends on teto-dpdk.
//! (`rustc-link-arg` would only apply to this package's own binaries and
//! tests, so a downstream application would fail to link.)
//!
//! Environment:
//! - `FF_PATH`: F-Stack source/build tree (default `/opt/f-stack`); headers and
//!   `libfstack.a` are taken from `$FF_PATH/lib`.
//! - `PKG_CONFIG_PATH`: where to find DPDK's `libdpdk.pc`.

use std::process::Command;

fn main() {
    // docs.rs builds in a network-isolated sandbox without F-Stack or DPDK and
    // only runs `cargo doc`, which compiles the crate but never links. Skip the
    // native cxx/pkg-config build entirely — the cxx bridge still expands to
    // pure-Rust FFI declarations, so rustdoc succeeds without them.
    if std::env::var("DOCS_RS").is_ok() {
        return;
    }

    println!("cargo:rerun-if-changed=src/sys.rs");
    println!("cargo:rerun-if-changed=cxx_layer/fstack_wrapper.h");
    println!("cargo:rerun-if-changed=cxx_layer/fstack_wrapper.cpp");
    println!("cargo:rerun-if-env-changed=FF_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    let ff_lib = format!("{}/lib", std::env::var("FF_PATH").unwrap_or_else(|_| "/opt/f-stack".into()));
    if !std::path::Path::new(&format!("{ff_lib}/libfstack.a")).exists() {
        panic!(
            "libfstack.a not found in {ff_lib}. Build F-Stack and set FF_PATH to its \
             source tree (default /opt/f-stack); see the project's Dockerfile."
        );
    }

    let dpdk = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libdpdk")
        .unwrap_or_else(|e| panic!("DPDK not found via pkg-config (set PKG_CONFIG_PATH): {e}"));

    let mut build = cxx_build::bridge("src/sys.rs");
    build
        .file("cxx_layer/fstack_wrapper.cpp")
        .include("cxx_layer")
        .include(&ff_lib)
        .flag_if_supported("-std=c++17")
        .flag_if_supported("-Wno-unused-parameter");
    for dir in &dpdk.include_paths {
        build.include(dir);
    }
    build.compile("cxx_layer");

    // F-Stack's FreeBSD code and DPDK's drivers register themselves through
    // constructors and linker sets that nothing references directly, so their
    // archives must be linked whole.
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    prelink_fstack(&ff_lib, &out_dir);
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static:+whole-archive,-bundle=fstack_teto");
    emit_dpdk_link_flags();
}

fn run(cmd: &mut Command) -> String {
    let output = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd:?}: {e}"));
    if !output.status.success() {
        panic!("{cmd:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Link `libfstack.a` into one relocatable object whose FreeBSD linker sets
/// (`set_sysinit_set`, ...) carry their own `__start_`/`__stop_` symbols.
///
/// FreeBSD finds SYSINITs, sysctls etc. by walking the section between the
/// linker-generated `__start_set_X`/`__stop_set_X` symbols. Linkers that
/// garbage-collect sections only referenced that way (lld, the default for
/// Rust on x86_64 Linux since 1.90) drop them, and the final link fails with
/// undefined `__start_set_*`. `-z nostart-stop-gc` avoids that, but a library
/// can't pass linker flags to its dependents' binaries. Defining the symbols
/// inside the sections turns them into ordinary references that keep the
/// sections alive with any linker.
fn prelink_fstack(ff_lib: &str, out_dir: &str) {
    let all = format!("{out_dir}/fstack_all.o");
    let merged = format!("{out_dir}/fstack_teto.o");
    let archive = format!("{out_dir}/libfstack_teto.a");
    let script = format!("{out_dir}/fstack_sets.ld");

    run(Command::new("ld").args(["-r", "--whole-archive", &format!("{ff_lib}/libfstack.a"), "-o", &all]));

    let headers = run(Command::new("readelf").args(["-S", "-W", &all]));
    let mut sets: Vec<&str> = headers
        .split_whitespace()
        .filter(|w| w.starts_with("set_") && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    sets.sort_unstable();
    sets.dedup();
    let mut ld_script = String::from("SECTIONS {\n");
    for set in &sets {
        ld_script.push_str(&format!(
            "  {set} : {{ PROVIDE(__start_{set} = .); KEEP(*({set})) PROVIDE(__stop_{set} = .); }}\n"
        ));
    }
    ld_script.push_str("}\n");
    std::fs::write(&script, ld_script).expect("write linker script");

    run(Command::new("ld").args(["-r", &all, "-T", &script, "-o", &merged]));
    let _ = std::fs::remove_file(&archive);
    run(Command::new("ar").args(["rcs", &archive, &merged]));
}

/// Translate `pkg-config --static --libs libdpdk` into propagating cargo
/// directives, keeping its `--whole-archive` grouping.
fn emit_dpdk_link_flags() {
    let output = Command::new("pkg-config")
        .args(["--static", "--libs", "libdpdk"])
        .output()
        .unwrap_or_else(|e| panic!("failed to run pkg-config: {e}"));
    if !output.status.success() {
        panic!(
            "pkg-config --static --libs libdpdk failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let flags = String::from_utf8(output.stdout).expect("pkg-config output is not UTF-8");

    let mut whole_archive = false;
    let mut linked = std::collections::HashSet::new();
    for token in flags.split_whitespace() {
        match token {
            "-Wl,--whole-archive" => whole_archive = true,
            "-Wl,--no-whole-archive" => whole_archive = false,
            // Only relevant for dynamically loaded driver plugins.
            "-Wl,--as-needed" | "-Wl,--export-dynamic" => {}
            "-pthread" => println!("cargo:rustc-link-lib=dylib=pthread"),
            _ => {
                if let Some(dir) = token.strip_prefix("-L") {
                    println!("cargo:rustc-link-search=native={dir}");
                } else if let Some(file) = token.strip_prefix("-l:") {
                    // e.g. -l:librte_eal.a (always inside the whole-archive group)
                    let name = file
                        .strip_prefix("lib")
                        .and_then(|f| f.strip_suffix(".a"))
                        .unwrap_or_else(|| panic!("unexpected pkg-config token {token}"));
                    let kind = if whole_archive { "static:+whole-archive,-bundle" } else { "static:-bundle" };
                    if linked.insert(name.to_owned()) {
                        println!("cargo:rustc-link-lib={kind}={name}");
                    }
                } else if let Some(name) = token.strip_prefix("-l") {
                    // DPDK repeats its libraries as plain -lrte_* after the
                    // whole-archive group; those are already linked.
                    if linked.insert(name.to_owned()) {
                        println!("cargo:rustc-link-lib=dylib={name}");
                    }
                } else {
                    panic!("unexpected pkg-config token {token}");
                }
            }
        }
    }
}
