# Android Fixtures

Shared fixtures for protocol + Loro so Rust, Swift, and Kotlin converge byte-for-byte.

## Location

- `fixtures/protocol/` — wire frames (registry JSON, chat2 binary frames, loro-protocol bytes)
- `fixtures/loro/` — Loro snapshot / update blobs (workspace registry + session transcripts)
- Checked in as small binary + JSON files. Paths referenced by all three platforms.

## Naming and versioning

- `fixtures/protocol/registry/<name>.json` — e.g. `hello-empty.json`, `state-full.json`, `push-chat-create.json`, `error-quota.json`
- `fixtures/protocol/chat2/<name>.bin` — length-prefixed frame blobs + `.json` sidecar with decoded header for review
- `fixtures/loro/registry/<name>.bin` — registry snapshots (`registry1` shape)
- `fixtures/loro/session/<name>.bin` — session snapshots / checkpoints (`schemaVersion` in doc meta)
- Version via directory or `vN` suffix when wire format changes: `state-v2.json`. Old fixtures kept for regression unless protocol is bumped.
- Every fixture has a sibling `.meta.json` with `source`, `createdAt`, `loroVersion`, `generator` (command that regenerates it).

## Deterministic regeneration

- Rust is the generator: `cargo run -p zeron-doc --bin gen-fixtures` (to be added in Task 1.4) writes fixtures from typed constructors, not hand-edited bytes.
- Kotlin and Swift tests are consumers only — they decode fixtures, never generate them.
- CI asserts fixtures are byte-stable: `cargo test -p <binding-crate>` re-exports and compares; Kotlin `FixtureTest` decodes same files.

## Privacy / content rules

- No credentials, tokens, user data, or production transcripts. Synthetic devices/chats/parts only.
- Names are `device:test-1`, `chat:fixture-…`, lorem text.

## Ownership

- **Rust**: owns generation + import/export round-trip tests (`cargo test -p zeron-doc`, `cargo test -p <binding-crate>`).
- **Kotlin**: owns decode + adapter tests that consume the same fixtures (unit tests, no network).
- **Swift**: existing iOS tests continue to consume via SPM; no change required.

## Ignore / artifact hygiene

- Fixtures are intentionally committed; do not add `fixtures/` to `.gitignore`.
- Generated temp output (e.g. `fixtures/.tmp/`) is ignored via `fixtures/.gitignore` if needed.
- No large blobs (>512KB per file without justification); checkpoint fixtures are capped blobs.

## Roadmap

- Task 1.4 adds the Rust fixture corpus (workspace state + one session transcript) and the cross-platform import tests.
- Phase 4 adds protocol codec fixtures (valid + malformed/truncated) consumed by Kotlin.
