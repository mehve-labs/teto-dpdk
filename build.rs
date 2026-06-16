fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=src/config.rs");
    println!("cargo:rerun-if-changed=src/fstack.rs");
    println!("cargo:rerun-if-changed=cxx_layer/fstack_wrapper.h");
    println!("cargo:rerun-if-changed=cxx_layer/fstack_wrapper.cpp");

    println!("cargo:rustc-link-search=native=/opt/f-stack/lib");
    println!("cargo:rustc-link-lib=static=fstack");

    let mut build = cxx_build::bridge("src/fstack.rs");

    build
        .file("cxx_layer/fstack_wrapper.cpp")
        .include("cxx_layer")
        .include("/opt/f-stack/lib") // F-Stack headers
        .flag_if_supported("-std=c++17")
        .flag_if_supported("-Wno-unused-parameter");

    if let Ok(dpdk) = pkg_config::Config::new().cargo_metadata(false).probe("libdpdk") {
        for header_path in &dpdk.include_paths {
            build.include(header_path);
        }
    } else {
        build.include("/usr/local/include");
    }

    build.compile("cxx_layer");
    
    // DPDK uses custom ELF sections for initialization which get stripped.
    println!("cargo:rustc-link-arg=-Wl,-z,nostart-stop-gc");
    
    // Manually pass pkg-config static libs to rustc to preserve --whole-archive
    let output = std::process::Command::new("pkg-config")
        .args(&["--static", "--libs", "libdpdk"])
        .output()
        .expect("Failed to run pkg-config");
    let libs = String::from_utf8_lossy(&output.stdout);
    for token in libs.split_whitespace() {
        if token.starts_with("-L") {
            println!("cargo:rustc-link-search=native={}", &token[2..]);
        } else {
            println!("cargo:rustc-link-arg={}", token);
        }
    }
}
