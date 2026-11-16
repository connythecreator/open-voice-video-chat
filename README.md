# ovvc

ovvc is a local-first, peer-to-peer chat and voice application, a Discord-style
workspace with no required server, where every commune is a signed append-only
event log that members replicate directly to each other.

A **commune** is what the UI calls a guild, a named group with channels,
categories, members and a policy. Each member keeps the full log on their own
machine, verifies every signature before indexing an event, and syncs with
other members over libp2p. Voice calls are end-to-end encrypted, use a direct
pair for two people, a full mesh for small calls, and a flat star with an
elected hub and per-listener mixing for larger calls.

This tree is the v0.1 development line, available at
<https://github.com/connythecreator/open-voice-video-chat>. It is not tagged yet.

## What v0.1 implements

The v0.1 surface is text communes and direct messages, presence, attachments,
search, reactions, replies, edit and delete, themes, settings, the setup flow,
the network map and star voice.

* **Signed, replicated history.** Guilds, channels, categories, messages,
  replies, mentions, edits, deletes, reactions, reads, member profiles and
  direct messages are Ed25519-signed events in an append-only log. A peer
  verifies and validates an event before it enters the local store.
* **Local-first operation.** A commune with no reachable peer still works from
  the local store, and the next sync fills the gaps.
* **Peer-to-peer networking.** libp2p over TCP with Noise and Yamux, a
  request-response sync protocol, mDNS, Kademlia discovery, relay and DCUtR
  hole punching, UPnP and NAT-PMP.
* **Text and direct messages.** Communes, categories, text channels, messages,
  replies, mentions, edits, deletes, search, reactions, read markers,
  content-addressed attachments up to 25 MiB, member profiles and one-to-one
  conversations.
* **Presence and setup.** Display names, about text, pronouns and links,
  presence, typing indicators, the first-run setup flow and the network map.
* **Star voice.** 48 kHz mono Opus, end-to-end encryption with per-call X25519
  key agreement and ChaCha20-Poly1305, a direct pair for two members, a mesh up
  to six members, and a flat star above that with an elected hub that decodes
  each leaf once, mixes per listener and re-encrypts a separate downlink. The
  frontends report the real `media star` path and the `keys leaf->hub` key
  rule.
* **Two media transports.** WebRTC audio through `webrtc-rs` and a libp2p
  media fallback, both carrying the same encrypted packet.
* **One daemon, several frontends.** The daemon owns the store, the identity
  and every network node, and the CLI, TUI and GUI attach over a local Unix
  socket. The GUI starts a daemon when none is running.
* **Themes and settings.** Built-in defaults, a system file, system drop-ins, a
  user file, a named profile, environment variables and flags, in that order.
  Themes load from `theme.toml` or a named preset.
* **A supporting relay server.** `ovvc-server` provides a libp2p relay for
  peers that cannot connect directly. It is never required by a commune, and
  there is no relay-only mode.
* **A null audio backend.** The whole audio path runs without a sound card,
  which is what the CI and the multi-daemon tests use.

## What v0.1 does not implement

The GUI and TUI do not expose video, camera, screen share, relay-only,
hide-my-IP, plugin, sandbox, avatar-picking or channel-deletion controls, and
the policy view is read-only.

* **Video and screen sharing.** Video is a descriptor and a negotiated codec
  name only. There is no camera capture, screen-share operation, video track or
  live video codec path.
* **Plugins.** There is no plugin loader, sandbox or directory browser.
* **Policy enforcement.** Guild policy is a signed, versioned read model. The
  settings and policy views are read-only, and enforcement, signed policy
  events and policy-driven media keys are not in v0.1.
* **Server-authoritative communes and sharding.** The storage trait and the
  event model leave room for both, but the shipped v0.1 replicates the full
  history and does not shard it.
* **Relay-only mode, hiding your IP, avatars and channel deletion.**
* **Windows and WSL parity.** The Windows work lives in a separate fork and is
  parked until after the star media system. Linux is the supported platform
  for v0.1.
* **Mobile and web frontends.**

## Prerequisites

* **Rust 1.95 or newer.** That is the real workspace floor because the locked
  egui 0.37 and eframe 0.37 stack requires it. Install with
  [rustup](https://rustup.rs/).
* **A C toolchain, CMake and pkg-config.** `audiopus_sys` builds libopus from
  source.
* **Linux GUI development libraries** for the desktop client. On Debian or
  Ubuntu:

  ```sh
  sudo apt-get install build-essential cmake pkg-config \
    libx11-dev libxcursor-dev libxi-dev libxrandr-dev libxkbcommon-dev \
    libwayland-dev libegl1-mesa-dev libgl1-mesa-dev libvulkan-dev
  ```

* **A Vulkan driver** for the GUI tests. The software rasteriser is enough:

  ```sh
  sudo apt-get install mesa-vulkan-drivers
  ```

* **PipeWire development headers** only if you want the live audio backend.
  `scripts/audio-dev-env.sh` stages them under `target/` without root, or
  install `libpipewire-0.3-dev` and `libspa-0.2-dev` with your package
  manager.

## Build

```sh
cargo build --release
```

The binaries land in `target/release`:

| Binary | Purpose |
| --- | --- |
| `ovvc` | Headless CLI |
| `ovvc-tui` | Terminal client |
| `ovvc-gui` | egui desktop client |
| `ovvc-daemon` | Local store, IPC server and network node |
| `ovvc-server` | Optional libp2p relay |

The default build uses the silent null audio backend. Build the daemon with
the live PipeWire backend when the headers are staged:

```sh
source scripts/audio-dev-env.sh
cargo build --release -p ovvc-daemon --features pipewire
```

### Static musl build (headless)

The daemon, CLI, TUI and relay server can be built statically for
`x86_64-unknown-linux-musl`. Install the musl C toolchain first, then run the
script:

```sh
sudo apt-get install musl-tools cmake
scripts/build-musl.sh
```

The script installs the Rust target when missing, sets the musl compiler, and
sets `LIBOPUS_NO_PKG=1` so `audiopus_sys` builds Opus from source for musl
instead of taking a glibc `opus.pc` from the host. On Arch, install `musl` and
`cmake` instead of `musl-tools`. The GUI is not part of this target, because a
static musl GUI cannot load its window libraries.

### Musl GUI build (Alpine)

A static musl GUI cannot work. The window stack loads X11, Wayland, xkbcommon
and Vulkan with `dlopen`, and a static musl binary has no dynamic loader, so
`dlopen` fails with `Dynamic loading not supported`. Build the GUI as a
dynamically linked musl binary for a musl userland such as Alpine:

```sh
docker run --rm -v "$PWD:/src" -w /src rust:1.100.0-alpine sh -c '
  apk add --no-cache bash build-base cmake &&
  scripts/build-musl-gui.sh'
```

The binary needs X11 or Wayland, `libxkbcommon` and a Vulkan driver on the
machine that runs it. It does not run on a glibc desktop. On a glibc desktop,
build the normal GUI with `cargo build --release -p ovvc-gui` instead.

## Run

The desktop client starts its own daemon, so this is the shortest path:

```sh
cargo run -p ovvc-gui
```

The terminal client attaches to the same daemon:

```sh
cargo run -p ovvc-tui
```

A headless first run looks like this:

```sh
cargo run -p ovvc-cli -- init
cargo run -p ovvc-cli -- guild create "my commune"
cargo run -p ovvc-cli -- channel create "my commune" general
cargo run -p ovvc-cli -- send "my commune" general "hello"
cargo run -p ovvc-cli -- chat "my commune" general
```

To invite a second machine, give the commune a dialable address and mint a
signed invite:

```sh
cargo run -p ovvc-cli -- guild invite "my commune" \
  --peer /ip4/203.0.113.10/tcp/41000
```

The other machine joins with the printed code:

```sh
cargo run -p ovvc-cli -- guild join '<invite code>'
```

`ovvc doctor` prints local health checks, `ovvc config show` prints the
resolved configuration, and `ovvc --help` lists the rest. Data lives in the
platform data directory, or wherever `OVVC_DATA_DIR` or the configuration
points.

Voice channels are created from the GUI or TUI, then joined with
`ovvc voice "my commune" join <channel>` or the matching frontend command.

## Test

The unit and integration suite runs with:

```sh
cargo test --workspace
```

Several daemon tests start real processes and real libp2p links. They are
slower than the library tests, and a few are clearer with one test thread:

```sh
cargo test -p ovvc-daemon -- --test-threads=1
```

Focused media verification:

```sh
scripts/star-media-verify.sh          # star key rule and three-daemon star media
scripts/star-media-verify.sh --direct # two-person libp2p and WebRTC calls
scripts/star-media-verify.sh --all    # the above plus takeover and jitter
```

Infrastructure tests that need a display, DBus, a pseudo terminal or a Vulkan
rasteriser:

```sh
scripts/test-all-infra.sh
```

Fuzzing uses `cargo-fuzz` and the nightly toolchain:

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run fuzz_event
```

## Documentation

* [Architecture](docs/ARCHITECTURE.md)
* [Protocol](docs/PROTOCOL.md)
* [Media](docs/MEDIA.md)
* [Roadmap](docs/ROADMAP.md)
* [Contributing](CONTRIBUTING.md)
* [Security policy](SECURITY.md)
* [Changelog](CHANGELOG.md)

## License

ovvc is licensed under the GNU Affero General Public License, version 3 or
later. See [LICENSE](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you is licensed under the same
AGPL-3.0-or-later terms, and you keep the copyright in your contribution.
