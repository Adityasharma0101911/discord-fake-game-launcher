use std::{env, path::Path, process::Command};

fn main() {
    // Build the fake-game exe so main.rs can embed it (include_bytes!(env!("DUMMY_EXE"))).
    let dummy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/dummygame");
    let status = Command::new(env::var("CARGO").unwrap())
        .args(["build", "--release", "--manifest-path"])
        .arg(dummy.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(dummy.join("target"))
        .status()
        .expect("failed to run cargo for src/dummygame");
    assert!(status.success(), "building src/dummygame failed");

    println!("cargo:rustc-env=DUMMY_EXE={}", dummy.join("target/release/dummygame.exe").display());
    println!("cargo:rerun-if-changed={}", dummy.join("src").display());
    println!("cargo:rerun-if-changed={}", dummy.join("Cargo.toml").display());

    tauri_build::build()
}
