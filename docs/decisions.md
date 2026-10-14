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
- **Encoder stops while content is still buffered** (delay active): the relay
  keeps sending the buffered tail at its normal pace, then unpublishes. A new
  encoder connection on the same route replaces the old session.
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
- **Instant enable** picks the latest buffered keyframe at least `delay +
  200 ms` old. The achieved delay is reported as the current delay (it is at
  most one keyframe interval longer than requested); the requested value is
  the target.
- **Aging ("buffering")**: when no keyframe is old enough, output pauses at
  the current position (no jump, nothing skipped or repeated) until unsent
  content reaches the requested age. Viewers see a pause of up to the delay,
  which is the only way to build a delay without replaying content.
- **"Buffer not ready"** is returned only when nothing usable has been
  buffered yet (no keyframe received), since aging handles every other case.
- **Requested delay longer than the buffer**: the buffer grows to the delay
  (also persisted as the new buffer setting) and the message says the delay
  becomes fully effective once the buffer has filled.
- **Changing the delay while active**: longer ages in place, shorter drains
  faster than real time to the new value; differences under 0.5 s only
  retarget.
- **Going live** sends the backlog as fast as the upstream connection accepts
  it. Bounds: 10 s or 10 000 messages, then a forced switch to the latest
  keyframe (timestamps stay continuous).
- **Default delay** is 30 s with a 60 s buffer, so the default enable is
  instant once the buffer has filled.
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
- **Encoder "screenshots"** in the setup assistant are drawn schematics of
  the stream-settings form (service, server, key) rather than screenshots of
  third-party software, which keeps them original and translatable.
- **Translations**: desktop strings use Slint `@tr()` with bundled gettext
  catalogues in `crates/app/lang/<code>/LC_MESSAGES/opendelay.po`; the
  language list is generated at build time from that folder (the native name
  comes from the `X-Language-Name` header). Adding a language = adding a
  `.po` file. `scripts/extract_strings.py <po>` lists missing strings. Errors
  and refusals from the core carry stable codes that the UI translates; only
  low-level network error details stay in English.
- **Diagnostics in the desktop app** open the log file or the logs folder with
  the system handler; download is offered by the HTTP API.
- **Help links** point at the repository URL from `Cargo.toml`, still a
  placeholder (`example.invalid`) until the project is published.
- `cargo run -p opendelay --example screenshot --features screenshot -- DIR`
  renders every page offscreen (software renderer) for visual review.
