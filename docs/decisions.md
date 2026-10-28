# Design decisions

Choices made where `SPEC.md` / `DESIGN.md` leave room for interpretation.
Each entry: the decision, then why.

## Build and licensing

- **Dependencies are checked with `cargo deny`** (`deny.toml`) for the Linux and
  Windows targets only. Workspace crates are private (`publish = false`) and
  GPL-3.0-only.
- **`THIRD_PARTY_LICENSES.md` is generated** from `cargo metadata` by
  `scripts/third_party_licenses.py`, so it cannot drift from `Cargo.lock`.

## RTMP

- **Plain handshake only.** The listener answers with a zero version field in
  S1, which makes digest-capable clients (librtmp, FFmpeg) fall back to the
  plain version-3 handshake; the upstream client sends a plain C1. RTMPE
  (version 6) is rejected.
- **Format 3 chunk that starts a new message** reuses the previous delta, and
  the delta after a format 0 header is 0 (what librtmp-based encoders such as
  OBS produce). Our writer never emits format 3 for a new message, only for
  continuation chunks, so upstream servers never face that ambiguity.
- **Extended timestamps on format 3 chunks** are written (RTMP 1.0 §5.3.1.3)
  and, when reading, consumed only if they repeat the stored value, so peers
  that omit them still parse.
- **Aggregate messages** (type 22) from an encoder are split into their
  audio/video/data messages before buffering, so every buffered message has
  its own timestamp and keyframe flag.
- **Keyframes for switching** are keyframes on the default track (track 0, or
  any single-track message). Enhanced RTMP ModEx blocks are skipped.
- **At most 64 chunk streams** per connection are tracked (memory bound for
  untrusted LAN peers).

## Relay

- **Publish negotiation.** On the encoder's `connect` the relay connects to the
  upstream with the encoder's own command object (only `app` and `tcUrl`
  rewritten, so Enhanced RTMP capability fields pass through) and relays the
  platform's `_result`/`_error`. `releaseStream`, `FCPublish` and
  `createStream` are answered locally (stream id 1). On `publish` the relay
  runs `releaseStream`/`FCPublish`/`createStream`/`publish` upstream with the
  same key and relays the platform's `onStatus` (including rejections such as
  `NetStream.Publish.BadName`) to the encoder.
- **Upstream recovery**: up to 8 attempts with exponential back-off from 0.5 s
  (capped at 15 s, about one minute in total). After that the encoder is
  disconnected so its own reconnect logic takes over. Status shows the attempt
  number and last error. A server `onStatus` with level `error` or
  `NetConnection.Connect.ReconnectRequest` also triggers recovery.
- **After a reconnect** output resumes at the latest buffered keyframe, after
  re-sending metadata and sequence headers; upstream timestamps continue from
  where the old connection stopped (one frame later).
- **A slow upload skips ahead, it does not drift.** Output more than 5 s behind
  its schedule jumps to the newest keyframe that is due, like an encoder
  dropping frames under congestion. Without this the real delay grew silently
  past the one shown (90 s against 30 s in testing) until the connection
  stalled. `currentMs` includes the lag, and an `upload_slow` finding shows
  while it is behind or skipped in the last 30 s.
- **Encoder stops while content is still buffered** (delay active): the relay
  keeps sending the buffered tail at its normal pace, then unpublishes.
- **An encoder reconnecting continues the old session.** A new connection from
  the same machine replaces the session on its route only when it publishes
  with the same stream key; until then it is answered locally and touches
  nothing, so a program that merely connects (or guesses a key) cannot end
  the stream. A different key is refused while the old session still sends.
  With the delay on, the new session continues the old buffer and platform
  connection: the unsent tail goes out first, then the new stream on the
  same timeline, and the platform sees no reconnect. If the platform
  connection was lost meanwhile, a new one is opened and the buffer resumes
  from a keyframe.
- **An encoder reconnecting keeps the delay.** When a route's session ends
  with the delay on (OBS crash, network blip, restart), an encoder publishing
  on that route within 10 minutes starts already delayed by the same amount:
  nothing is sent until its media is that old, so viewers see a pause, never
  undelayed content. Going live before the session ends clears this.
  Routes sending its stream start as far behind as their source.
- **Emergency stop is latched.** Disconnecting alone is not enough, because
  OBS reconnects automatically within seconds. After an emergency stop new
  publishes are refused (`NetConnection.Connect.Rejected`) until the user
  allows streaming again from any control surface.
- **Graceful shutdown** closes the listener first (new connections are
  refused at TCP level), waits up to 10 s for streams to end, then disconnects
  the rest.

## Delay engine

- **Timing uses arrival time.** A message is due `delay` after it arrived from
  the encoder (not after its RTMP timestamp), so encoder hiccups are replayed
  faithfully and the delay is wall-clock accurate.
- **Switches land on the delay, not past it.** Enabling (and changing) the
  delay jumps when a buffered keyframe becomes `delay + 200 ms` old (within
  250 ms), so viewers see the last `delay` seconds again and the switch
  comes at most one keyframe interval after the request. An earlier version
  jumped at once to the newest keyframe that was old enough, which set
  viewers back by up to one keyframe interval more than requested. The
  achieved delay is reported as the current delay; the requested value is
  the target.
- **Growing ("buffering")**: when no keyframe is old enough yet, viewers stay
  live and jump back once one is. Output never pauses: an earlier version
  paused until content aged, and Twitch closed the connection after 30 s
  without data. Another replayed what was buffered at once and jumped back a
  second time later; viewers saw two repeats.
- **"Buffer not ready"** is returned only when nothing usable has been
  buffered yet (no keyframe received), since growing handles every other case.
- **Requested delay longer than the buffer**: the buffer kept grows to the
  delay and the message says the delay starts once the buffer has filled. The
  buffer setting stays what the user chose, so the buffer shrinks back when
  the delay is shortened (a single long delay used to keep its memory for
  the rest of the session).
- **Changing the delay while active**: longer jumps back, shorter jumps
  forward, each when a keyframe reaches the new value (as above);
  differences under 0.5 s only retarget.
- **Going live** jumps forward to the latest unsent keyframe (or the next one
  to arrive), skipping the backlog; timestamps stay continuous. An earlier
  version sent the backlog faster than real time, which leaves viewers
  behind unless their player skips ahead by itself.
- **Default delay** is 30 s with a 60 s buffer, so the default enable is
  instant once the buffer has filled.
- **Configuration re-sent at a jump** (SPEC §4.2 "sequence headers /
  metadata"): the audio and video sequence headers and Enhanced RTMP video
  metadata (colour information) are re-sent at every jump. The script-data
  `onMetaData` is re-sent only on a new upstream connection: mid-stream,
  ingests built on libavformat expose it as a new data stream (observed with
  ffmpeg), and decoders need only the sequence headers.
- **Timestamps**: output = source + offset; at every jump the offset is set
  so the upstream timeline continues one video frame after the last message
  sent. Audio and video are each clamped to never decrease (global clamping
  would distort audio when an encoder interleaves tracks loosely); script data
  is clamped to the highest timestamp sent. 32-bit wrap-around is handled.
- **Memory**: sent content is kept for the buffer length (whole keyframe
  intervals); hard limits are 1 GiB per stream and buffer length plus 30 s.
  When the bitrate makes the buffer target exceed 1 GiB, status reports a
  degraded state with the sustainable length, and delays above it are refused
  as unsustainable.
- **Several streams**: delay actions apply to every publishing stream (one per
  route) and succeed if any stream accepted; the first refusal is returned
  otherwise. The delay settings are shared.

## Local API and controllers

- **Request policy** (`crates/core/src/api.rs`): the HTTP server binds
  127.0.0.1 only. Every request must carry a loopback `Host`
  (`127.0.0.1`/`localhost`/`[::1]` with our port), which defeats DNS
  rebinding. A present `Origin` must be our own origin; a present
  `Sec-Fetch-Site` must be `same-origin` or `none`. `POST`/`PUT`/`DELETE`
  also need the per-run token in `X-OpenDelay-Token`.
- **Token delivery**: the token is embedded (meta tag) in the dock, overlay
  and setup pages only. There is no token endpoint. Pages send
  `X-Frame-Options: DENY` and `frame-ancestors 'none'`, so a malicious site
  cannot frame the dock to click its buttons.
- **WebSocket without token**: hardware controllers cannot obtain a per-run
  token, so the WebSocket relies on the Origin/Host checks (browsers always
  send `Origin` on WebSocket handshakes; native clients send none). `Origin:
  null` is rejected.
- **Refusals over HTTP** return status 200 with `success: false` and a code;
  malformed requests and validation errors return 400; policy violations 403.
- **Twitch routes** take their server from the Twitch server setting, so
  picking an ingest updates every Twitch route.
- **Route ids**: the default route has id `twitch`; new routes get random ids.
  Local paths are stored lowercase and compared case-insensitively.

## Settings

- `settings.json` lives in `%APPDATA%\OpenDelay` (Windows) or
  `$XDG_CONFIG_HOME/opendelay` (Linux); `OPENDELAY_CONFIG_DIR` overrides.
  Logs live in `%LOCALAPPDATA%\OpenDelay\logs` or
  `$XDG_DATA_HOME/opendelay/logs`; `OPENDELAY_DATA_DIR` overrides.
- Version 1 is the first format. `migrate()` upgrades documents step by step
  (unversioned files are treated as version 1). Unknown fields are ignored,
  missing ones take defaults, out-of-range values are clamped, a corrupt file
  is renamed to `settings.json.bad` and defaults are used. Saves are atomic.
- The update check is **off by default** (opt-in), in keeping with the
  no-telemetry rule.

## Desktop UI

- **Contrast.** White on the accent fill measures 4.25:1 and white on the
  error fill 3.8:1, below the 4.5:1 target, so primary and danger buttons use
  `bg-base` (#060608) as label colour (4.8:1 and 5.3:1). `text-muted` and
  `accent` text are 4.4:1 on `bg-surface`, so cards use `text-secondary` for
  small text and `accent-hover` for accent-coloured text; muted text appears
  only on the base and sidebar backgrounds.
- **Tabular digits**: Slint cannot enable OpenType features, so the hero
  number draws each digit in a fixed-width column (which the odometer needs
  anyway). Other numbers use Inter's default figures.
- **Modal backdrop**: Slint has no backdrop blur; modals dim the window
  (70 % black) instead.
- **Translations**: the UI ships in English only (French was removed at the
  project owner's request, although SPEC §7.4 mentions it). Every desktop
  string still goes through Slint `@tr()`: adding
  `crates/app/lang/<code>/LC_MESSAGES/opendelay.po` (with an
  `X-Language-Name` header) bundles it at build time and shows the language
  picker in Settings; web pages read `crates/core/web/lang/<code>.json`. No
  code changes are needed. Errors and refusals from the core carry stable codes
  that the UI turns into text.
- **Diagnostics in the desktop app** open the log file or the logs folder with
  the system handler; download is offered by the HTTP API.
- **Help links** point at the repository URL from `Cargo.toml`, still a
  placeholder (`example.invalid`) until the project is published.
- `cargo run -p opendelay --example screenshot --features screenshot -- DIR`
  renders every page offscreen (software renderer) for visual review.

## Integrations

- **Health checks** (`crates/core/src/health.rs`): keyframe interval warns
  above 2.5 s (high above 5 s) and recommends 2 s; "keyframes missing" after
  10 s without one; B-frames are inferred from non-zero composition times
  (3 frames of evidence); timestamp findings count backwards steps and gaps
  over 1 s; "slow start" when the first keyframe arrives more than 5 s after
  publishing; the only built-in resolution limit is Kick at 1920×1080.
  Findings are logged once per session.
- **Multistream from one encoder output**: a route with a `source` sends that
  route's encoder stream with its own stored key. Each such route is a
  separate session (own engine, upstream, recovery, log) fed by the source's
  encoder loop; media payloads are shared (`Bytes`), only the buffer
  bookkeeping is per route. This reuses the whole single-route path (delay
  actions already applied to every session) instead of fanning one engine's
  output out to several connections, which would need per-connection
  cursors and resynchronisation. Delay actions also apply to sessions still
  connecting or reconnecting, so no platform ends up less delayed than the
  others. A route sending another's stream retries for as long as the
  source streams (the source route's own failure still disconnects the
  encoder). Keys live in the settings file like OBS keeps them in its
  profile, are redacted from session logs and are never returned by the API.
- **No OBS service entry**: an earlier version added an "OpenDelay" entry to
  OBS's `services.json` so OBS applied a 2 s keyframe interval by itself.
  It was removed for the simpler setup other relay apps use: a "Custom…"
  server and the keyframe interval set once in OBS (Setup page). Entries
  added by that version stay in OBS's list until OBS downloads a new one.
- **Twitch servers** come from Twitch's public ingest list
  (`https://ingest.twitch.tv/ingests`), fetched at startup when a Twitch route
  exists and on demand. Accepted hosts: `live.twitch.tv`, `*.twitch.tv`,
  `*.contribute.live-video.net`, `*.global-contribute.live-video.net`, plain
  RTMP on port 1935.
- **Updates**: off by default (the HTTP endpoint refuses too); GitHub "latest
  release" JSON; download only on request. The release's `manifest.json`
  (version, and per platform the archive name and SHA-256) is signed with
  Ed25519; the signed version must be newer than the running one and the
  archive is streamed to a temporary file, hashed, and kept in
  `<data dir>/updates` only if it matches. Nothing is executed. Feed URL and
  public key are placeholders (verification fails closed). Verification uses
  `ring` (already linked by rustls) rather than a second Ed25519 crate.
- **Tray**: Slint's built-in `SystemTrayIcon` (Win32 / StatusNotifierItem), so
  no GTK dependency. Linux defaults to "quit on close" because Slint cannot
  report whether a tray host exists.
- **Quitting** with active streams shows a "waiting for streams" panel while
  the relay refuses new encoders and waits up to 10 s; "Quit now" forces it.
- **Hotkeys** are stored in `global-hotkey` notation (`shift+control+KeyD`).
  A shortcut needs Ctrl, Alt or Super unless it is a function key.
- **Licences beyond the original list**: `BSL-1.0` (Slint's Windows clipboard)
  and `Unlicense` (offered as `MIT OR Unlicense` by several crates and alone by
  `ksni`, Slint's Linux tray) are both GPL-3.0-compatible per the FSF and are
  allowed in `deny.toml`.
- **Web pages** (dock, overlay, setup) take their strings from
  `crates/core/web/lang/<code>.json`, embedded at build time by the core
  build script; a new language is a new JSON file.
- **Ports**: 1937 (RTMP) and 3937 (HTTP) by default, not the usual 1935 /
  3000, so OpenDelay can run next to other local RTMP relays;
  `OPENDELAY_RTMP_PORT` / `OPENDELAY_HTTP_PORT` move them when another program
  already listens there (the app otherwise shows an error toast).

## Security review (hardening)

- **Route takeover**: a new encoder connection may replace a session on the
  same route only when it comes from the same IP address (an encoder
  reconnecting). Any other machine is refused while the route streams, so a
  LAN peer cannot cut the user's stream. A route is taken at `publish`, not at
  `connect`: a peer that never publishes blocks no one. Taking a route and
  registering a session are checked under the same lock as emergency stop,
  so neither can race past it.
- **Resource limits**:
  - Connections: 16 from this computer (127.0.0.1 / ::1) and, separately,
    16 from other computers with at most 4 per address, so LAN peers can
    never use up the local encoder's places.
  - Before `publish`: one 30 s deadline from accepting the connection to
    `publish` (handshake within 10 s), and messages are limited to 64 KiB
    (real `connect`/`publish` commands are a few hundred bytes).
  - AMF0 decoding stops after 65 536 values per message (each value costs
    ~32–56 bytes of memory per 1–3 input bytes); script data over 64 KiB is
    never parsed as `onMetaData`.
  - Buffers: 1 GiB per stream and 2 GiB for all streams together (a route
    sending another route's stream shares its payloads and is not counted again).
  - Encoders must send data at least every 20 s after publishing; an
    upstream write that makes no progress for 15 s is treated as a lost
    connection and recovered.
- **Stream keys** never reach logs or status: once known, the key is masked
  in every session-log line and in `lastError` (platforms may echo it in
  error descriptions). Keys shorter than 6 characters are not masked.
- **Files**: settings, the logs folder and log files are created owner-only
  (0600 / 0700) on Unix. Only well-formed log ids are listed or served.
  Settings are written to a synced temporary file and renamed; the previous
  file is kept as `settings.json.bak` and used if the current one is corrupt.
  Fields unknown to this version are kept on save. Delay changes are saved by
  a background writer; other settings changes are saved outside the settings
  lock and report a failed save.
- **HTTP**: every response carries `X-Content-Type-Options: nosniff` and API
  responses `Cache-Control: no-store`, in addition to the Host/Origin/
  Sec-Fetch-Site checks and the token. Pages have no inline scripts or styles
  and are served with `Content-Security-Policy: default-src 'self'` and
  `Referrer-Policy: no-referrer`; only the dock (which sends commands)
  carries the token.
- **Network at startup**: none. The Twitch server list is fetched when its
  picker is first opened (cached 10 minutes); the update check runs only when
  enabled, and `GET /api/update` refuses while it is off.
- **Robustness**: two panics reachable from stream input were fixed (bitrate
  estimate with empty payloads, very large metadata dimensions). The RTMP
  parsers and the delay engine are fuzzed with libFuzzer (`fuzz/`, see the
  README); the engine target checks that upstream timestamps never decrease.
- **LAN mode access key**: other computers must add `?key=<key>` to the server
  URL (`rtmp://<lan-ip>:1937/<route>?key=…`); OBS and librtmp-based encoders
  pass that query in `app` and `tcUrl`, where it is checked (constant time)
  before the route lookup, the platform connection and the session log. The
  key is 16 random hex digits created once per installation, kept in the
  settings and shown on the Setup page, where "New key" replaces it at once
  for new connections (connected streams keep running); this computer (127.0.0.1, ::1) needs
  none. Without it, LAN mode was an open relay for anyone on the network,
  who could also take a route before the user's encoder or push the user's
  session logs out of rotation. The query never reaches the platform (`app`,
  `tcUrl` and `swfUrl` are rewritten).

## Performance

- The tokio runtime uses two worker threads (plus the blocking pool for the
  Twitch list and update downloads).
- No animated background: the window is drawn on the CPU and redraws only
  what changed, so an idle window draws nothing. A full-window animation
  redraws every pixel, four times as many at 200% display scale.
- LAN addresses (one syscall per interface) are cached for 10 s instead of
  being enumerated on every status poll; the UI fetches settings once per
  refresh.
- Release builds are stripped.

## Platform integration

- **Slint features**: default features off; enabled are `std`,
  `compat-1-18` (required), `backend-winit` (X11 + Wayland on Linux, Win32),
  `renderer-software`, `accessibility` and `system-tray`. `slint-build` runs
  without its default compile-time software-renderer resources.
- **CPU rendering only**, so the GPU stays free for the encoder. Measured on
  Windows with an NVIDIA GPU, the window (1180×800) uses 10 MB private memory
  (21 MB at 200% scale), against 132 MB with the OpenGL (FemtoVG) renderer,
  most of it driver memory. CPU use, idle / streaming / while a delay builds
  up: 0% / 0.2% / 0.4% of one core at 100% scale, 0% / 0.2% / 1.2% at 200%;
  the last is the rolling digits, 0.2% with Reduce motion. One renderer also
  removes the OpenGL failure fallback (VMs, remote desktops).
- **Tray host detection** on Linux asks the session bus for
  `org.kde.StatusNotifierWatcher`; without it no tray is created and closing
  the window quits. "Keep in tray" is the default everywhere else.
- **Wayland global shortcuts** use `org.freedesktop.portal.GlobalShortcuts`
  when present (a new portal session per binding change); the captured
  shortcut is the preferred trigger and the desktop has the final say.
- **Single instance**: a second launch reads the running instance's token from
  its dock page (local processes may read it) and asks it to show its window
  via `POST /api/window/show`.
- **Windows icon**: `build.rs` writes a compiled resource file from
  `assets/logo.ico` (PNG-compressed images) and passes it to the linker, so
  no resource compiler is needed for native or cross builds.
- **Release endpoints**: feed URL, signing key and repository URL can be set at
  build time (`OPENDELAY_UPDATE_FEED`, `OPENDELAY_UPDATE_KEY`,
  `OPENDELAY_REPOSITORY`).
- **Kick** limits (1920×1080) follow Restream's Kick guide; Kick's own help
  pages could not be found.
