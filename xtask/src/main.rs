//! Two-stage builds: helpers first, then binaries which embed those helpers.
use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!(
            "cargo xtask build [--native] [--release]\ncargo xtask test\ncargo xtask dist\n\nbuild: embed all four remote helpers, then build native stalker\n--native: embed only the host helper, using the normal Rust toolchain\ntest: build the native helper, then test the workspace with it embedded\ndist: cross-compile all helpers and all four main binaries\n\nFull builds require cargo-zigbuild, Zig, all four rustup targets, and a macOS SDK at SDKROOT."
        );
        return Ok(());
    }
    let action = args[0].as_str();
    if !matches!(action, "build" | "test" | "dist")
        || args[1..]
            .iter()
            .any(|arg| !matches!(arg.as_str(), "--native" | "--release"))
    {
        return Err("unknown arguments; run `cargo xtask --help`".into());
    }
    if action != "build" && args.len() != 1 {
        return Err("only build accepts --native/--release".into());
    }
    let native = action == "test" || args.iter().any(|arg| arg == "--native");
    let release = args.iter().any(|arg| arg == "--release");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    env::set_current_dir(root)?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new("rustc").arg("-vV").output()?;
    if !output.status.success() {
        return Err("rustc -vV failed".into());
    }
    let version = String::from_utf8(output.stdout)?;
    let host = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or("rustc did not report a host target")?;
    if native
        && !TARGETS.contains(&host)
        && !matches!(
            host,
            "x86_64-unknown-linux-gnu" | "aarch64-unknown-linux-gnu"
        )
    {
        return Err(format!("unsupported native helper target: {host}").into());
    }
    if !native {
        let sdk = env::var_os("SDKROOT").ok_or("set SDKROOT to a macOS SDK directory; cargo-zigbuild's Linux build image includes one (see README)")?;
        if !Path::new(&sdk).is_dir() {
            return Err("SDKROOT is not a directory".into());
        }
        let status = Command::new(&cargo)
            .args(["zigbuild", "--help"])
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            return Err("install cargo-zigbuild and Zig first (see README)".into());
        }
    }
    let target_dir =
        PathBuf::from(env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()));
    fs::create_dir_all(&target_dir)?;
    let target_dir = target_dir.canonicalize()?;
    let helpers = target_dir
        .join("helpers")
        .join(if native { host } else { "bundled" });
    fs::create_dir_all(&helpers)?;
    let targets = if native { vec![host] } else { TARGETS.to_vec() };
    for target in &targets {
        eprintln!("Building remote helper for {target}");
        let mut build = Command::new(&cargo);
        build.args([
            if native { "build" } else { "zigbuild" },
            "--locked",
            "--release",
            "--features",
            "remote-agent",
            "--bin",
            "stalker-agent",
            "--target",
            target,
        ]);
        build
            .env_remove("STALKER_HELPER_DIR")
            .env("CARGO_TARGET_DIR", &target_dir);
        deployment_target(&mut build, target);
        checked(&mut build)?;
        fs::copy(
            target_dir.join(target).join("release/stalker-agent"),
            helpers.join(target),
        )?;
    }
    if action == "dist" {
        let destination = target_dir.join("dist");
        fs::create_dir_all(&destination)?;
        for target in TARGETS {
            eprintln!("Building bundled stalker for {target}");
            let mut build = Command::new(&cargo);
            build
                .args([
                    "zigbuild",
                    "--locked",
                    "--release",
                    "--bin",
                    "stalker",
                    "--target",
                    target,
                ])
                .env("STALKER_HELPER_DIR", &helpers)
                .env("CARGO_TARGET_DIR", &target_dir);
            deployment_target(&mut build, target);
            checked(&mut build)?;
            let path = destination.join(format!("stalker-{target}"));
            fs::copy(target_dir.join(target).join("release/stalker"), &path)?;
            println!("{}", path.display());
        }
    } else {
        let mut command = Command::new(&cargo);
        if action == "test" {
            command.args(["test", "--locked", "--workspace"]);
        } else {
            command.args(["build", "--locked", "--bin", "stalker"]);
            if release {
                command.arg("--release");
            }
        }
        checked(
            command
                .env("STALKER_HELPER_DIR", &helpers)
                .env("CARGO_TARGET_DIR", &target_dir),
        )?;
    }
    Ok(())
}

fn deployment_target(command: &mut Command, target: &str) {
    if target.ends_with("apple-darwin") && env::var_os("MACOSX_DEPLOYMENT_TARGET").is_none() {
        command.env("MACOSX_DEPLOYMENT_TARGET", "11.0");
    }
}

fn checked(command: &mut Command) -> Result<(), Box<dyn Error>> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} failed with {status}").into());
    }
    Ok(())
}
