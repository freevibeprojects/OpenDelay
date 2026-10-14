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
