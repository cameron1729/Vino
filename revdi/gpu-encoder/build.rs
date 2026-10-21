use std::{env, path::PathBuf, process::Command};

fn main() {
    println!(
        "cargo:rustc-env=VINO_BUILD_FLAGS={}",
        env::var("CARGO_ENCODED_RUSTFLAGS")
            .unwrap_or_default()
            .replace('\u{1f}', " ")
    );
    for name in ["encode", "compare"] {
        let source = format!("shaders/{name}.comp");
        println!("cargo:rerun-if-changed={source}");
        let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join(format!("{name}.spv"));
        let status = Command::new("glslc")
            .args(["--target-env=vulkan1.2", "-O", &source, "-o"])
            .arg(output)
            .status()
            .expect("glslc is required: install the glslc package");
        assert!(status.success(), "shader compilation failed: {source}");
    }
}
