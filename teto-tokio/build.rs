fn main() {
    println!("cargo:rustc-link-search=native=/opt/f-stack/lib");
    println!("cargo:rustc-link-lib=static=fstack");

    println!("cargo:rustc-link-arg=-Wl,-z,nostart-stop-gc");

    let output = std::process::Command::new("pkg-config")
        .args(&["--static", "--libs", "libdpdk"])
        .output()
        .expect("Failed to run pkg-config for libdpdk");
    let libs = String::from_utf8_lossy(&output.stdout);
    for token in libs.split_whitespace() {
        if token.starts_with("-L") {
            println!("cargo:rustc-link-search=native={}", &token[2..]);
        } else {
            println!("cargo:rustc-link-arg={}", token);
        }
    }
}
