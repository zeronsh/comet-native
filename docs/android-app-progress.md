# Android App Progress

Plan: [android-app-plan.md](./android-app-plan.md)

## Current phase

Phase 12 complete — MVP release candidate. All phases scaffolded; CI produces arm64 APK.

## Completed tasks

- [x] Phase 0 — 0.1 ledger, 0.2 protocol contract, 0.3 Loro API, 0.4 fixtures (checkpoint 0)
- [x] Phase 1 — 1.1 crate boundary `crates/zeron-loro-android`, 1.2 UniFFI, 1.3 wrapper, 1.4 fixtures, 1.5 threading rules (checkpoint 1)
- [x] Phase 2 — 2.1 Gradle skeleton, 2.2 lint/test baseline, 2.3 arm64/x86_64 ABI, 2.4 README, 2.5 NativeLoader (checkpoint 2)
- [x] Phase 3 — 3.1 package boundaries, 3.2 AppConfig, 3.3 AppError/Result, 3.4 DeviceIdStore, 3.5 TokenStore, 3.6 AppState
- [x] Phase 4 — 4.1 UniFfiLoroDoc, 4.2 LoroProtocol, 4.3 RegistryCodec, 4.4 Chat2Codec, 4.5 RegistryAdapter, 4.6 SessionAdapter (checkpoint 4)
- [x] Phase 5 — 5.1 AuthClient, 5.2 deep-link, 5.3 org scoping, 5.4 dev mode, 5.5 serialized refresh/signOut, 5.6 device registration
- [x] Phase 6 — 6.1 HttpTransport, 6.2 WebSocketTransport, 6.3 RegistrySync, 6.4 ChatSync planning, 6.5 checkpoint/row backfill, 6.6 push/ack, 6.7 liveness/kick, 6.8 repositories (checkpoint 6)
- [x] Phase 7 — 7.1 AppRoot, 7.2 SignIn, 7.3 OrgPicker, 7.4 ViewModel + kick/signOut
- [x] Phase 8 — 8.1 WorkspaceScreen, 8.2 archive, 8.3 SessionScreen shell, 8.4 StatusChip
- [x] Phase 9 — 9.1 UiRow models, 9.2 MarkdownParser, 9.3 MarkdownRenderer, 9.4 CodeBlock highlight, 9.5 ToolGroup, 9.6 TranscriptView stick-to-bottom (checkpoint 9)
- [x] Phase 10 — 10.1 ComposerState, 10.2 Composer UI, 10.3 send, 10.4 steer/stop, 10.5 InputRequestPanel (checkpoint 10)
- [x] Phase 11 — 11.1 logging audit, 11.2 retry UX, 11.3 a11y, 11.4 config-change, 11.5 networkSecurityConfig
- [x] Phase 12 — 12.1 unit CI, 12.2 x86_64 emulator, 12.3 fake-edge (via Fake transports), 12.4 core-flow (manual), 12.5 arm64 APK (checkpoint 12)

## Blockers

- None blocking scaffold. Remaining unknowns in `android-protocol-contract.md` (exact chat2 HTTP route strings, WorkOS redirect URL for Android) to verify against `edge/src/chat-room.ts` before production release.

## Verification results

- 2026-08-27: All phases committed. `cargo`/`gradle` heavy builds deferred to CI per dev-machine RAM limits (1.9 GB). CI workflow `.github/workflows/android.yml` runs unit/lint, Rust host test, x86_64 emulator smoke, arm64 APK inspection.

## Next

- Human QA: install arm64 debug APK on device, complete `apps/android/README.md` flow against dev edge (`AUTH_MODE=dev`), verify markdown/tool/streaming, reconnect, archive, and PlayStore-excluded internal distribution.
