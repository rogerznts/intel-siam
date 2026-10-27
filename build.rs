use std::env;
use std::hash::{DefaultHasher, Hasher};
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=helper/t2u_tunnel.c");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // libt2u.so is x86_64-only, so the helper is always x86_64, whatever the app's own arch.
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("t2u_tunnel");
    let status = Command::new("cc")
        .args(["-arch", "x86_64", "-O2", "-Wall", "-Werror", "-o"])
        .arg(&out)
        .arg("helper/t2u_tunnel.c")
        .status()
        .expect("run cc for helper/t2u_tunnel.c");
    assert!(status.success(), "cc failed for helper/t2u_tunnel.c");

    let bytes = std::fs::read(&out).expect("read built helper");
    let mut hasher = DefaultHasher::new();
    hasher.write(&bytes);
    println!("cargo:rustc-env=T2U_HELPER_HASH={:016x}", hasher.finish());
}
