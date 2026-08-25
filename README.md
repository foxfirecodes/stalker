# Stalker

Stalker reruns one command when relevant files change. It is useful for local
builds, tests, and code generators that should stay up to date while you work.

## Install

Install from this checkout with Rust and Cargo:

```sh
cargo install --path .
```

Or run it without installing:

```sh
cargo run -- --watch . -- cargo test
```

## Use

Pass one or more paths to watch, then put the command after `--`:

```sh
stalker --watch . -- cargo test
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
- `--markers` — put machine-readable start and end lines around each run.
- `--print-events` — print accepted filesystem events to standard error.
- `--no-gitignore` — include paths normally ignored by the repository.

Durations use whole-number `ms`, `s`, `m`, or `h` units, such as `150ms`,
`2s`, or `1m`.

## How it works

Stalker watches the given paths recursively using the operating system's file
watcher. It filters events against the watch roots, your include and ignore
globs, and repository `.gitignore` rules. It starts the command once by
default, then groups rapid changes into one rerun after the debounce period.

Only one child command runs at a time. If files change while it runs, Stalker
starts one follow-up run as soon as the current command exits. Child output
passes through unchanged unless you enable `--markers`.
