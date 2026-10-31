# OpenDelay controller protocol

This document describes how hardware and software controllers (Stream Deck
plugins, Touch Portal, Bitfocus Companion, scripts) control OpenDelay. It is
part of the project and may be implemented freely by third parties.

Protocol version: **1**.

## Connection

OpenDelay serves a WebSocket on the local machine only:

```
ws://127.0.0.1:3937/ws
```

- The `Host` header must be `127.0.0.1:3937`, `localhost:3937` or
  `[::1]:3937` (standard WebSocket clients send this automatically).
- Native clients should send **no `Origin` header**. Browser-based clients are
  only accepted when their page is served by OpenDelay itself
  (`Origin: http://127.0.0.1:3937`). Any other origin, including `null`, is
  rejected with HTTP 403. This stops arbitrary websites from controlling the
  stream.
- No token is needed on the WebSocket; the origin check is what keeps web
  pages out, and any local program can already control local software.
- Messages are UTF-8 JSON text frames, at most 64 KiB. A larger message up to
  1 MiB gets a `too_large` error reply and the connection stays open; beyond
  1 MiB the connection is dropped without a close frame. Binary frames get an
  error reply.
- No subprotocol is needed. A client that offers `opendelay` gets it echoed;
  other offered subprotocols are not selected (browsers then fail the
  connection), so offer `opendelay` or none.

On connect the server sends:

```json
{ "type": "hello", "app": "OpenDelay", "version": "1.2.0", "protocol": 1 }
```

Clients should check `protocol` and ignore message fields they do not know.

## Requests and responses

Every request is an object with an `action` and an optional `id`. The `id` can
be any JSON value; it is echoed back unchanged so replies can be matched.

```json
{ "id": 7, "action": "enableDelay", "seconds": 30 }
```

Every request gets exactly one response:

```json
{
  "type": "response",
  "id": 7,
  "success": true,
  "message": "Delay on: viewers are now 30 s behind.",
  "code": null,
  "detail": null,
  "status": { "...": "see Status below" }
}
```

When `success` is `false`, `code` is a machine-readable reason and `message`
explains it in English. For `invalid_in_mode` and `invalid_value`, `detail`
names the cause (`go_live_first`, `delay_range`,
`buffer_range`, `not_a_number`); it is `null` otherwise. Malformed or unknown requests also get a response
(never a disconnect).

### Actions

| `action` | Parameters | Effect |
|---|---|---|
| `getStatus` | — | Reply with the current status only. |
| `subscribe` | — | Start receiving `status` pushes (see below). |
| `unsubscribe` | — | Stop status pushes. |
| `enableDelay` | `seconds` (optional, 1–600) | Turn the delay on with the configured or the given duration (which becomes the configured delay). |
| `goLive` (alias `disableDelay`) | — | Turn the delay off and return to live. |
| `toggleDelay` | — | `enableDelay` if live, otherwise `goLive`. |
| `setDelay` | `seconds` (1–600) | Change the delay; applies immediately when active and to the next activation. |
| `setBuffer` | `seconds` (5–600) | Change the rolling buffer length; the buffer kept is never shorter than the delay. |
| `setDelayMode` | `enabled` (bool) | Turn the rolling-buffer feature on or off as a whole. |
| `toggleOverlay` | — | Show or hide the on-stream overlay. |
| `setOverlayVisible` | `visible` (bool) | Show or hide the overlay explicitly. |
| `emergencyStop` | — | Disconnect every stream immediately and block publishing. |
| `clearEmergencyStop` | — | Allow publishing again after an emergency stop. |

### Reason codes

| `code` | Meaning |
|---|---|
| `no_stream` | No stream is connected. |
| `buffer_not_ready` | The buffer has nothing usable yet; the message includes the fill percentage. |
| `already_active` | The delay is already on. |
| `already_live` | The stream is already live. |
| `delay_mode_off` | Delay mode is turned off. |
| `invalid_in_mode` | The action is not valid right now (e.g. turning delay mode off while delayed). |
| `transition_in_progress` | Going live is still waiting for a keyframe. |
| `unsustainable` | The requested delay does not fit in memory at the current bitrate. |
| `transport_error` | The stream connection failed while applying the action. |
| `invalid_value` | A parameter is out of range. |
| `bad_request` | Malformed JSON, missing `action` or invalid parameters. |
| `unknown_action` | The `action` is not part of this protocol. |
| `too_large` | The message or request body is over 64 KiB. |
| `forbidden` | Wrong `Host`, cross-site `Origin`/`Sec-Fetch-Site`, or a missing or wrong HTTP token (HTTP 403). |
| `not_found` | No such route, log or endpoint (HTTP 404). |
| `update_check_off` | `GET /api/update` while "Check for updates" is off. |

Clients should treat any other `code` as a failure with a readable `message`.
`invalid_value` carries a `detail`: `delay_range`, `buffer_range` or
`not_a_number`. Parameters of the wrong JSON type give `bad_request`.

## Status pushes

After `subscribe`, the server sends the status once per second and right after
every change:

```json
{ "type": "status", "status": { "...": "see Status below" } }
```

## Status

All durations are milliseconds.

```json
{
  "version": "1.2.0",
  "state": "delayed",
  "connected": true,
  "uptimeMs": 754000,
  "bitrateKbps": 6012,
  "delay": {
    "modeEnabled": true,
    "state": "delayed",
    "currentMs": 31420,
    "targetMs": 30000,
    "configuredMs": 30000,
    "bufferFillMs": 60000,
    "bufferTargetMs": 60000,
    "ready": true,
    "maxDelayMs": 59800,
    "progress": null,
    "degraded": null,
    "bufferLimitMs": null,
    "skippedMs": 0
  },
  "health": {
    "codec": "H.264",
    "width": 1920,
    "height": 1080,
    "keyframeIntervalMs": 2000,
    "bFrames": false,
    "frames": 22620,
    "findings": []
  },
  "streams": [
    {
      "id": 3,
      "routeId": "twitch",
      "routeName": "Twitch",
      "routePath": "twitch",
      "phase": "live",
      "uptimeMs": 754000,
      "bitrateKbps": 6012,
      "bytesIn": 566800000,
      "bytesOut": 566700000,
      "recoveryAttempt": 0,
      "lastError": null,
      "delay": { "...": "same shape as above" },
      "health": { "...": "same shape as above" }
    }
  ],
  "overlayVisible": true,
  "overlayColor": "violet",
  "emergencyStop": false,
  "encoderRefused": null,
  "listener": {
    "mode": "local",
    "configuredMode": "local",
    "restartRequired": false,
    "port": 1937,
    "lanAddresses": ["192.168.1.20"]
  }
}
```

- `state`: `offline` (no encoder), `connecting`, `live`, `buffering` (the
  buffer does not reach back as far as the requested delay yet; viewers keep
  seeing what they saw before), `delayed` (while switching to another delay,
  `delay.targetMs` differs from `delay.currentMs` until a keyframe reaches it),
  `draining` (waiting for a keyframe to go live) or `recovering` (platform
  connection lost, reconnecting).
- `delay.currentMs` is how far behind the encoder the platform currently is;
  `delay.targetMs` is the delay being established (0 when live).
- `delay.ready` tells whether enabling the configured delay would be instant.
- `delay.progress` (0–1) is set while buffering: how far back the buffer
  reaches towards the requested delay.
- `delay.degraded` is a message and `delay.bufferLimitMs` the buffer length that
  fits in memory, when the bitrate makes the buffer target unsustainable.
- `delay.skippedMs`: content skipped this stream because the upload was
  slower than the stream (see `upload_slow`).
- `streams[].phase`: `connecting`, `live`, `recovering` (platform connection
  lost, reconnecting) or `finishing` (the encoder stopped; the buffered tail is
  still being sent).
- `health` is `null` while offline; `keyframeIntervalMs`, `codec`, `width`
  and `height` are `null` until measured (always for audio-only streams).
- `health.findings` lists problems with the encoder settings, each with a
  `code` (`codec`, `keyframe_interval`, `keyframes_missing`, `b_frames`,
  `timestamps`, `slow_start`, `resolution`, `no_video`, `no_audio`,
  `bitrate_low`, `bitrate_high`, `upload_slow`), a `severity` (`low`, `medium`,
  `high`), a `confidence` (0–1), the observed `value`, the `limit` it was
  compared with and an English `message`. `value` and `limit` are display
  strings (e.g. `"4.0 s"`, `"220 kbit/s"`), empty when there is nothing to
  show. `bitrate_high` exists only for routes with the `twitch` (8500 kbit/s)
  or `kick` (8000 kbit/s) preset; bitrates count video and audio.
- `encoderRefused`: why the last encoder was turned away (unknown or disabled
  route, or one that sends another route's stream), for two minutes; `null`
  otherwise or once an encoder gets a route.
- With several routes live, `streams` lists each one and the top-level fields
  describe the first.

Newer versions may add fields; clients must ignore unknown ones.

## HTTP API

The same actions are available over HTTP for scripts and the OBS dock:

- `GET /api/status` — `{ "success": true, "message": "", "code": null, "status": {...} }`
- `POST /api/action` with a request body like the WebSocket request above,
  including `getStatus`; the reply is the WebSocket reply without `type`
  (`id` is echoed when sent). `subscribe` has no effect over HTTP.
  Malformed requests and unknown actions are HTTP 400, other replies 200 with
  `success` telling the outcome.
- `GET /api/routes`, `POST /api/routes`, `PUT|DELETE /api/routes/{id}` — routes. A route
  with `source` (another route's id) sends that route's encoder stream with its
  own `key` (multistream). The key is write-only: responses always carry
  `"key": ""`, and saving a route with an empty `key` keeps the stored one.
- `GET|PUT /api/network` (`{"listenerMode": "local"|"lan"}`), `GET|PUT
  /api/twitch` (`{"server": "rtmp://…"}`), `GET /api/twitch/servers`
  (Twitch's ingest list, cached for 10 minutes).
- `GET /api/diagnostics`, `GET /api/diagnostics/{id}` — session logs.
- `GET /api/update` — release feed check (never downloads). Refused with
  code `update_check_off` unless "Check for updates" is on.
- `POST /api/window/show` — bring the desktop window forward (used when
  OpenDelay is started a second time).

Errors from the other endpoints are JSON with `success: false`, `code` and
`message`: HTTP 400 for invalid input, 404 `not_found`, 405 for a wrong method,
403 `forbidden`, 413 `too_large`.

State-changing HTTP requests (`POST`, `PUT`, `DELETE`) need the header
`X-OpenDelay-Token`, whose per-run value is only available inside pages served
by OpenDelay. External controllers should use the WebSocket.
