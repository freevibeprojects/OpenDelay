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
