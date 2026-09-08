# Android App Implementation Plan

Status: planning only. No Android implementation exists yet.

This document is the execution plan for adding a native Android client to Zeron. It is intentionally broken into small tasks so a low-cost coding model can complete one task at a time without guessing project architecture.

## 1. Product scope

Build a native Android application in `apps/android` using Kotlin and Jetpack Compose. The Android app is a **remote viewport**, like the existing iOS app:

- It does not run the agent engine locally.
- It connects to the existing edge/backend protocol.
- It joins the same workspace and chat rooms as desktop and iOS.
- It reads and writes the same Loro-backed documents.
- It sends durable commands to the host device.

### MVP features

- WorkOS sign-in using an external browser and deep-link return.
- Development auth mode for local edge/CI testing.
- Organization selection.
- Secure access/refresh token storage.
- Device identity and registration.
- Workspace/home screen.
- Session list.
- Session transcript with live updates.
- Full transcript rendering: markdown, code blocks, inline code, lists, links, tables, tool groups, error chips, input-request chips, and streaming updates.
- Composer for sending prompts.
- Steering an active session.
- Answering agent input requests.
- Archive and unarchive sessions.
- Registry and chat WebSocket connections.
- Reconnect and liveness handling required for foreground use.
- Contract tests against shared protocol fixtures.
- Internal arm64 APK.

### Explicitly out of MVP

- Attachments and image upload.
- Push notifications.
- Background session monitoring.
- Full offline-first behavior.
- Tablet-specific layout.
- Landscape-specific design.
- Demo mode.
- Google Play publication.
- Running an agent directly on Android.
- A second Android-specific backend API.

## 2. Fixed architecture decisions

- UI: Kotlin + Jetpack Compose.
- Minimum Android API: 26.
- Primary form factor: phone portrait.
- UI parity: functional/design-language parity, not pixel-perfect iOS layout.
- State: Loro CRDT documents, not a replacement mirror model.
- Loro Android access: internal Rust/UniFFI binding; do not implement CRDT logic in Kotlin.
- Native ABIs: `arm64-v8a` in the internal APK; compile `x86_64` for CI/emulator tests only.
- Auth: WorkOS and dev mode.
- Login: external browser/Custom Tab plus validated deep-link callback.
- Networking: established Kotlin libraries are allowed, but dependencies must be verified and kept minimal.
- Repository location: `apps/android`.
- Backend contract: reuse the exact protocol already consumed by iOS and implemented by Rust/edge.
- Testing: shared fixtures and contract tests are mandatory before declaring protocol support complete.
- First distribution: internal APK, not Play Store.

## 3. Rules for every implementation agent

1. Read this document and the specific task before editing.
2. Do not silently expand scope.
3. Do not rewrite iOS code unless a task explicitly says so.
4. Do not invent a new backend endpoint when an existing endpoint/protocol exists.
5. Do not parse terminal ANSI output or run an agent on-device.
6. Do not implement CRDT behavior in Kotlin. Keep CRDT operations inside the FFI/native boundary.
7. Keep tasks small. If a task needs more than five files or has more than three independent concerns, split it before coding.
8. Match existing protocol names, field names, frame formats, and error semantics exactly.
9. Never commit secrets, tokens, generated local signing files, or machine-specific paths.
10. Add or update tests with behavior changes.
11. Run the narrowest relevant verification after every task.
12. Do not mark a task complete when only compilation succeeds; acceptance criteria must also be checked.
13. If an API is missing or ambiguous, stop and document the blocker instead of guessing.
14. Preserve public wire compatibility with desktop, edge, and iOS.

## 4. Dependency graph

```text
Protocol inventory and fixtures
        |
        +--> Loro API inventory
        |       |
        |       +--> Rust UniFFI binding spike
        |               |
        |               +--> Android native build integration
        |
        +--> Android project scaffold
                |
                +--> common Kotlin primitives
                        |
                        +--> auth and secure storage
                        |
                        +--> registry/chat protocol clients
                                |
                                +--> workspace/session repositories
                                        |
                                        +--> Compose screens
                                                |
                                                +--> end-to-end and release checks
```

Do not start the full UI before the Loro/FFI spike and protocol fixture work have passed.

## 5. Task conventions

Each task below is designed for one focused coding session. A task should leave the repository buildable. If the task discovers a larger problem, make the smallest safe change and add a blocker note rather than continuing into unrelated work.

Verification commands are examples. Use the actual Gradle wrapper and task names created by the scaffold. Do not use npm/pnpm/yarn for this Android project.

---

# Phase 0 — project reconnaissance and contracts

## Task 0.1: Create Android implementation tracking document

**Description:** Create a short progress ledger that records task status, blockers, and decisions without duplicating this full plan.

**Files:**
- `docs/android-app-progress.md`

**Acceptance criteria:**
- [ ] The file links to this plan.
- [ ] It contains sections for current phase, completed tasks, blockers, and verification results.
- [ ] It states that implementation has not started yet.

**Verification:**
- [ ] Read the file and confirm it contains no unsupported technical claims.

**Dependencies:** None.

**Estimated scope:** XS.

## Task 0.2: Inventory iOS/backend contracts used by the mobile app

**Description:** Produce a concise inventory of endpoints, WebSocket paths, frame types, auth fields, Loro document containers, and command shapes used by iOS. This is documentation only; do not port code yet.

**Read first:**
- `apps/ios/README.md`
- `apps/ios/Zeron/Auth/AuthClient.swift`
- `apps/ios/Zeron/Sync/RegistryClient.swift`
- `apps/ios/Zeron/Sync/ChatRoomClient.swift`
- `apps/ios/Zeron/Sync/DeviceRelayClient.swift`
- `apps/ios/Zeron/Sync/WorkspaceStore.swift`
- `apps/ios/Zeron/Sync/SessionStore.swift`
- `crates/proto/src/agent.rs`
- `crates/sync/src/chat_client.rs`
- `docs/chat2-sync.md`
- `docs/registry-sync.md`

**Files:**
- `docs/android-protocol-contract.md`

**Acceptance criteria:**
- [ ] Auth routes and request/response fields are listed.
- [ ] Registry WebSocket path, handshake, frame format, and reconnect expectations are listed.
- [ ] Chat2 WebSocket and HTTP behavior, including cursor/backfill rules, is listed.
- [ ] Device relay frame envelope is listed if required by the chosen MVP flows.
- [ ] Workspace/session Loro container names and writer rules are listed.
- [ ] The document explicitly identifies unknowns instead of guessing.

**Verification:**
- [ ] Cross-check every listed field against a source file.
- [ ] No implementation files are changed.

**Dependencies:** None.

**Estimated scope:** S.

## Task 0.3: Inventory shared Loro operations required by Android

**Description:** Determine the smallest native Loro API Android actually needs. Do not expose the entire Loro API.

**Read first:**
- `crates/doc/src/`
- `crates/sync/src/`
- `apps/ios/Zeron/Sync/SessionStore.swift`
- `apps/ios/Zeron/Sync/WorkspaceStore.swift`
- `apps/ios/Zeron/Sync/LoroValueJSON.swift`
- `docs/research/loro-rust.md`

**Files:**
- `docs/android-loro-api.md`

**Acceptance criteria:**
- [ ] Required document lifecycle operations are listed.
- [ ] Snapshot/update import and export needs are listed.
- [ ] Required JSON/read operations are listed.
- [ ] Change subscription requirements are listed.
- [ ] Version/frontier/cursor requirements are listed separately from optional operations.
- [ ] Each operation links to its current Rust or Swift usage.

**Verification:**
- [ ] Confirm no operation is included merely because it sounds useful.

**Dependencies:** Task 0.2.

**Estimated scope:** S.

## Task 0.4: Define shared fixture strategy

**Description:** Decide where protocol and Loro fixtures live and how Rust, Swift, and Kotlin tests consume them. Prefer small checked-in binary/JSON fixtures over generated test-only data.

**Files:**
- `docs/android-protocol-contract.md`
- `docs/android-loro-api.md`
- `docs/android-fixtures.md`

**Acceptance criteria:**
- [ ] Fixture naming and versioning rules are documented.
- [ ] A fixture can be regenerated deterministically.
- [ ] Fixtures do not contain credentials, user data, or production transcripts.
- [ ] Rust and Kotlin test ownership is clear.

**Verification:**
- [ ] Review paths against repository ignore rules before adding generated artifacts.

**Dependencies:** Tasks 0.2 and 0.3.

**Estimated scope:** S.

## Checkpoint 0: Contracts ready

- [ ] Tasks 0.1–0.4 are complete.
- [ ] Unknown protocol details are explicitly listed.
- [ ] No Android implementation started before contract review.
- [ ] A human reviews the contract documents before native FFI work.

---

# Phase 1 — Loro Android native binding spike

This is the highest-risk phase. Stop here if the binding cannot be built reliably.

## Task 1.1: Choose the FFI crate boundary

**Description:** Identify whether to add a new Rust crate or extend an existing crate for the Android Loro wrapper. Keep the wrapper separate from application/domain logic.

**Likely files:**
- `Cargo.toml`
- `crates/` new or existing binding location
- `docs/android-loro-api.md`

**Acceptance criteria:**
- [ ] The chosen crate boundary is documented.
- [ ] The wrapper depends on the existing Loro version, not a second incompatible version.
- [ ] No desktop engine behavior is changed.
- [ ] The wrapper API contains only the operations from Task 0.3.

**Verification:**
- [ ] `cargo check -p <binding-crate>` or the appropriate workspace check passes.

**Dependencies:** Tasks 0.3 and 0.4.

**Estimated scope:** S.

## Task 1.2: Add UniFFI scaffolding for Android

**Description:** Add the minimum UniFFI configuration and Rust module needed to generate Android bindings. Do not expose Loro containers yet.

**Likely files:**
- Binding crate `Cargo.toml`
- Binding crate Rust source
- UniFFI configuration
- `Cargo.lock`

**Acceptance criteria:**
- [ ] UniFFI scaffolding compiles for the host target.
- [ ] The generated API has a stable package/module name.
- [ ] No unsafe public API is exposed without a documented reason.
- [ ] Existing workspace checks remain green.

**Verification:**
- [ ] `cargo fmt --check`.
- [ ] `cargo check -p <binding-crate>`.
- [ ] Run the binding generation command in a clean temporary output directory.

**Dependencies:** Task 1.1.

**Estimated scope:** M.

## Task 1.3: Expose a minimal native Loro document wrapper

**Description:** Implement the minimal wrapper for document creation, import/export, state reading, and update subscription required by Android.

**Acceptance criteria:**
- [ ] The wrapper supports creating a document.
- [ ] The wrapper imports a snapshot/update produced by the existing Rust implementation.
- [ ] The wrapper exports bytes that the existing Rust implementation can import.
- [ ] The wrapper exposes deterministic state reads needed by the Android stores.
- [ ] Subscription callbacks do not leak native resources.
- [ ] Errors cross the FFI boundary as typed/inspectable errors rather than panics.

**Verification:**
- [ ] Rust unit tests cover empty, snapshot import, update import, export, and malformed bytes.
- [ ] Rust tests verify round-trip convergence with an existing `zeron-doc` document.
- [ ] Run `cargo test -p <binding-crate>`.

**Dependencies:** Task 1.2.

**Estimated scope:** M.

## Task 1.4: Add Rust cross-platform fixture tests

**Description:** Test that Android-facing bytes remain compatible with the existing Rust document and protocol code.

**Acceptance criteria:**
- [ ] Fixtures cover at least workspace state and one session transcript.
- [ ] Rust imports every fixture successfully.
- [ ] Rust exports are stable enough for Kotlin tests to consume.
- [ ] Tests reject truncated and malformed data.

**Verification:**
- [ ] `cargo test -p <binding-crate>`.
- [ ] `cargo test -p zeron-doc`.

**Dependencies:** Tasks 1.3 and 0.4.

**Estimated scope:** S.

## Task 1.5: Document native memory and threading rules

**Description:** Document ownership, callback threading, disposal, and coroutine interaction rules for the Kotlin wrapper.

**Files:**
- `docs/android-loro-api.md`
- Generated/native wrapper comments

**Acceptance criteria:**
- [ ] Every long-lived native object has an explicit close/dispose rule.
- [ ] Callback thread behavior is specified.
- [ ] Kotlin must not call blocking native operations from the main thread.
- [ ] Cancellation behavior is specified.
- [ ] The document warns against retaining invalid native handles.

**Verification:**
- [ ] Review API comments against the implementation.

**Dependencies:** Task 1.3.

**Estimated scope:** XS.

## Checkpoint 1: Loro spike accepted

- [ ] Native library builds for a host target.
- [ ] Native library builds for `arm64-v8a`.
- [ ] Native library can be consumed from a minimal Kotlin test harness.
- [ ] Snapshot/update round trips pass.
- [ ] Malformed input produces errors, not process crashes.
- [ ] Memory disposal rules are documented.

If any item fails, do not proceed to full Android UI. Record the blocker and revise the binding boundary.

---

# Phase 2 — Android project scaffold

## Task 2.1: Create the Gradle Android application skeleton

**Description:** Create `apps/android` as a standalone native Android application using Kotlin and Compose. Keep generated files minimal and review all generated content.

**Files likely created:**
- `apps/android/settings.gradle.kts`
- `apps/android/build.gradle.kts`
- `apps/android/gradle.properties`
- `apps/android/app/build.gradle.kts`
- `apps/android/app/src/main/AndroidManifest.xml`
- `apps/android/app/src/main/java/.../MainActivity.kt`
- `apps/android/app/src/main/res/`
- Gradle wrapper files as appropriate

**Acceptance criteria:**
- [ ] The app opens a placeholder Compose screen.
- [ ] Minimum SDK is 26.
- [ ] Target/compile SDK uses the repository-approved current Android SDK.
- [ ] Debug build has a stable application id under the Zeron namespace.
- [ ] No secrets or local absolute paths are committed.

**Verification:**
- [ ] `./gradlew :app:assembleDebug`.
- [ ] `./gradlew :app:testDebugUnitTest`.

**Dependencies:** None, but coordinate with Phase 1 output.

**Estimated scope:** M.

## Task 2.2: Add Kotlin formatting, lint, and test baseline

**Description:** Add only the formatting/lint/test configuration needed to make every later task verifiable.

**Acceptance criteria:**
- [ ] Kotlin compilation and unit tests run from the wrapper.
- [ ] Static analysis has a documented command.
- [ ] Test source directories are present.
- [ ] Configuration does not disable useful warnings globally.

**Verification:**
- [ ] Run the documented format/check commands.
- [ ] Add one trivial test and confirm it runs, then keep or replace it with a meaningful scaffold test.

**Dependencies:** Task 2.1.

**Estimated scope:** S.

## Task 2.3: Configure arm64 APK and x86_64 CI native targets

**Description:** Configure Android packaging so internal APKs include only `arm64-v8a`, while CI can build/load `x86_64` for emulator tests.

**Acceptance criteria:**
- [ ] Release/internal APK packaging includes `arm64-v8a`.
- [ ] x86_64 can be compiled for test workflows.
- [ ] armeabi-v7a and x86 are not included.
- [ ] ABI behavior is documented.

**Verification:**
- [ ] Inspect APK contents with the Android build tooling.
- [ ] Build arm64 debug/release variants.
- [ ] Build x86_64 native test artifact if the FFI is already available.

**Dependencies:** Tasks 1.2 and 2.1.

**Estimated scope:** S.

## Task 2.4: Add Android documentation and local setup instructions

**Description:** Document JDK, Android SDK, NDK, Rust target, Gradle, and emulator/device setup without assuming a developer's local paths.

**Files:**
- `apps/android/README.md`

**Acceptance criteria:**
- [ ] A new developer can identify required tools and versions.
- [ ] Commands use the Gradle wrapper.
- [ ] arm64 device testing and x86_64 emulator testing are distinguished.
- [ ] No production credentials are required for unit tests.
- [ ] Dev edge configuration is clearly marked as development-only.

**Verification:**
- [ ] Follow instructions on a clean machine or CI-like environment as far as available.

**Dependencies:** Tasks 2.1–2.3.

**Estimated scope:** S.

## Task 2.5: Add native build integration

**Description:** Integrate the validated Rust/UniFFI library into the Android Gradle build without coupling application UI to build internals.

**Acceptance criteria:**
- [ ] Gradle invokes or consumes the native build reproducibly.
- [ ] arm64 native library is packaged into the APK.
- [ ] Native library loading has a single tested entry point.
- [ ] Build failures identify the missing toolchain clearly.
- [ ] No checked-in generated binaries are required unless explicitly documented.

**Verification:**
- [ ] Build debug APK from a clean output directory.
- [ ] Install/run on an arm64 device or compatible test target.
- [ ] Execute one native smoke test from Kotlin.

**Dependencies:** Checkpoint 1 and Tasks 2.1–2.3.

**Estimated scope:** M.

## Checkpoint 2: Android foundation accepted

- [ ] Placeholder app builds.
- [ ] Unit test task runs.
- [ ] Native library loads on arm64.
- [ ] ABI packaging matches the decision.
- [ ] Setup documentation is sufficient for another agent.

---

# Phase 3 — Kotlin application foundations

## Task 3.1: Define package structure and dependency boundaries

**Description:** Create package boundaries for `auth`, `protocol`, `loro`, `sync`, `data`, `ui`, and `testing`. Do not add feature code yet.

**Acceptance criteria:**
- [ ] UI does not import low-level WebSocket implementation directly.
- [ ] Repositories expose domain models/use cases rather than wire frames.
- [ ] Native Loro access is isolated behind a Kotlin interface.
- [ ] Test fakes can be supplied without Android framework dependencies.

**Verification:**
- [ ] Compile and inspect imports.

**Dependencies:** Checkpoint 2.

**Estimated scope:** S.

## Task 3.2: Add application configuration model

**Description:** Add typed configuration for edge URL, auth mode, deep-link scheme/host, and device environment. Keep production defaults safe.

**Acceptance criteria:**
- [ ] Production config does not point to a local/dev endpoint.
- [ ] Dev mode can be selected through a test/debug configuration.
- [ ] Configuration is injectable in unit tests.
- [ ] Invalid URLs/modes fail clearly.

**Verification:**
- [ ] Unit tests cover production, dev, invalid URL, and missing config.

**Dependencies:** Task 3.1.

**Estimated scope:** S.

## Task 3.3: Add common result/error model

**Description:** Define typed errors for auth, transport, protocol decoding, native Loro, authorization, server rejection, and cancellation.

**Acceptance criteria:**
- [ ] Errors preserve HTTP status or protocol code when available.
- [ ] User-facing messages do not expose tokens or sensitive payloads.
- [ ] Retryable versus permanent errors are distinguishable.
- [ ] Tests cover mapping of representative failures.

**Dependencies:** Task 3.1.

**Estimated scope:** S.

## Task 3.4: Add device identity persistence

**Description:** Generate and persist a stable Android device id using the same semantic shape expected by the registry protocol.

**Acceptance criteria:**
- [ ] Device id is stable across normal app restarts.
- [ ] Device id is not derived from a mutable display name.
- [ ] First-run generation is thread-safe.
- [ ] Reset behavior is documented and tested.

**Verification:**
- [ ] Unit tests cover first read, repeated read, and reset/fake storage.

**Dependencies:** Tasks 3.1 and 3.2.

**Estimated scope:** S.

## Task 3.5: Add secure token storage abstraction

**Description:** Create a storage interface and Android implementation backed by Keystore-compatible secure storage. Keep tokens out of logs and ordinary preferences.

**Acceptance criteria:**
- [ ] Access and refresh tokens are stored separately.
- [ ] Reads/writes/deletes are suspend-safe.
- [ ] Storage errors are surfaced.
- [ ] No token value is logged.
- [ ] Unit tests use an in-memory fake, not real secrets.

**Verification:**
- [ ] Unit tests cover save/load/delete and missing values.
- [ ] Device/manual check confirms sign-out removes credentials.

**Dependencies:** Tasks 3.2 and 3.3.

**Estimated scope:** M.

## Task 3.6: Add lifecycle/connection state model

**Description:** Define observable application states for signed out, signing in, selecting organization, connecting, ready, disconnected, and fatal error.

**Acceptance criteria:**
- [ ] States are mutually understandable and not overloaded.
- [ ] UI can render state without inspecting transport internals.
- [ ] Transitions are testable without Compose.
- [ ] Disconnected does not erase already loaded state.

**Dependencies:** Tasks 3.1–3.3.

**Estimated scope:** S.

---

# Phase 4 — Protocol and Loro Kotlin layer

## Task 4.1: Wrap generated UniFFI API with Kotlin-safe Loro interface

**Description:** Hide generated/native types behind a small Kotlin interface with explicit close and coroutine rules.

**Acceptance criteria:**
- [ ] UI/domain code does not import generated UniFFI names directly.
- [ ] Native resources are closed deterministically.
- [ ] Native callbacks are marshaled to the expected coroutine/context.
- [ ] Errors map to the common error model.

**Verification:**
- [ ] Kotlin unit tests use a fake implementation.
- [ ] Instrumented/native smoke test opens and closes a real document.

**Dependencies:** Checkpoint 1 and Task 3.1.

**Estimated scope:** M.

## Task 4.2: Port Loro protocol primitive codec

**Description:** Port the binary `loro-protocol` framing/encoding required by the edge rooms. Match the Rust and Swift implementation byte-for-byte.

**Acceptance criteria:**
- [ ] Magic/version/type/payload framing matches documented protocol.
- [ ] Lengths and integer encodings match existing implementations.
- [ ] Malformed/truncated frames are rejected safely.
- [ ] Unknown frame types have the documented behavior.

**Verification:**
- [ ] Kotlin tests decode shared valid fixtures.
- [ ] Kotlin tests reject malformed fixtures.
- [ ] Kotlin output is accepted by Rust fixture tests.

**Dependencies:** Tasks 0.2, 0.4, 3.3.

**Estimated scope:** M.

## Task 4.3: Implement registry frame codec

**Description:** Implement JSON/text or binary registry frame encoding/decoding as used by the existing registry client. Keep transport-independent.

**Acceptance criteria:**
- [ ] Hello/state/row/ack/error/presence frames needed by MVP are represented.
- [ ] Required numeric values preserve 64-bit precision.
- [ ] Unknown fields are tolerated where the protocol allows it.
- [ ] Unknown frame kinds do not crash the client.

**Verification:**
- [ ] Fixture-based unit tests cover every MVP frame.
- [ ] Tests cover wrong types, missing fields, and unknown fields.

**Dependencies:** Task 4.2.

**Estimated scope:** M.

## Task 4.4: Implement chat2 frame codec

**Description:** Implement the chat2 frame header/payload and length-prefixed response decoding used by iOS/Rust.

**Acceptance criteria:**
- [ ] Hello/state/rows request/row/rows done/push/ack/error/probe frames are supported as needed.
- [ ] Cursor values use unsigned 64-bit-safe handling.
- [ ] Checkpoint payload bytes remain opaque to the codec.
- [ ] Invalid frame lengths are rejected without buffer overrun.

**Verification:**
- [ ] Fixture tests cover empty room, checkpoint room, row backfill, push ack, quota, and permanent errors.

**Dependencies:** Tasks 0.2, 0.4, and 4.2.

**Estimated scope:** M.

## Task 4.5: Add Loro document adapter for workspace registry

**Description:** Build a typed adapter that reads registry document state into Kotlin domain models. Do not reimplement CRDT merge semantics.

**Acceptance criteria:**
- [ ] Spaces, chats, devices, statuses, and relevant fields are mapped.
- [ ] Missing optional fields use documented defaults.
- [ ] Malformed document values produce typed errors or skipped invalid rows according to policy.
- [ ] Mapping is deterministic.

**Verification:**
- [ ] Fixture tests compare expected domain models.
- [ ] Tests cover empty registry, archived chats, missing optional values, and unknown rows.

**Dependencies:** Tasks 0.3, 1.3, and 4.1.

**Estimated scope:** M.

## Task 4.6: Add Loro document adapter for session transcript and command ledger

**Description:** Map session document containers into transcript/domain models and append commands using the established writer discipline.

**Acceptance criteria:**
- [ ] Message parts and continuation parts are joined correctly.
- [ ] Tool, error, input, and text parts map to typed models.
- [ ] Command entries use client-minted ids and the existing command shapes.
- [ ] Android only writes allowed command/workspace fields.
- [ ] Host-owned transcript fields are never overwritten by the viewer.

**Verification:**
- [ ] Fixture tests cover normal messages, streamed text, tool groups, continuations, errors, input requests, and commands.
- [ ] Tests verify viewer writes do not mutate host-owned transcript data.

**Dependencies:** Tasks 0.3, 1.3, 4.1, and 4.5.

**Estimated scope:** M.

## Checkpoint 4: Data/protocol layer accepted

- [ ] Shared fixtures decode in Kotlin.
- [ ] Kotlin-produced frames match Rust/Swift expectations.
- [ ] Loro imports/exports pass cross-platform tests.
- [ ] Registry/session adapters return deterministic domain models.
- [ ] No UI code depends directly on wire or generated FFI types.

---

# Phase 5 — Auth and device registration

## Task 5.1: Implement auth HTTP client

**Description:** Implement `/auth/exchange`, `/auth/orgs`, and `/auth/refresh` using the existing field names and bearer rules.

**Acceptance criteria:**
- [ ] Requests use HTTPS in production configuration.
- [ ] JSON fields match iOS/edge exactly.
- [ ] Non-2xx responses preserve status and safe error text.
- [ ] Refresh supports organization scoping.
- [ ] Tokens never appear in logs or exceptions.

**Verification:**
- [ ] HTTP client unit tests use a fake server/interceptor.
- [ ] Tests cover successful exchange, org list, scoped refresh, invalid JSON, and non-2xx.

**Dependencies:** Tasks 3.2, 3.3, 3.5, and 4.2.

**Estimated scope:** M.

## Task 5.2: Implement WorkOS browser login and deep-link callback

**Description:** Open the external browser/Custom Tab, return through a registered deep link, validate callback state, and complete code exchange.

**Acceptance criteria:**
- [ ] Manifest intent filter matches only the intended callback.
- [ ] State/nonce validation prevents accepting an unrelated callback.
- [ ] Cancellation and browser failure return to a safe auth state.
- [ ] Callback code is exchanged once.
- [ ] Callback query parameters are not logged.

**Verification:**
- [ ] Unit tests cover state mismatch, missing code, cancellation, and duplicate callback.
- [ ] Manual device test completes a dev/staging login flow.

**Dependencies:** Task 5.1.

**Estimated scope:** M.

## Task 5.3: Implement organization selection and token scoping

**Description:** Display organizations after exchange and refresh the selected organization token before opening the workspace.

**Acceptance criteria:**
- [ ] Zero organizations produces a clear error.
- [ ] One organization can auto-select according to iOS behavior.
- [ ] Multiple organizations show a selection state.
- [ ] Selected org id is persisted only after successful refresh.
- [ ] Workspace does not open with an unscoped token when org scope is required.

**Verification:**
- [ ] State-machine tests cover zero/one/multiple orgs and refresh failure.

**Dependencies:** Tasks 3.6 and 5.1.

**Estimated scope:** M.

## Task 5.4: Implement dev auth mode

**Description:** Support the existing development bearer convention without enabling it in production builds by accident.

**Acceptance criteria:**
- [ ] Dev credentials are injectable through debug/test configuration.
- [ ] Production builds cannot silently use dev mode.
- [ ] Dev bearer formatting matches the edge implementation.
- [ ] No dev identity is persisted as a production account.

**Verification:**
- [ ] Unit tests cover user-only and user-plus-org dev identities.
- [ ] Integration test connects to a local/dev edge when available.

**Dependencies:** Tasks 3.2, 3.5, and 5.1.

**Estimated scope:** S.

## Task 5.5: Implement sign-out and token refresh lifecycle

**Description:** Clear tokens, stop connections, clear account-scoped state, and refresh access tokens before expiry or after an auth failure.

**Acceptance criteria:**
- [ ] Sign-out closes registry/chat transports.
- [ ] Sign-out removes access and refresh tokens.
- [ ] Refresh is serialized to avoid token races.
- [ ] Failed refresh returns to signed-out state.
- [ ] Existing UI state is not treated as authenticated after sign-out.

**Verification:**
- [ ] Unit tests cover concurrent refresh, refresh failure, sign-out during refresh, and expired access token.

**Dependencies:** Tasks 3.5, 3.6, and 5.1.

**Estimated scope:** M.

## Task 5.6: Implement device registration

**Description:** Register the Android device through the existing registry/workspace mechanism and expose its presence state.

**Acceptance criteria:**
- [ ] Registration uses the persisted device id.
- [ ] Device name is user-safe and does not contain secrets.
- [ ] Duplicate registration is harmless.
- [ ] Registration failure is visible and retryable.

**Verification:**
- [ ] Protocol fixture/integration tests cover first registration and repeat registration.

**Dependencies:** Tasks 3.4, 4.3, 4.5, and 5.5.

**Estimated scope:** M.

## Checkpoint 5: Auth accepted

- [ ] WorkOS and dev auth state machines pass tests.
- [ ] Tokens are secure and never logged.
- [ ] Deep link callback rejects invalid state.
- [ ] Sign-out fully disconnects and clears credentials.
- [ ] Device registration is visible in the registry.

---

# Phase 6 — Registry and chat synchronization

## Task 6.1: Add HTTP transport abstraction

**Description:** Add a testable HTTP abstraction used by auth and chat backfill requests. Keep it independent of Compose.

**Acceptance criteria:**
- [ ] Requests support headers, body, status, and streaming where needed.
- [ ] Cancellation is propagated.
- [ ] Tests can provide deterministic responses.
- [ ] Authorization headers are attached centrally and safely.

**Dependencies:** Task 3.1.

**Estimated scope:** S.

## Task 6.2: Add WebSocket transport abstraction

**Description:** Add a testable WebSocket abstraction for registry/chat clients, including binary/text messages, close, and cancellation.

**Acceptance criteria:**
- [ ] Transport does not expose a concrete library to domain code.
- [ ] Receive loop terminates on close/cancellation.
- [ ] Send failures are surfaced.
- [ ] Tests can script inbound frames and failures.

**Dependencies:** Task 3.1.

**Estimated scope:** S.

## Task 6.3: Implement registry WebSocket lifecycle

**Description:** Implement registry hello, frame receive, state application, presence handling, reconnect, and foreground kick behavior based on iOS/Rust semantics.

**Acceptance criteria:**
- [ ] Registry joins with the persisted device/cursor state.
- [ ] State and row frames update the registry document through the Loro adapter.
- [ ] Reconnect uses bounded exponential backoff.
- [ ] Liveness deadlines prevent indefinite hanging.
- [ ] Unknown future frames are tolerated where permitted.

**Verification:**
- [ ] Fake WebSocket tests cover successful join, malformed frame, server close, reconnect, and liveness timeout.

**Dependencies:** Tasks 4.3, 4.5, 6.2, and 5.6.

**Estimated scope:** L; split if it exceeds one focused session.

## Task 6.4: Implement chat2 join and state planning

**Description:** Implement chat2 hello/state handling, cursor amnesty, checkpoint presence detection, and rows request planning without yet adding UI.

**Acceptance criteria:**
- [ ] Hello includes the correct cursor/device fields.
- [ ] State header parsing matches Rust/Swift.
- [ ] Checkpoint-required versus rows-only plans match existing semantics.
- [ ] Cursor never advances over an unreceived gap.
- [ ] Unknown server fields are handled safely.

**Verification:**
- [ ] Fixture tests cover empty, resumed, checkpoint, reset, and cursor-gap cases.

**Dependencies:** Tasks 4.4 and 4.6.

**Estimated scope:** M.

## Task 6.5: Implement chat2 checkpoint and row backfill

**Description:** Implement HTTP checkpoint/row fetch, length-prefixed frame parsing, checkpoint import, buffered rows, and cursor advancement.

**Acceptance criteria:**
- [ ] Checkpoint bytes import through the Loro adapter.
- [ ] Rows received during checkpoint import are buffered and replayed in order.
- [ ] Partial/malformed responses do not advance the cursor incorrectly.
- [ ] Cursor advances only through contiguous rows.
- [ ] Retry behavior is bounded and cancellation-safe.

**Verification:**
- [ ] Tests cover checkpoint success, checkpoint failure, rows during checkpoint, malformed response, and gap repair.

**Dependencies:** Tasks 4.4, 4.6, 6.1, and 6.4.

**Estimated scope:** L; split checkpoint and row processing if needed.

## Task 6.6: Implement chat2 live push and ack handling

**Description:** Queue command/document update batches, send them after state is received, process ack/error/quota responses, and preserve durable retry semantics.

**Acceptance criteria:**
- [ ] Pending batches survive WebSocket reconnect in the repository/document layer.
- [ ] Batch ids are unique and deduplicate replays.
- [ ] Permanent errors retire only the rejected batch.
- [ ] Quota errors retain the batch and retry according to policy.
- [ ] Ack gaps trigger backfill rather than cursor jumps.

**Verification:**
- [ ] Tests cover send, reconnect replay, duplicate ack, quota, permanent rejection, and gap repair.

**Dependencies:** Tasks 4.4, 4.6, 6.2, and 6.5.

**Estimated scope:** M.

## Task 6.7: Implement chat2 liveness and foreground reconnect

**Description:** Add ping/probe deadlines, backoff, cancellation, and an app-foreground hook. Do not rely on WebSocket auto-pong as proof of server health.

**Acceptance criteria:**
- [ ] Protocol frames, not only pongs, prove room health.
- [ ] Silent sockets are replaced after the documented deadline.
- [ ] Foregrounding triggers a safe kick/reconnect.
- [ ] Backoff resets only after stable connection according to policy.
- [ ] No timer survives repository shutdown.

**Verification:**
- [ ] Fake-clock or deterministic timer tests cover silence, probe success, probe timeout, foreground kick, and shutdown.

**Dependencies:** Task 6.5 and 6.6.

**Estimated scope:** M.

## Task 6.8: Add workspace/session repository APIs

**Description:** Expose clean application-level flows for observing workspace/session state and queueing run/steer/interrupt/respond-input commands.

**Acceptance criteria:**
- [ ] UI receives immutable/state-flow-friendly models.
- [ ] Repository owns transport and Loro lifecycle.
- [ ] Commands are optimistic only where iOS semantics allow it.
- [ ] Host-owned transcript state is read-only to the Android viewer.
- [ ] Repository shutdown is idempotent.

**Verification:**
- [ ] Unit tests cover observe, open session, queue prompt, steer, interrupt, input response, archive, and shutdown.

**Dependencies:** Tasks 4.5, 4.6, 6.3, and 6.7.

**Estimated scope:** M.

## Checkpoint 6: Sync accepted

- [ ] Registry and chat fake-server tests pass.
- [ ] Loro docs converge from shared fixtures.
- [ ] Reconnect does not skip rows.
- [ ] Commands remain durable and deduplicated.
- [ ] Foreground/background lifecycle does not leak timers or sockets.

---

# Phase 7 — Compose navigation and auth UI

## Task 7.1: Add app root and navigation state

**Description:** Create the Compose root that renders auth, organization selection, loading, workspace, and error states from the application state model.

**Acceptance criteria:**
- [ ] Navigation does not inspect WebSocket internals.
- [ ] Back behavior is defined for auth/org/workspace screens.
- [ ] Recomposition does not create duplicate repositories or sockets.
- [ ] Loading and error states are accessible.

**Verification:**
- [ ] Compose tests cover signed-out, loading, org picker, ready, and error states.

**Dependencies:** Tasks 3.6 and 6.8.

**Estimated scope:** M.

## Task 7.2: Build sign-in screen

**Description:** Implement the external-browser sign-in entry point and dev-mode sign-in controls for debug builds only.

**Acceptance criteria:**
- [ ] Production screen does not expose dev credentials.
- [ ] Sign-in progress disables duplicate submission.
- [ ] Errors are user-readable and retryable.
- [ ] Accessibility labels exist for primary controls.

**Verification:**
- [ ] Compose tests cover idle, loading, error, and success callback states.

**Dependencies:** Task 5.2 and 7.1.

**Estimated scope:** S.

## Task 7.3: Build organization picker

**Description:** Implement zero/one/multiple organization states and selection confirmation.

**Acceptance criteria:**
- [ ] Organization names are displayed safely.
- [ ] Selecting an org cannot submit twice.
- [ ] Refresh failure returns a recoverable error.
- [ ] The workspace opens only after scoped auth succeeds.

**Verification:**
- [ ] Compose tests cover all org counts and failures.

**Dependencies:** Task 5.3 and 7.1.

**Estimated scope:** S.

## Task 7.4: Add lifecycle collection and sign-out UI

**Description:** Connect app lifecycle to repository foreground kick and add a sign-out action that fully resets the session.

**Acceptance criteria:**
- [ ] Foreground event invokes the repository hook once per transition.
- [ ] Sign-out returns to sign-in screen.
- [ ] Credentials and active room subscriptions are cleared.
- [ ] No stale workspace screen remains reachable after sign-out.

**Verification:**
- [ ] Unit/state tests cover lifecycle transitions and sign-out.
- [ ] Manual device test confirms sign-out behavior.

**Dependencies:** Tasks 5.5, 6.7, and 7.1.

**Estimated scope:** M.

---

# Phase 8 — Workspace and session UI

## Task 8.1: Build home/workspace screen

**Description:** Display workspace spaces and sessions from the registry repository, including loading, empty, disconnected, and error states.

**Acceptance criteria:**
- [ ] Sessions are sorted using the documented iOS/desktop attention rules or the approved Android equivalent.
- [ ] Archived sessions are separated or filtered consistently.
- [ ] Device/space identity is clear.
- [ ] Tapping a session opens its transcript.

**Verification:**
- [ ] View-model tests cover sorting, empty state, archived state, and stale connection.
- [ ] Compose tests cover list rendering and navigation.

**Dependencies:** Tasks 4.5, 6.8, and 7.1.

**Estimated scope:** M.

## Task 8.2: Implement archive/unarchive actions

**Description:** Add Android-native archive controls that write the established LWW field through the workspace repository.

**Acceptance criteria:**
- [ ] Archive/unarchive uses the existing mutation shape.
- [ ] UI updates optimistically only with a safe rollback/error path.
- [ ] Repeated action is idempotent.
- [ ] Archived session is not accidentally deleted.

**Verification:**
- [ ] Repository and Compose tests cover success, failure, retry, and repeated action.

**Dependencies:** Task 8.1 and 6.8.

**Estimated scope:** S.

## Task 8.3: Build session screen shell

**Description:** Add session header, connection/status area, transcript container, and composer placeholder.

**Acceptance criteria:**
- [ ] Session store lifecycle is tied to screen ownership correctly.
- [ ] Reopening a session does not create duplicate chat connections.
- [ ] Status is visible without blocking transcript rendering.
- [ ] Back navigation stops or detaches resources according to repository policy.

**Verification:**
- [ ] Compose/navigation tests cover open, recompose, rotate/process recreation as supported, and close.

**Dependencies:** Task 8.1 and 6.8.

**Estimated scope:** M.

## Task 8.4: Add session status and error chips

**Description:** Render working, waiting/input, completed, archived, disconnected, and error states using native Android components.

**Acceptance criteria:**
- [ ] Status mapping is centralized and tested.
- [ ] Error text is safe and actionable.
- [ ] Input-request state is visually distinct.
- [ ] Status does not depend on parsing raw server messages in Compose.

**Verification:**
- [ ] Unit tests cover every status mapping.
- [ ] Compose tests cover visible state variants.

**Dependencies:** Task 8.3 and 4.6.

**Estimated scope:** S.

---

# Phase 9 — Transcript rendering

## Task 9.1: Define transcript presentation models

**Description:** Convert session domain parts into stable UI rows with stable keys, grouping, and continuation joining.

**Acceptance criteria:**
- [ ] Row keys remain stable while text streams.
- [ ] Tool calls/results group deterministically.
- [ ] Continuation parts do not duplicate visible text.
- [ ] Input/error/tool/text rows have explicit types.

**Verification:**
- [ ] Pure Kotlin tests cover streaming append, tool grouping, continuation joining, and malformed parts.

**Dependencies:** Tasks 4.6 and 8.3.

**Estimated scope:** M.

## Task 9.2: Add markdown parser abstraction

**Description:** Add a parser boundary for markdown blocks. Use a maintained Kotlin-compatible parser after verifying dependency/license; do not hardcode an unverified library.

**Acceptance criteria:**
- [ ] Parser dependency and license are documented.
- [ ] Parser output is block-oriented.
- [ ] Unsupported markdown constructs degrade safely.
- [ ] Parsing can be cancelled/coalesced for streaming updates.

**Verification:**
- [ ] Unit tests cover headings, lists, links, tables, strikethrough, task lists, inline code, and malformed markdown.

**Dependencies:** Task 9.1.

**Estimated scope:** M.

## Task 9.3: Build markdown Compose renderer

**Description:** Render the supported markdown block model with Android-native layout, including code blocks and inline code.

**Acceptance criteria:**
- [ ] Text remains readable on phone portrait widths.
- [ ] Code blocks preserve line boundaries and support horizontal overflow or safe wrapping policy.
- [ ] Links have accessible semantics.
- [ ] Tables do not crash or produce unbounded layout.
- [ ] Renderer does not perform expensive parsing in composition.

**Verification:**
- [ ] Compose tests cover representative blocks.
- [ ] Manual screenshot/device check covers narrow phone width.

**Dependencies:** Task 9.2.

**Estimated scope:** M.

## Task 9.4: Add syntax highlighting as paint-only enhancement

**Description:** Add syntax highlighting for code blocks without allowing highlighting to change measured code layout.

**Acceptance criteria:**
- [ ] Highlighting failure falls back to plain code.
- [ ] Highlighting does not block the main thread.
- [ ] Code height is based on the defined wrapping/line policy, not token colors.
- [ ] Language labels are optional and safe.

**Verification:**
- [ ] Unit tests cover known language, unknown language, malformed code, and large code block fallback.
- [ ] Performance smoke check confirms no obvious main-thread stall.

**Dependencies:** Task 9.3.

**Estimated scope:** M.

## Task 9.5: Add tool grouping and collapsible tool UI

**Description:** Render tool calls/results as grouped rows with compact and expanded states.

**Acceptance criteria:**
- [ ] Tool grouping is stable during streaming.
- [ ] Tool input/output is escaped and never rendered as executable markup.
- [ ] Error tool results are visually distinct.
- [ ] Large outputs have a bounded rendering strategy.

**Verification:**
- [ ] Presentation tests cover consecutive tools, interleaved text, errors, and large output.
- [ ] Compose tests cover collapsed/expanded behavior.

**Dependencies:** Tasks 9.1 and 9.3.

**Estimated scope:** M.

## Task 9.6: Add streaming updates and stick-to-bottom behavior

**Description:** Update the transcript as Loro/session state changes while preserving the user's scroll position and following the tail when appropriate.

**Acceptance criteria:**
- [ ] New content is visible without rebuilding unrelated rows.
- [ ] User scroll-up stops automatic following.
- [ ] Returning near the bottom re-enables following.
- [ ] Own sent prompt scrolls to the relevant position.
- [ ] No unbounded coroutine or snapshot subscription is created.

**Verification:**
- [ ] Compose UI tests cover append while pinned, append while scrolled up, re-engage, and session reopen.
- [ ] Manual device test checks a long streaming response.

**Dependencies:** Tasks 9.1–9.5 and 6.8.

**Estimated scope:** M.

## Checkpoint 9: Transcript accepted

- [ ] Full required markdown constructs render.
- [ ] Tool/error/input rows render correctly.
- [ ] Streaming does not flicker or duplicate continuation content.
- [ ] Scroll behavior is stable on a phone.
- [ ] Large output has bounded behavior.

---

# Phase 10 — Composer, steering, and input requests

## Task 10.1: Build composer state model

**Description:** Define draft, sending, steering, stopping, disabled, and input-request states without coupling UI to transport details.

**Acceptance criteria:**
- [ ] Draft is scoped to a session.
- [ ] Send/steer/stop availability is derived from state.
- [ ] Failed send can restore the draft safely.
- [ ] Empty/whitespace-only messages are rejected locally.

**Verification:**
- [ ] Pure Kotlin tests cover every state transition.

**Dependencies:** Tasks 3.6, 6.8, and 8.3.

**Estimated scope:** S.

## Task 10.2: Build prompt composer UI

**Description:** Add a native Compose text input with send action, multiline behavior, keyboard handling, and accessibility labels.

**Acceptance criteria:**
- [ ] Send action is available from keyboard and visible button.
- [ ] Multiline input does not submit unexpectedly according to the defined Android behavior.
- [ ] Input remains usable on a small phone.
- [ ] Sending disables duplicate submissions.

**Verification:**
- [ ] Compose tests cover typing, empty input, send, loading, and failure.
- [ ] Manual keyboard test on an arm64 device.

**Dependencies:** Task 10.1.

**Estimated scope:** M.

## Task 10.3: Connect prompt sending and optimistic echo

**Description:** Queue a run command through the session repository and show the user message according to the durable command/optimistic echo rules.

**Acceptance criteria:**
- [ ] User message id is client-minted as required.
- [ ] Command is written to the session document, not sent as an ad-hoc RPC.
- [ ] Failure returns the draft or an explicit retry state.
- [ ] Reconnect does not duplicate the command.

**Verification:**
- [ ] Repository tests cover success, transport failure, reconnect, and duplicate submission.

**Dependencies:** Tasks 4.6, 6.6, and 10.2.

**Estimated scope:** M.

## Task 10.4: Add steering and stop controls

**Description:** Morph composer actions for active sessions and queue steer/interrupt commands using established shapes.

**Acceptance criteria:**
- [ ] Steering is available only when the session state allows it.
- [ ] Stop/interrupt is explicit and idempotent.
- [ ] Idle-session steering follows the existing next-turn semantics.
- [ ] UI reflects queued versus acknowledged command state.

**Verification:**
- [ ] State and repository tests cover active, idle, disconnected, duplicate, and failure cases.

**Dependencies:** Tasks 10.1–10.3.

**Estimated scope:** M.

## Task 10.5: Build input-request question panel

**Description:** Render agent questions and submit `respondInput` commands through the durable command ledger.

**Acceptance criteria:**
- [ ] Questions and choices are displayed safely.
- [ ] Required/optional answers are validated.
- [ ] Selection cannot submit twice.
- [ ] Cancel behavior matches the established command semantics.
- [ ] Input request disappears only after state confirms response or an explicit terminal outcome.

**Verification:**
- [ ] Unit tests cover single/multiple choice, free text if supported, invalid answers, duplicate submit, and reconnect.
- [ ] Compose tests cover keyboard/accessibility navigation.

**Dependencies:** Tasks 4.6, 8.4, and 10.1.

**Estimated scope:** M.

## Checkpoint 10: Interaction accepted

- [ ] User can send a prompt end-to-end.
- [ ] User can steer/stop an active session.
- [ ] User can answer an input request.
- [ ] Failure and reconnect do not lose durable commands.
- [ ] Composer remains usable with keyboard visible.

---

# Phase 11 — Reliability, security, and accessibility

## Task 11.1: Audit logging and sensitive-data handling

**Description:** Review Android logs, exceptions, analytics hooks, and crash reporting for tokens, prompts, code, paths, and repository data leakage.

**Acceptance criteria:**
- [ ] Tokens never appear in logs.
- [ ] Full prompts/code are not logged by default.
- [ ] WebSocket payloads are not logged in production.
- [ ] Deep-link callback values are not logged.
- [ ] Debug logging is explicitly gated.

**Verification:**
- [ ] Search source for token/payload logging.
- [ ] Add regression tests where practical.

**Dependencies:** After Tasks 5–10.

**Estimated scope:** M.

## Task 11.2: Add transport retry and error UX audit

**Description:** Verify every retryable transport failure has bounded retry behavior and a visible UI state.

**Acceptance criteria:**
- [ ] No infinite tight retry loop exists.
- [ ] No failed send is silently discarded.
- [ ] Auth failures route to auth recovery.
- [ ] Protocol failures are distinguishable from offline transport failures.
- [ ] User can retry where appropriate.

**Verification:**
- [ ] Run scripted failure tests for auth, registry, chat, and command send.

**Dependencies:** Tasks 5.5, 6.7, 7.4, and 10.3.

**Estimated scope:** M.

## Task 11.3: Add accessibility semantics

**Description:** Add content descriptions, traversal order, roles, touch target sizes, and readable error semantics.

**Acceptance criteria:**
- [ ] Primary actions have content descriptions.
- [ ] Session status is exposed to accessibility services.
- [ ] Transcript code and tool rows remain understandable.
- [ ] Color is not the only status signal.
- [ ] Touch targets meet Android accessibility guidance.

**Verification:**
- [ ] Compose semantics tests cover primary screens.
- [ ] Manual TalkBack pass on sign-in, session, composer, and input panel.

**Dependencies:** Tasks 7–10.

**Estimated scope:** M.

## Task 11.4: Add configuration-change and process-recreation handling

**Description:** Verify that screen recreation does not duplicate sockets, lose drafts unexpectedly, or corrupt native Loro handles.

**Acceptance criteria:**
- [ ] ViewModels/repositories survive normal Compose recomposition.
- [ ] Native handles are recreated or restored safely.
- [ ] Active subscriptions are not duplicated.
- [ ] Unsaved draft behavior is documented.

**Verification:**
- [ ] Instrumented tests cover recreation where supported.
- [ ] Manual device rotation/process recreation check, even though portrait is primary.

**Dependencies:** Tasks 4.1, 6.8, and 8.3.

**Estimated scope:** M.

## Task 11.5: Add network/security policy configuration

**Description:** Configure HTTPS, cleartext restrictions, deep-link verification, and safe certificate behavior without weakening production security for local development.

**Acceptance criteria:**
- [ ] Production disallows unintended cleartext traffic.
- [ ] Dev cleartext exceptions are debug-only and narrowly scoped.
- [ ] App links/deep links are restricted to expected hosts/schemes.
- [ ] No trust-all certificate manager exists.

**Verification:**
- [ ] Inspect manifest/network security config.
- [ ] Test production-like build against invalid TLS/configuration.

**Dependencies:** Tasks 2.1, 3.2, and 5.2.

**Estimated scope:** S.

---

# Phase 12 — CI, integration tests, and internal APK

## Task 12.1: Add Android unit-test CI job

**Description:** Run Kotlin unit tests, lint/static checks, and the appropriate native host/build checks in CI.

**Acceptance criteria:**
- [ ] CI uses pinned/declared JDK, Android SDK, NDK, Rust, and Gradle inputs.
- [ ] No credentials are required for unit/contract tests.
- [ ] Failures identify the exact task.
- [ ] CI caches are safe and do not hide stale generated code.

**Verification:**
- [ ] Run the CI commands locally where possible.

**Dependencies:** Tasks 2.2, 2.5, and 4.2.

**Estimated scope:** M.

## Task 12.2: Add x86_64 emulator/instrumented test support

**Description:** Compile the x86_64 native test artifact and run a small instrumented smoke suite without adding x86_64 to the internal arm64 APK.

**Acceptance criteria:**
- [ ] Emulator ABI configuration is explicit.
- [ ] Native Loro smoke test runs on x86_64.
- [ ] Artifact packaging remains arm64-only for internal release.
- [ ] Tests avoid production auth.

**Verification:**
- [ ] Run the emulator smoke suite in CI or a documented local equivalent.

**Dependencies:** Tasks 2.3, 2.5, 4.1, and 12.1.

**Estimated scope:** M.

## Task 12.3: Add fake-edge integration test suite

**Description:** Build deterministic fake HTTP/WebSocket servers or protocol fixtures that exercise auth, registry, chat backfill, live updates, command ack, and reconnect.

**Acceptance criteria:**
- [ ] Tests cover successful core flow without real credentials.
- [ ] Tests cover malformed frames and disconnects.
- [ ] Tests verify cursor/command durability semantics.
- [ ] Tests use the same field/frame shapes as production fixtures.

**Verification:**
- [ ] Run the complete fake-edge suite in CI.

**Dependencies:** Tasks 4.2–4.6, 5.1, 6.3–6.7.

**Estimated scope:** L; split by auth/registry/chat if needed.

## Task 12.4: Add end-to-end Android core-flow test

**Description:** Test sign-in/dev auth → workspace → open session → receive transcript → send prompt → observe response/input state → archive.

**Acceptance criteria:**
- [ ] Test uses a controlled dev/fake backend.
- [ ] Test does not depend on production model calls.
- [ ] Test asserts visible user outcomes, not only internal calls.
- [ ] Failures capture useful logs without sensitive payloads.

**Verification:**
- [ ] Run on the supported emulator/device target.

**Dependencies:** Tasks 7–10 and 12.3.

**Estimated scope:** M.

## Task 12.5: Produce internal arm64 APK

**Description:** Build a reproducible debug/internal APK with arm64 native library and documented installation steps.

**Acceptance criteria:**
- [ ] APK contains only the approved internal ABI.
- [ ] Version/name identify the build source.
- [ ] Installation succeeds on a supported arm64 device.
- [ ] App can complete the core-flow test against dev/fake backend.
- [ ] No signing secrets or release keystore are committed.

**Verification:**
- [ ] `./gradlew :app:assembleDebug` or the approved internal task.
- [ ] Inspect APK ABI contents.
- [ ] Install and run smoke test.

**Dependencies:** Tasks 11.1–11.5 and 12.1–12.4.

**Estimated scope:** M.

## Checkpoint 12: MVP release candidate

- [ ] Unit, contract, instrumentation, and core-flow tests pass.
- [ ] arm64 APK installs and runs.
- [ ] No production secrets are required.
- [ ] Security/logging audit passes.
- [ ] Known limitations are documented.
- [ ] A human can test the app using `apps/android/README.md`.

---

# 6. Safe parallelization

The following may run in parallel **only after their contracts are fixed**:

- Documentation tasks 0.1–0.4.
- Kotlin test fixture preparation after Task 0.4.
- Compose placeholder screens after Checkpoint 2, provided they use fakes.
- Pure presentation-model tests after Task 4.6.
- Accessibility review after the relevant screen exists.

The following must remain sequential:

- Loro FFI boundary → generated bindings → Gradle integration.
- Protocol fixtures → codec implementation → transport clients.
- Auth/token storage → authenticated registry/chat clients.
- Loro adapters → repositories → UI.
- Fake-edge integration → end-to-end test → APK release.

Do not assign two agents to edit the same protocol or Gradle files simultaneously.

# 7. Risks and mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| No official Kotlin Loro binding | High | Keep UniFFI wrapper minimal; validate it before UI work. |
| UniFFI callback/threading complexity | High | Document ownership; expose Kotlin-safe wrapper; test disposal and cancellation. |
| Android native build instability | High | Pin toolchains; compile arm64 release and x86_64 CI separately; add CI early. |
| Protocol drift from iOS/Rust | High | Shared fixtures and byte-level contract tests. |
| WebSocket lifecycle leaks | High | Transport abstraction, deterministic timers, foreground kick, shutdown tests. |
| Cursor jumps skip transcript rows | High | Preserve contiguous cursor advancement and gap-repair tests. |
| Token/prompt leakage in logs | High | Central logging policy and source audit before APK. |
| Compose recomposition duplicates sockets | Medium | Repository/ViewModel ownership tests and lifecycle review. |
| Full markdown renderer scope expands | Medium | Define supported block model first; fallback safely for unsupported syntax. |
| Phone-only layout limits future tablets | Low | Avoid fixed dimensions; defer tablet-specific design explicitly. |

# 8. Definition of done

The Android MVP is complete only when:

- `apps/android` builds reproducibly with the documented toolchain.
- The arm64 APK installs on a supported device.
- Auth works in WorkOS/staging and dev mode according to configuration.
- The app joins the existing registry/chat protocols.
- Loro state converges with iOS/Rust fixtures.
- A user can open a session, watch live transcript updates, send a prompt, steer/stop, answer input requests, and archive/unarchive.
- Markdown/tool/error/input transcript rendering is usable on a phone.
- Reconnect and foreground lifecycle behavior are tested.
- Contract, unit, Compose, instrumentation, and core-flow tests pass.
- No credentials, tokens, or private user data are committed or logged.
- Known limitations are documented in `apps/android/README.md` and the progress ledger.
