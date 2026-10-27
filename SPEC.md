# OpenDelay — Functional Specification

Status: clean-room functional specification, version 1.
This document describes required *behaviour* only. It contains no source code,
no decompiled or extracted material, and no text copied from any existing
product. Implementers must build from this document alone.

---

## 1. Purpose

OpenDelay is a desktop application that sits between a live-streaming encoder
(primarily OBS Studio; also Streamlabs, Aitum Vertical and any RTMP encoder)
and one or more streaming platforms. The encoder publishes to OpenDelay on the
local machine; OpenDelay relays the stream to the real platform ingest server.

Its core feature is a **switchable broadcast delay**: the streamer can delay
what viewers see by a chosen amount (for example to hide in-game positions from
stream snipers) and turn the delay on or off at any time **without stopping
the stream and without a visible freeze**.

Primary platform: Windows 10/11 x64. Linux support is desirable.

## 2. Core concepts

- **Route**: one relay path. It has a unique local application path on the
  local RTMP listener (for example `twitch`, `youtube`) and an upstream
  destination URL. The encoder publishes to `rtmp://<listener>/<local path>`
  using its normal stream key; OpenDelay forwards the publish to the
  upstream URL with the same stream key.
- **Passthrough**: live relay with no added delay.
- **Rolling buffer**: while a stream is connected, OpenDelay keeps the most
  recent N seconds of the incoming media in memory, so that delay can be
  switched on instantly by replaying from the past.
- **Delay active / inactive**: whether viewers currently receive delayed
  content.

## 3. RTMP relay

### 3.1 Listener
- Accept RTMP publish connections on TCP port 1937.
- Two listener modes, user-selectable:
  - **Local only** (default): bind to the loopback address.
  - **LAN**: bind to all interfaces, so an encoder on another PC on the
    local network can publish. The UI must show this machine's LAN IPv4
    addresses for convenience.
- Changing listener mode takes effect after an application restart; the UI
  must say so.
- Disable Nagle (TCP_NODELAY) on both incoming and outgoing sockets.

### 3.2 Upstream
- Support `rtmp://` and `rtmps://` (TLS) upstream URLs. Reject other schemes
  and URLs without a host with a clear validation error.
- Perform the RTMP handshake and the publish command sequence upstream on
  behalf of the encoder, and relay the platform's responses back so the
  encoder sees a normal publish session.
- Parse the RTMP chunk stream properly: chunk size changes, all chunk header
  formats, multiple chunk stream IDs, AMF0 command messages, audio/video/data
  messages, and Enhanced RTMP (including multitrack video). Message
  timestamps must be tracked per stream.
- Detect video keyframes, including Enhanced RTMP keyframe signalling.

### 3.3 Upstream recovery
- If the upstream connection drops while the encoder is still connected,
  reconnect automatically with bounded retries and back-off.
- After reconnecting, resume from a keyframe so the platform receives a
  decodable stream. Report recovery state (attempt count, last error) in
  status.

### 3.4 Shutdown
- When the application is quitting, refuse new encoder connections, then
  wait (bounded, e.g. 10 s) for active streams to stop before exiting.
- **Emergency stop**: an API / controller action (no button in the app or dock)
  that immediately disconnects every active stream, upstream and downstream.

## 4. Delay engine

### 4.1 Buffer
- Rolling buffer length is configurable. Suggested presets: 15 s, 30 s, 45 s,
  60 s, 90 s, 2 min, 3 min, 4 min, 5 min, 10 min. Default 60 s; maximum 10 min.
- The buffer stores RTMP messages with timestamps and keyframe markers.
- Track and expose: buffer fill (ms), buffer target (ms), whether the buffer
  holds enough content to support the requested delay ("ready"), and the
  largest delay currently achievable.
- Memory use is bounded by the buffer length; old content is discarded.
- If the stream's bitrate makes the requested buffer unsustainable, report a
  degraded state with a reason rather than failing silently.

### 4.2 Switching delay ON (instant)
- On request with delay D:
  - When a buffered keyframe becomes D old (with a small safety margin),
    start sending from it: viewers see the last D seconds again. This
    happens within one keyframe interval of the request.
  - If the buffer does not yet contain content D old, keep sending live and
    jump back once a keyframe D old is buffered. Output never pauses:
    platforms drop an ingest that sends nothing (Twitch after 30 s) and
    viewers would see a frozen stream.
  - Make the switch only at a message boundary, so no RTMP message is cut
    mid-way.
- The platform must see continuous, monotonically increasing timestamps
  across the switch: rewrite outgoing timestamps with a running offset.
- Re-send codec configuration (sequence headers / metadata) before replaying,
  and send full chunk headers for each chunk stream after the jump.
- If the requested delay is larger than the current buffer, grow the buffer
  to fit it, and report that the delay starts once the buffer has filled.

### 4.3 Switching delay OFF (go live)
- Jump forward to the latest keyframe (viewers skip the delayed backlog), or
  at the next keyframe when none is left to send. A shorter delay jumps
  forward when a keyframe reaches the new delay. (Sending the backlog faster
  than real time does not shorten what viewers see unless their player skips
  ahead by itself.)
- Keep timestamps continuous across the transition.

### 4.4 Actions
All of these must be available to every control surface (UI, hotkeys, tray,
local API, controller WebSocket):

| Action | Effect |
|---|---|
| Enable delay | Activate delay with the configured default or a given duration |
| Disable delay / Go live | Return to live |
| Toggle delay | Enable if inactive, disable if active |
| Set delay | Change the delay duration (while active or for next activation) |
| Resize buffer | Change the rolling buffer length |
| Enable/disable delay mode | Turn the rolling-buffer feature on or off as a whole |
| Toggle overlay visibility | Show/hide the on-stream overlay |
| Emergency stop | See §3.4 |

### 4.5 Refusals
Actions that cannot be performed must return a machine-readable reason code
plus a human-readable message. Required reasons:

- no stream connected
- buffer not ready yet (include fill percentage)
- delay already active / already live
- delay mode is turned off
- action not valid in the current mode
- another transition is still in progress
- requested delay cannot be sustained at the current bitrate
- transport error

A reconnecting upstream is not a refusal: delay actions apply to it too, so
several destinations never diverge.

### 4.6 Persistence
Persist whether delay mode is enabled and the default buffer length; restore
them at startup.

## 5. Stream health checks

Inspect the incoming stream and warn the user about encoder settings that
break or degrade instant switching. Each finding has a severity (low /
medium / high) and a confidence. Required checks:

- video codec unsupported or unknown
- keyframe interval too long (switches can only land on keyframes; suggest
  2 s or less)
- keyframes missing
- B-frames likely enabled
- irregular or non-monotonic timestamps
- slow startup handoff
- resolution above the destination platform's limit (e.g. Kick)

Expose detected codec, resolution, keyframe interval, B-frame detection and
the number of observed frames.

## 6. Routes and destinations

- Presets: Twitch, YouTube, Kick, TikTok, Restream, Custom. Each preset fills
  in a sensible local path and upstream URL.
- **Twitch**: the user can pick a Twitch ingest server; only Twitch-owned
  ingest hosts on port 1935 are accepted. Default is Twitch's global
  auto-selecting ingest endpoint.
- Rules:
  - Local paths must be unique across routes.
  - A route that is currently streaming cannot be edited, disabled or
    deleted; tell the user to stop the stream first. Exception: a route
    sending another route's stream can be disabled or deleted mid-stream,
    which stops only its platform connection.
  - Route names are 1–64 characters without control characters.
  - The default Twitch route may be edited or disabled but not deleted.
  - Path changes take effect on the encoder's next connection; say so.
- Multistream: several routes may be live at the same time (one encoder
  output per route). A route can also send the stream of another route
  (its "source") with its own stored stream key, so one encoder output
  reaches several platforms (e.g. Twitch + Kick + YouTube). Each such route
  has its own upstream connection, recovery (retrying for as long as the
  source streams) and session log; delay actions apply to all of them.
  Encoders cannot publish to such a route directly. Stored keys are never
  returned by the API.

## 7. User interface

Desktop window, plus pages served by the local web server so they can be
used inside OBS.

### 7.1 Main window
- Dashboard: connection state, uptime, delay state, current and target delay,
  buffer fill progress, bitrate/packet counters, health warnings, big
  delay on/off control, delay input (typed seconds, ±5 s, presets),
  buffer preset selector.
- Multi-stream page: route list with add/edit/delete/enable.
- Overlay page: overlay URL with copy button, live preview.
- Controllers page: hotkey configuration and controller (Stream Deck) setup.
- Settings: start with OS, close-to-tray vs quit on close, Twitch server,
  listener mode, language, update check.
- Diagnostics: list of recent stream session logs with download.
- Help links to the project's own documentation and community.

### 7.2 Encoder setup
The Setup page lists each route's server URL (routes that receive an encoder
stream) with the steps for OBS: "Custom…" service, the platform's own stream
key, keyframe interval 2 s and CBR; plus Streamlabs and other encoders, and a
shortcut to add a platform.

### 7.3 OBS dock and overlay
- **Dock page**: a compact version of the control panel, served over HTTP on
  the local web server, intended for OBS "Custom Browser Dock".
- **Overlay page**: transparent page for an OBS Browser Source that shows the
  delay state on stream (badge with delay on/off, countdown/progress during
  transitions). Can be hidden via the toggle-overlay action. Accepts a
  language parameter and a preview/scale parameter for the in-app preview.

### 7.4 Languages
English and French at launch. All UI strings in translation files; adding a
language must not require code changes.

## 8. Control surfaces

### 8.1 Global hotkeys
User-configurable system-wide shortcuts, unset by default:
enable delay, go live, toggle overlay, and "custom delay" (enables delay with
a user-chosen number of seconds, default 60).

### 8.2 Tray
Tray icon with menu: show window, enable/disable delay, go live, quit.

### 8.3 Local HTTP API
- HTTP server on a localhost port (default 3937), serving the dock, overlay,
  pages and a JSON API covering: status, every action in §4.4,
  routes CRUD, network settings, Twitch server, diagnostics list/download,
  update check.
- Responses are JSON with a success flag, message, and the fresh status.
- **Security (required)**:
  - Bind to loopback only.
  - State-changing endpoints require a per-run random token, obtainable only
    by same-origin pages, sent in a custom request header.
  - Reject requests whose `Origin` / `Sec-Fetch-Site` indicate a cross-site
    browser request, so arbitrary websites cannot control the stream.
- The UI polls status (faster for a short period after a command).

### 8.4 Controller WebSocket
WebSocket endpoint on the same server for hardware controllers (e.g. an
Elgato Stream Deck plugin, Touch Portal, Bitfocus Companion). Request/response
JSON messages for every action in §4.4 plus status; push status updates to
subscribers. Unknown or malformed requests get an error reply, never a
disconnect. Document the protocol in the repository so third parties can
build plugins. (Writing the Stream Deck plugin itself is optional, separate
work.)

## 9. Diagnostics

- One plain-text log file per stream session in a logs folder in the
  app data directory (overridable by an environment variable), named with the
  session start time and connection id.
- Header with session id, start/end time, connection id and Twitch server;
  body with relay events, delay transitions, upstream errors, health findings.
- Log ids exposed via API must be validated against path traversal.
- Rotate or cap the number/size of kept logs.

## 10. Settings storage

JSON settings file in the per-user config directory, containing: routes,
start-with-OS, quit-on-close, delay mode enabled, default
buffer, listener mode, Twitch server, language. Versioned, with migrations
for older files. Unknown fields must not cause a crash.

## 11. Updates

Optional update check against the project's own release feed (e.g. GitHub
Releases). Updates must be signature-verified before installing. The user is
asked before downloading; no silent updates.

## 12. Explicit non-goals

- No licensing, activation, trial period, machine binding, or payment code.
  OpenDelay is free software.
- No telemetry or analytics.
- No branding, names, logos, colours, layouts, wording or assets of any
  existing commercial product.

## 13. Acceptance criteria

1. OBS can publish to `rtmp://127.0.0.1:1937/<route>` and the stream appears
   on the platform with no delay in passthrough.
2. With a 60 s buffer filled, enabling a 30 s delay makes viewers see content
   from ~30 s earlier within one keyframe interval, with no stream interruption
   on the platform side.
3. Disabling delay returns viewers to live without disconnecting.
4. Timestamps sent upstream never decrease (verified by an automated test
   using a recorded/synthetic RTMP stream).
5. A cross-site browser request cannot trigger any action.
6. Two routes can be live simultaneously.
7. Emergency stop disconnects everything within one second.
8. Core relay and delay logic is covered by automated tests that run without
   network access (synthetic RTMP input, mock upstream).
