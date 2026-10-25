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

The GNU toolchain (`x86_64-pc-windows-gnu`, e.g. with MSYS2's MinGW-w64) also
builds; it needs `windres` on the `PATH` for the executable's icon. Release
builds should use MSVC: only its linker can make Windows load the program's
DLL imports from System32 alone.

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

- **Tray icon (Linux)** uses the StatusNotifierItem protocol (KDE, waybar,
  most other desktops; GNOME needs the AppIndicator extension). OpenDelay
  checks for a tray host at startup; without one, closing the window quits
  instead of hiding it.
- **Global hotkeys** use system-wide key grabs on Windows and X11. On Wayland
  they go through the desktop's GlobalShortcuts portal (KDE Plasma, GNOME 48+,
  Hyprland), which asks you to confirm the shortcuts. Compositors without that
  portal (e.g. sway) allow no global shortcuts: bind your desktop's own
  shortcuts to the controller WebSocket or the HTTP API instead.
- **Rendering** uses OpenGL. If OpenGL is unusable (some VMs and remote
  desktops), OpenDelay restarts with its software renderer and remembers that
  in a `software-renderer` file next to the settings; delete the file to try
  OpenGL again.
- **One instance**: starting OpenDelay again brings the running window forward.
- **Start with the system** writes the per-user `Run` registry value on
  Windows and `~/.config/autostart/opendelay.desktop` on Linux.
- Settings live in `%APPDATA%\OpenDelay` / `~/.config/opendelay`, session logs
  in `%LOCALAPPDATA%\OpenDelay\logs` / `~/.local/share/opendelay/logs`
  (override with `OPENDELAY_CONFIG_DIR` / `OPENDELAY_DATA_DIR`).
- **LAN mode** (Settings → Network) lets an encoder on another computer
  publish to `rtmp://<this computer>:1935/<route>?key=<key>`; the key is on the
  Setup page, and connections from other computers without it are refused.
  "New key" there replaces it; streams already connected keep running.
  On Windows, allow OpenDelay through Windows Defender Firewall when it asks
  after the restart (for the network type the other computer is on).
- The RTMP listener uses port 1935 and the local web server port 3000; set
  `OPENDELAY_RTMP_PORT` / `OPENDELAY_HTTP_PORT` if another program uses them.
- **Encoder settings**: pointing OBS at OpenDelay means choosing the
  "Custom…" service, and OBS then stops applying the platform's recommended
  settings. In Advanced output mode its keyframe interval falls back to
  "auto" (about 8 s with x264). Twitch viewers may then see a black screen,
  and the delay can only switch on keyframes. Set **Keyframe Interval** to
  2 s and **Rate Control** to CBR under Settings → Output. OpenDelay warns
  about long keyframe intervals a few seconds after a stream starts.

### Checks

```sh
cargo test
cargo clippy --all-targets
cargo deny check          # cargo install cargo-deny
```

Developer tools: `cargo run -p opendelay-core --example serve` runs the relay
and web server without a window; `cargo run -p opendelay --example screenshot
--features screenshot -- DIR` renders every page offscreen.
The UI is in English. To add a language, add
`crates/app/lang/<code>/LC_MESSAGES/opendelay.po` (desktop, with an
`X-Language-Name` header) and `crates/core/web/lang/<code>.json` (OBS pages);
no code changes are needed, and a language picker appears in Settings.
`python3 scripts/extract_strings.py <po file>` lists untranslated strings.

After changing dependencies, regenerate the licence list:

```sh
python3 scripts/third_party_licenses.py > THIRD_PARTY_LICENSES.md
```

## Fuzzing

`fuzz/` holds [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets
(nightly toolchain) for the chunk parser, AMF0, media inspection and the
delay engine. It is not part of the workspace.

```sh
cd fuzz
cargo fuzz run -O chunk_reader -- -malloc_limit_mb=64 -rss_limit_mb=2048 -max_len=1048576
cargo fuzz run -O amf0         -- -malloc_limit_mb=64 -rss_limit_mb=2048
cargo fuzz run -O media        -- -malloc_limit_mb=64 -rss_limit_mb=2048
cargo fuzz run -O engine       -- -malloc_limit_mb=64 -rss_limit_mb=2048
```

`-malloc_limit_mb` turns a single oversized allocation (e.g. input that
expands into a huge structure) into a reported crash.

## Repository layout

| Path | Contents |
|---|---|
| `crates/rtmp` | Sans-IO RTMP: handshake, chunk stream, AMF0, FLV / Enhanced RTMP media inspection |
| `crates/core` | Relay, delay engine, health checks, settings, diagnostics, HTTP API and controller WebSocket |
| `crates/app` | Desktop application (Slint UI, tray, hotkeys, autostart) |
| `docs/` | Design decisions and the controller protocol |
| `fuzz/` | Fuzz targets (separate from the workspace) |
| `SPEC.md`, `DESIGN.md` | Behaviour and visual requirements |

## Releases and updates

The optional update check reads a release feed (GitHub "latest release"
format). A download is kept only if it matches a signed manifest: the
manifest's Ed25519 signature must verify, its version must be newer than the
running one, and the archive's SHA-256 must be the one it lists for this
platform. Release builds set three build-time variables; without them the
feed URL and help links are placeholders and every download is rejected
(fail closed):

| Variable | Meaning |
|---|---|
| `OPENDELAY_UPDATE_FEED` | release feed URL, e.g. `https://api.github.com/repos/OWNER/opendelay/releases/latest` |
| `OPENDELAY_UPDATE_KEY` | Ed25519 public key, 64 hex digits |
| `OPENDELAY_REPOSITORY` | project URL used by the Help page links |

To create the key (once) and sign a release with OpenSSL 3:

```sh
openssl genpkey -algorithm ed25519 -out release-key.pem      # once; keep it secret
openssl pkey -in release-key.pem -pubout -outform DER | tail -c 32 | xxd -p -c 64   # OPENDELAY_UPDATE_KEY

V=X.Y.Z
cat > manifest.json <<JSON
{ "version": "$V",
  "files": [
    { "platform": "linux-x86_64", "name": "opendelay-$V-linux-x86_64.tar.gz",
      "sha256": "$(sha256sum opendelay-$V-linux-x86_64.tar.gz | cut -d' ' -f1)" },
    { "platform": "windows-x86_64", "name": "opendelay-$V-windows-x86_64.zip",
      "sha256": "$(sha256sum opendelay-$V-windows-x86_64.zip | cut -d' ' -f1)" } ] }
JSON
openssl pkeyutl -sign -inkey release-key.pem -rawin -in manifest.json | base64 -w0 > manifest.json.sig
```

Upload both archives (names containing `linux-x86_64` / `windows-x86_64`),
`manifest.json` and `manifest.json.sig` as release assets.
