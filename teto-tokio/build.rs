fn main() {
    // docs.rs builds in a network-isolated sandbox without F-Stack or DPDK and
    // only runs `cargo doc`, which compiles the crate but never links. Skip the
    // native link setup entirely so rustdoc succeeds without them.
    if std::env::var("DOCS_RS").is_ok() {
        return;
    }

    println!("cargo:rustc-link-search=native=/opt/f-stack/lib");
    println!("cargo:rustc-link-lib=static=fstack");

    println!("cargo:rustc-link-arg=-Wl,-z,nostart-stop-gc");

    let output = std::process::Command::new("pkg-config")
        .args(["--static", "--libs", "libdpdk"])
        .output()
        .expect("Failed to run pkg-config for libdpdk");
    let libs = String::from_utf8_lossy(&output.stdout);
    for token in libs.split_whitespace() {
        if let Some(dir) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else {
            println!("cargo:rustc-link-arg={}", token);
        }
    }
}
