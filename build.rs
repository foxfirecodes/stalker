use std::{env, fs, path::PathBuf};

const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    // Native development builds can use the host's GNU runtime.
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
];

fn main() {
    println!("cargo:rustc-check-cfg=cfg(embedded_helpers)");
    println!("cargo:rerun-if-env-changed=STALKER_HELPER_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    let mut registry = String::from("pub const HELPERS: &[(&str, &[u8])] = &[\n");
    if let Some(directory) = env::var_os("STALKER_HELPER_DIR") {
        let directory = PathBuf::from(directory)
            .canonicalize()
            .expect("resolve STALKER_HELPER_DIR");
        println!("cargo:rerun-if-changed={}", directory.display());
        let mut count = 0;
        for target in TARGETS {
            let path = directory.join(target);
            if path.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
                registry.push_str(&format!("({target:?}, include_bytes!({path:?})),\n"));
                count += 1;
            }
        }
        assert!(
            count > 0,
            "STALKER_HELPER_DIR contains no supported helper binaries"
        );
        println!("cargo:rustc-cfg=embedded_helpers");
    }
    registry.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("helpers.rs"),
        registry,
    )
    .unwrap();
}
