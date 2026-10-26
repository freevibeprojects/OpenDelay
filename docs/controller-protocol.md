# OpenDelay controller protocol

This document describes how hardware and software controllers (Stream Deck
plugins, Touch Portal, Bitfocus Companion, scripts) control OpenDelay. It is
part of the project and may be implemented freely by third parties.

Protocol version: **1**.

## Connection

OpenDelay serves a WebSocket on the local machine only:

```
ws://127.0.0.1:3000/ws
```

- The `Host` header must be `127.0.0.1:3000`, `localhost:3000` or
  `[::1]:3000` (standard WebSocket clients send this automatically).
- Native clients should send **no `Origin` header**. Browser-based clients are
  only accepted when their page is served by OpenDelay itself
  (`Origin: http://127.0.0.1:3000`). Any other origin, including `null`, is
  rejected with HTTP 403. This stops arbitrary websites from controlling the
  stream.
- No token is needed on the WebSocket; the origin check is what keeps web
  pages out, and any local program can already control local software.
- Messages are UTF-8 JSON text frames, at most 64 KiB. Binary frames get an
  error reply.

On connect the server sends:

```json
{ "type": "hello", "app": "OpenDelay", "version": "0.1.0", "protocol": 1 }
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
names the cause (`go_live_first`, `buffer_below_delay`, `delay_range`,
`buffer_range`, `not_a_number`); it is `null` otherwise. Malformed or unknown requests also get a response
(never a disconnect).

### Actions

| `action` | Parameters | Effect |
|---|---|---|
| `getStatus` | — | Reply with the current status only. |
| `subscribe` | — | Start receiving `status` pushes (see below). |
| `unsubscribe` | — | Stop status pushes. |
| `enableDelay` | `seconds` (optional, 1–600) | Turn the delay on with the configured or the given duration. |
| `goLive` (alias `disableDelay`) | — | Turn the delay off and return to live. |
| `toggleDelay` | — | `enableDelay` if live, otherwise `goLive`. |
| `setDelay` | `seconds` (1–600) | Change the delay; applies immediately when active and to the next activation. |
| `setBuffer` | `seconds` (5–600) | Change the rolling buffer length. |
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
  "version": "0.1.0",
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
    "degraded": null
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
  "listener": {
    "mode": "local",
    "configuredMode": "local",
    "restartRequired": false,
    "port": 1935,
    "lanAddresses": ["192.168.1.20"]
  }
}
```

- `state`: `offline` (no encoder), `connecting`, `live`, `buffering` (waiting
  for a keyframe that is as old as the requested delay; viewers keep seeing
  what they saw before), `delayed`,
  `draining` (waiting for a keyframe to go live or shorten the delay) or
  `recovering` (platform connection lost, reconnecting).
- `delay.currentMs` is how far behind the encoder the platform currently is;
  `delay.targetMs` is the delay being established (0 when live).
- `delay.ready` tells whether enabling the configured delay would be instant.
- `delay.progress` (0–1) is set while buffering: how far back the buffer
  reaches towards the requested delay.
- `health.findings` lists problems with the encoder settings, each with a
  `code` (`codec`, `keyframe_interval`, `keyframes_missing`, `b_frames`,
  `timestamps`, `slow_start`, `resolution`), a `severity` (`low`, `medium`,
  `high`), a `confidence` (0–1), the observed `value`, the `limit` it was
  compared with and an English `message`.
- With several routes live, `streams` lists each one and the top-level fields
  describe the first.

Newer versions may add fields; clients must ignore unknown ones.

## HTTP API

The same actions are available over HTTP for scripts and the OBS dock:

- `GET /api/status` — `{ "success": true, "message": "", "code": null, "status": {...} }`
- `POST /api/action` with a request body like the WebSocket request above.
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

State-changing HTTP requests (`POST`, `PUT`, `DELETE`) need the header
`X-OpenDelay-Token`, whose per-run value is only available inside pages served
by OpenDelay. External controllers should use the WebSocket.
