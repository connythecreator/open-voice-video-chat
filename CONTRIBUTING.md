# Contributing to ovvc

Thanks for taking the time to work on ovvc. The project is pre-1.0, so the
wire formats and the internal crate boundaries can still move, but the rules
below keep a change reviewable and keep a release build honest.

By taking part you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## Before you start

* Search the existing issues and pull requests before opening a new one.
* For a large change, open an issue first and describe the problem, the
  proposed shape and the tests you expect to add.
* Do not include a local notes file, a design export, a build directory, a
  test artifact or a machine-specific script in a pull request. The
  `.gitignore` covers the usual names, and `*.local.md` is deliberately
  reserved for notes that live outside the repository.
* Report a security problem privately, following [SECURITY.md](SECURITY.md).
  Do not open a public issue for a vulnerability.

## Build and test

The workspace needs Rust 1.95+, a C compiler for `ring`, and the GUI libraries
from the [README](README.md). CMake and pkg-config are only needed for live
audio.

```sh
cargo build --workspace
cargo test --workspace
```

Static musl: install `musl-tools` (`musl` on Arch) and run
`scripts/build-musl.sh`. The GUI uses `scripts/build-musl-gui.sh` on Alpine and
cannot be static musl.

The daemon tests start real processes and real libp2p links. Some suites are
clearer with one test thread:

```sh
cargo test -p ovvc-daemon -- --test-threads=1
```

The focused media and infrastructure checks are described in the README and in
`scripts/`. A change that touches the media plane should run
`scripts/star-media-verify.sh`, and a change that touches the GUI, the TUI, the
tray, notifications or the relay server should run `scripts/test-all-infra.sh`
when the host has the required display, DBus and Vulkan packages.

## Style and checks

Run these before you open a pull request:

```sh
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The historical tree is not rustfmt-clean in every crate, because the GUI, TUI
and media work landed from parallel branches. The CI checks the Rust files a
change touches, so format the files you edit and leave the rest alone:

```sh
rustfmt --check --edition 2024 --config skip_children=true path/to/file.rs
```

A dedicated formatting pass over the whole workspace is welcome once the
parallel branches have landed, and it should be a separate commit so it does
not hide a behavioural change.

* Keep the existing crate boundaries. A frontend should not reach around
  `ovvc-client` or `ovvc-remote`, and a transport should not own durable state.
* Prefer a typed error over a string, and classify a new error where the
  existing `Diagnostic` and `ErrorClass` path expects it.
* Add a test at the layer where the behaviour lives. A storage rule belongs in
  `ovvc-core` or `ovvc-store`, a wire rule belongs beside the codec, and a
  real-process path belongs in the daemon integration tests.
* Do not weaken an assertion to make a flaky test pass. Fix the timing, the
  isolation or the missing event instead.
* Keep a public comment accurate. If a private note is referenced, move the
  useful content into `docs/` and remove the private reference.

## Wire format changes

ovvc refuses an incompatible peer at the boundary instead of misreading a
frame. When you change a wire shape:

* Bump the version for the surface you changed, and follow the rule documented
  in [docs/PROTOCOL.md](docs/PROTOCOL.md).
* Add a test that an older frame is refused, or that the migration path still
  reads the old shape.
* Update the version summary in `docs/PROTOCOL.md` and the relevant crate
  documentation.
* If the change affects voice, update `docs/MEDIA.md` as well.

## Commit and pull request

* Write a short imperative commit subject, then explain the reason in the body
  when the change is not obvious. Keep tool output, agent names and worktree
  paths out of the message.
* Keep a pull request focused. A cleanup, a wire change and a new feature are
  easier to review as separate changes.
* Describe the verification you actually ran. A unit test run is useful, and a
  real process run is better for a path that crosses a socket or a peer link.
* Do not include generated binaries, screenshots, `.local.md` notes or a
  `target/` directory.

## License

Unless you explicitly state otherwise, any contribution you intentionally
submit for inclusion in this project is licensed under AGPL-3.0-or-later, as
described in the [README](README.md) and [LICENSE](LICENSE). You keep the
copyright in your contribution.
