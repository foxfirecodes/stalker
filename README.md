# Stalker

Stalker reruns one local command when relevant files change, locally or over SSH.
It is useful for builds, tests, code generators, and syncing a remote project.

## Install

For a native development build with an embedded SSH helper:

```sh
cargo xtask build --native --release
```

Copy `target/release/stalker` into a directory on your `PATH`. Installation is
one binary; the helper is embedded inside it. Native builds support remotes
matching the build host. On Linux these development helpers use the host libc.

For local watching only, ordinary Cargo commands also work:

```sh
cargo run -- --watch . -- cargo test
```

## Use

Pass one or more paths to watch, then put the command after `--`:

```sh
stalker --watch . -- cargo test
```

Keep watching and rerunning after changes until the command succeeds, then exit:

```sh
stalker --watch . --until-success -- cargo test
```

Watch only Rust source files and restart a local server after a 300 ms quiet
period:

```sh
stalker --watch src --include 'src/**/*.rs' --debounce 300ms -- cargo run
```

Run from another directory, ignore generated files, and wait for the first
change before starting:

```sh
stalker --cwd /path/to/project --watch . --ignore target --no-initial-run -- make test
```

Useful options:

- `--watch PATH` — path to watch; repeat for more than one path.
- `--include GLOB` — rerun only for matching paths, relative to `--cwd`.
- `--ignore GLOB` — skip matching paths, relative to `--cwd`.
- `--debounce DURATION` — wait for changes to settle; defaults to `150ms`.
- `--no-initial-run` — do not run until a relevant change arrives.
- `--until-success` — stop watching and exit with code `0` after the first
  successful run. Failed runs keep watching for changes.
- `--markers` — put machine-readable start and end lines around each run.
- `--raw-output` — attach child output directly to the terminal, retaining
  terminal-aware color and formatting. Cannot be used with `--markers`.
- `--print-events` — print accepted filesystem events to standard error.
- `--no-gitignore` — include paths normally ignored by the repository.

Durations use whole-number `ms`, `s`, `m`, or `h` units, such as `150ms`,
`2s`, or `1m`.

## Watch over SSH

Watch a remote project and run rsync locally whenever it changes:

```sh
stalker --remote tinyarch.local \
  --remote-cwd '~/code/bigtube' --watch . \
  -- rsync -avz --delete --partial tinyarch.local:~/code/bigtube/ ./
```

`--remote` accepts OpenSSH destinations: SSH config aliases, `user@host`, IPv6,
and `ssh://user@host:port`. Stalker invokes the system `ssh`, which handles your
keys, agent, host-key checks, and proxy/jump-host configuration. Use SSH config
aliases to share connection settings with commands such as rsync; Stalker does
not modify the command's arguments.

`--cwd` is always the local command's working directory. `--remote-cwd` sets the
base for remote watch paths, include/ignore globs, and `.gitignore` discovery;
it defaults to the SSH login directory. Quote `~` so your local shell does not
expand it. The remote helper expands leading `~` and `~/` using the remote HOME;
other shell expansions are not performed. Remote paths need not exist locally.

Repeat `--ssh-option KEY=VALUE` to pass OpenSSH configuration options:

```sh
stalker --remote user@server --ssh-option Port=2222 \
  --ssh-option ProxyJump=bastion \
  --remote-cwd '~/project' --watch . -- make local-build
```

Stalker detects the remote OS/CPU, uploads its embedded helper to a private
`/tmp/stalker.*` directory (mode `0700`), verifies the exact payload against its
SHA-256, then runs it with mode `0500`. It waits until all watches are registered
before starting the initial local command. Filtering happens remotely; debounce
and command execution stay local. Paths are sent as data, preserving Unix path
bytes, including non-UTF-8 names.

The remote needs Linux or macOS on x86-64 or ARM64, a POSIX shell, `uname`,
`mktemp`, `dd`, `chmod`, `rm`, and `sha256sum` (Linux) or `shasum` (macOS), plus
writable, executable `/tmp`. The SSH session uses stdin/stdout with no PTY or
socket forwarding; remote shell startup files must keep stdout clean.

On normal shutdown the helper exits and its temporary directory is removed.
Heartbeats expire abandoned helpers after 45 seconds. Unexpected disconnects
stop the session and any active local command, with an error; automatic reconnect
is not implemented. Security assumes a trusted remote account and OS: other
unprivileged users cannot replace the uploaded helper, while the same account
and root remain trusted.

## Build for Linux and macOS

Full builds embed helpers for all four targets: Linux x86-64/ARM64 using static
musl, and macOS Intel/Apple Silicon. macOS builds default to macOS 11.0 or newer;
set `MACOSX_DEPLOYMENT_TARGET` to override that minimum.

Install [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) and Zig, add
the Rust targets, and point `SDKROOT` at a macOS SDK:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
  x86_64-apple-darwin aarch64-apple-darwin
export SDKROOT=/path/to/MacOSX.sdk

# All remote helpers embedded in a native main binary:
cargo xtask build --release

# Cross-compile all helpers AND all four main binaries, including macOS on Linux:
cargo xtask dist
```

`dist` writes the standalone executables to `target/dist/stalker-TARGET`.
Both commands build helpers first and fail if any required target cannot build;
helper artifacts stay in `target/helpers/` and are not installed separately.
Cargo's incremental cache avoids recompiling unchanged helpers.

For example, the cargo-zigbuild project's Linux container includes Zig, Rust
targets, and a macOS SDK:

```sh
docker run --rm -v "$PWD:/io" -w /io \
  ghcr.io/rust-cross/cargo-zigbuild cargo xtask dist
```

For faster development without cross-toolchain setup, use `cargo xtask build
--native`. `cargo xtask test` builds and embeds the native helper, then runs the
workspace tests, including deployment/protocol integration tests. Plain `cargo
build`, `cargo test`, and `cargo install --path .` support local watching and omit
embedded helpers unless `STALKER_HELPER_DIR` names a previously built helper
directory. For example, after a full build:

```sh
STALKER_HELPER_DIR="$PWD/target/helpers/bundled" cargo install --path . --bin stalker
```

## How it works

Stalker watches the given paths recursively using the operating system's file
watcher. It filters events inside `--cwd` against the watch roots, your include
and ignore globs, and repository `.gitignore` rules. Paths watched outside
`--cwd` are accepted based on their watch roots only; CWD-relative globs and
`.gitignore` rules do not apply to them. It starts the command once by default,
then groups rapid changes into one rerun after the debounce period.

Only one child command runs at a time. If files change while it runs, Stalker
starts one follow-up run as soon as the current command exits. Child output
passes through unchanged unless you enable `--markers`.

With `--until-success`, an exit code of `0` ends the session, including any
queued follow-up run. Nonzero exits, signals, and spawn failures keep watching
for changes. Combine it with `--no-initial-run` to wait for a change before the
first attempt.
