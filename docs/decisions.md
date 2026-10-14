# Design decisions

Choices made where `SPEC.md` / `DESIGN.md` leave room for interpretation.
Each entry: the decision, then why.

## Build and licensing

- **Dependencies are checked with `cargo deny`** (`deny.toml`) for the Linux and
  Windows targets only. Workspace crates are private (`publish = false`) and
  GPL-3.0-only.
- **`THIRD_PARTY_LICENSES.md` is generated** from `cargo metadata` by
  `scripts/third_party_licenses.py`, so it cannot drift from `Cargo.lock`.
