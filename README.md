# OpenDelay

OpenDelay is a free, open-source desktop app that sits between your encoder
(OBS Studio, Streamlabs, Aitum Vertical or any RTMP encoder) and your
streaming platforms. The encoder publishes to OpenDelay on your own machine;
OpenDelay relays the stream to the real ingest server and can switch a
broadcast delay on and off at any time without stopping the stream.

- Local RTMP relay (`rtmp://127.0.0.1:1935/<route>`) to `rtmp://` or `rtmps://`
  upstreams, several routes live at once.
- Instant delay: a rolling buffer lets the delay start by replaying from the
  past; going live drains the backlog and rejoins the live edge.
- Desktop UI, OBS dock and on-stream overlay, global hotkeys, tray menu, a local
  HTTP API and a WebSocket protocol for hardware controllers
  ([docs/controller-protocol.md](docs/controller-protocol.md)).
- No accounts, licensing, telemetry or analytics.

Licence: [GPL-3.0-only](LICENSE). Third-party components:
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).

## Building

You need Rust (stable, 1.88 or newer) from <https://rustup.rs>.

### Linux (x86_64, X11 and Wayland)

Install the system libraries for the window, fonts, tray and hotkeys.

Debian / Ubuntu:

```sh
sudo apt install build-essential pkg-config libfontconfig1-dev libxkbcommon-dev \
    libwayland-dev libx11-dev libgtk-3-dev libayatana-appindicator3-dev
```

Fedora:

```sh
sudo dnf install gcc pkgconf-pkg-config fontconfig-devel libxkbcommon-devel \
    wayland-devel libX11-devel gtk3-devel libayatana-appindicator-gtk3-devel
```

Arch:

```sh
sudo pacman -S base-devel pkgconf fontconfig libxkbcommon wayland libx11 gtk3 \
    libayatana-appindicator
```

Then:

```sh
cargo build --release
./target/release/opendelay
```

### Windows (x86_64)

Native: install the Rust MSVC toolchain and the Visual Studio Build Tools
("Desktop development with C++"), then run `cargo build --release`. The binary
is `target\release\opendelay.exe`.

Cross-compiling from Linux with [cargo-xwin](https://github.com/rust-cross/cargo-xwin)
(it downloads the MSVC CRT and Windows SDK on first use, which means accepting
Microsoft's licence terms):

```sh
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin
cargo xwin build --release --target x86_64-pc-windows-msvc
```

### Checks

```sh
cargo test
cargo clippy --all-targets
cargo deny check          # cargo install cargo-deny
```

After changing dependencies, regenerate the licence list:

```sh
python3 scripts/third_party_licenses.py > THIRD_PARTY_LICENSES.md
```

## Repository layout

| Path | Contents |
|---|---|
| `crates/rtmp` | Sans-IO RTMP: handshake, chunk stream, AMF0, FLV / Enhanced RTMP media inspection |
| `crates/core` | Relay, delay engine, health checks, settings, diagnostics, HTTP API and controller WebSocket |
| `crates/app` | Desktop application (Slint UI, tray, hotkeys, autostart) |
| `docs/` | Design decisions and the controller protocol |
| `SPEC.md`, `DESIGN.md` | Behaviour and visual requirements |
