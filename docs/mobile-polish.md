# Mobile transcript and interaction polish

The mobile transcript keeps a small, eagerly laid-out tail beneath virtualized
history. A fully lazy tail could claim to be at its estimated bottom while
instantiating no visible rows after an append. The hosted simulator regression
captured an entirely blank viewport with a reported bottom distance of 0.5pt.
The new layout realizes the newest turn (bounded to 48 rows for very long
turns) and uses explicit bottom targets rather than computed global offsets.
Corrections follow content/viewport changes, not each scroll-geometry callback.
Automatic bottom anchoring on size changes caused a SwiftUI layout feedback loop
with mixed lazy/eager content on iPhone 17 Pro; the regression suite covers the
burst append that exposed it.

Follow intent changes only on a real user drag or an explicit send/jump.
Composer focus, keyboard resizing, tool expansion, and streaming growth do
not release it. The composer occupies real sibling layout space, preserving
its glass capsule/card morph while keeping transcript content above it. Compact
landscape uses a horizontal composer so the keyboard cannot cover its controls
or consume the entire transcript viewport.

A local submission reserves one viewport beneath the prompt. The reply consumes
that space during layout; short completed replies keep the runway until the
session is reopened. Long output naturally hands over to following the tail.
Remote user entries do not create a local-send runway.

The visual pass adopts Anara iOS's 17pt chat-body scale and larger secondary
labels, while retaining Zeron's Geist fonts and desktop palette. Code line
heights and list markers scale with Dynamic Type. Tool calls use the desktop
activity rail with quiet group summaries, per-tool failure labels, expandable
commands, and copy actions. Core composer and jump controls have 44pt targets
and explicit accessibility labels.

The project selector uses the native glass Menu button style. Applying glass
inside the Menu label allowed UIKit to restore a stale clipped label mask after
selection or dismissal. The menu itself now owns its glass surface and morph.

## Reproduce and test

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
xcodebuild -project apps/ios/Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test
```

`ZeronTests/TranscriptFollowTests` checks follow state independently of layout.
`ZeronTests/TranscriptLayoutTests` mounts the real SwiftUI transcript in a
simulator window and measures exact tail-row and viewport frames. Debug probes
are enabled only by these tests and compile to no-ops in release builds.

Coverage includes warm and delayed 600-turn histories, burst appends, a large
shrink, repeated warm reopens, composer/keyboard viewport changes, narrow and
landscape sizes, accessibility text resizing, and runway retention/overflow.

`ZeronUITests/MobilePolishTests` exercises actual project selection/dismissal,
session navigation, scroll gestures, jump-to-latest, keyboard/composer changes,
tool disclosures, model picking, sending, question entry, new-session creation,
and device rotation. Named screenshot
attachments are retained in the `.xcresult` bundle for visual review.

Validated on iPhone 16e and iPhone 17 Pro (iOS 26.3.1 Simulator): 67 unit/hosted
layout tests and six UI scenarios passed on both. The final landscape refinements
were also rerun separately on iPhone 16e. A Release simulator build passed.

All interaction data is the offline demo dataset. This pass does not claim
physical-device performance, live host/edge delivery, or attachment transfer
coverage beyond the existing protocol tests.

## Reviewed screenshots

Simulator captures use the offline demo fixtures.

| Project selector after dismissal | Tool activity | Multiline composer |
| --- | --- | --- |
| ![Intact glass selector](screenshots/mobile-polish/home-glass.png) | ![Desktop-style tool activity](screenshots/mobile-polish/tool-activity.png) | ![Tail above keyboard and composer](screenshots/mobile-polish/multiline-keyboard.png) |

Additional captures: [project menu](screenshots/mobile-polish/project-menu.png),
[send runway](screenshots/mobile-polish/send-runway.png), and
[warm 600-turn reopen](screenshots/mobile-polish/warm-600-turns.png), and
[landscape keyboard](screenshots/mobile-polish/landscape-keyboard.png).
