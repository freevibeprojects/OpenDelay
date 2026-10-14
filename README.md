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

cargo-xwin needs `clang-cl`, `llvm-lib` and `lld-link` on the `PATH`. If your
distribution's LLVM/LLD packages are not installed, the Rust toolchain's
copies work through symlinks (both are multi-call binaries):

```sh
rustup component add llvm-tools
B=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin
mkdir -p ~/.local/xwin-bin && ln -sf $B/llvm-ar ~/.local/xwin-bin/llvm-lib && ln -sf $B/rust-lld ~/.local/xwin-bin/lld-link
PATH=~/.local/xwin-bin:$PATH cargo xwin build --release --target x86_64-pc-windows-msvc
```

### Platform notes

- **Tray icon (Linux)** uses the StatusNotifierItem protocol (KDE, most other
  desktops; GNOME needs the AppIndicator extension). Because a missing tray
  would leave a hidden window unreachable, closing the window quits on Linux
  unless you choose "Keep in tray" in Settings.
- **Global hotkeys** work on Windows and X11. Wayland compositors do not allow
  global key grabs; bind your desktop's own shortcuts to the controller
  WebSocket or the HTTP API instead (see the controller protocol).
- **Start with the system** writes the per-user `Run` registry value on
  Windows and `~/.config/autostart/opendelay.desktop` on Linux.
- Settings live in `%APPDATA%\OpenDelay` / `~/.config/opendelay`, session logs
  in `%LOCALAPPDATA%\OpenDelay\logs` / `~/.local/share/opendelay/logs`
  (override with `OPENDELAY_CONFIG_DIR` / `OPENDELAY_DATA_DIR`).
- The RTMP listener uses port 1935 and the local web server port 3000; set
  `OPENDELAY_RTMP_PORT` / `OPENDELAY_HTTP_PORT` if another program uses them.

### Checks

```sh
cargo test
cargo clippy --all-targets
cargo deny check          # cargo install cargo-deny
```

Developer tools: `cargo run -p opendelay-core --example serve` runs the relay
and web server without a window; `cargo run -p opendelay --example screenshot
--features screenshot -- DIR` renders every page offscreen.
`python3 scripts/extract_strings.py crates/app/lang/fr/LC_MESSAGES/opendelay.po`
lists untranslated desktop strings.

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

## Releases and updates

The optional update check reads the release feed set in
`crates/core/src/update.rs` (`FEED_URL`, GitHub "latest release" format; a
placeholder until the project is published). An update is only kept after its
Ed25519 signature verifies against `PUBLIC_KEY` (also a placeholder: all
downloads are rejected until it is set). To sign a release archive with
OpenSSL 3:

```sh
openssl genpkey -algorithm ed25519 -out release-key.pem      # once; keep it secret
openssl pkey -in release-key.pem -pubout -outform DER | tail -c 32 | xxd -i   # PUBLIC_KEY bytes
openssl pkeyutl -sign -inkey release-key.pem -rawin -in opendelay-X.Y.Z-windows-x86_64.zip \
    | base64 -w0 > opendelay-X.Y.Z-windows-x86_64.zip.sig
```

Upload the archive (its name must contain `windows-x86_64` or `linux-x86_64`)
and the `.sig` file as release assets.
