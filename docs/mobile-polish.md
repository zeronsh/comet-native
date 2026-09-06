# Mobile transcript and interaction polish

The transcript uses a native `UITableView` for row reuse, height resolution,
scroll gestures, and animated offsets, with `UIHostingConfiguration` for the
existing SwiftUI message content. The earlier mixed lazy/eager layout passed
settled checks but still lost visible rows during rapid streaming and keyboard
resizes. A regression sampling those transitions at 90ms intervals reproduced
an absent tail while the old layout reported a bottom distance below 1pt.

Follow intent changes only on a real user drag, accessibility page navigation,
an explicit send/jump, or a user-message expansion. Keyboard and composer
resizing do not release it. The composer occupies sibling layout space so the
last row rests above it, while the transcript can draw outside its viewport
under the glass composer and material header during scrolling. Compact
landscape uses a horizontal composer to preserve usable transcript space.

A local submission reserves one viewport beneath the prompt. The reply consumes
that space during layout; short completed replies keep it across warm session
navigation, including optimistic-message adoption. Long output naturally hands
over to following the tail. Remote user entries do not create a local runway.
Native scroll animation replaces delayed SwiftUI scroll-target retries; hosted
tests sample intermediate presentation offsets instead of only the end state.

Existing streaming text starts fully visible when its row is reattached. Only
newly appended text fades, and the frame ticker stops after that fade settles.
This prevents revisiting a streaming row from briefly painting it black.

Long user messages have a five-line preview and Show more / Show less, with the
desktop's 400-character / five-line thresholds and measurement for wrapped text.
Expansion is retained in the warm session store and preserves the reading anchor.
The full text remains available to Copy. Controls have 44pt touch targets.

Accessibility exposes realized cells through a separate viewport container.
UIKit's default table proxies otherwise instantiate thousands of hosted rows
when inspecting a large history. VoiceOver page scrolling and Earlier/Later
messages actions expose adjacent pages without building the entire transcript.

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
simulator window and measures physical tail-row and viewport frames. Debug probes
are enabled only by these tests and compile to no-ops in release builds.

Coverage includes warm and delayed 600-turn histories, burst appends, a large
shrink, repeated warm reopens, composer/keyboard viewport changes, narrow and
landscape sizes, accessibility text resizing and paging, runway retention/overflow,
immediate history scrolling, and streaming during rapid keyboard resizes.

`ZeronUITests/MobilePolishTests` exercises actual project selection/dismissal,
session navigation, scroll gestures, jump-to-latest, keyboard/composer changes,
tool disclosures, model picking, sending, question entry, new-session creation,
cancelled back swipes while streaming, user-message folding, and device rotation. Named screenshot
attachments are retained in the `.xcresult` bundle for visual review.

The follow-up passed 73 unit/hosted-layout tests and nine UI scenarios on both
iPhone 16e and iPhone 17 Pro (iOS 26.3.1 Simulator). The final disclosure-only
refinement was rerun on both devices. A Release simulator build passed. Video
frames from three partial back swipes were also reviewed while the reply streamed.
The original 67-unit/six-UI pass did not cover the rapid transitions that exposed
these regressions; the added checks specifically exercise those transitions.

All interaction data is the offline demo dataset. This pass does not claim
physical-device performance, live host/edge delivery, or attachment transfer
coverage beyond the existing protocol tests.

## Follow-up screenshots

| Five-line user preview | Expansion retained after reopening | Streaming with keyboard open |
| --- | --- | --- |
| ![Show more](screenshots/mobile-transcript-recovery/user-collapsed.png) | ![Show less after reopening](screenshots/mobile-transcript-recovery/user-expanded-reopen.png) | ![Visible streaming transcript](screenshots/mobile-transcript-recovery/stream-keyboard.png) |

Additional captures: [expanded text beneath the glass composer](screenshots/mobile-transcript-recovery/user-expanded.png),
[multiline composer](screenshots/mobile-transcript-recovery/multiline-composer.png),
[fast scrolling after reopen](screenshots/mobile-transcript-recovery/early-scroll.png), and
[cancelled back swipe](screenshots/mobile-transcript-recovery/cancelled-back-swipe.png), and
[a frame during a partial back swipe](screenshots/mobile-transcript-recovery/partial-back-swipe.png).

## Original visual pass captures

Simulator captures use the offline demo fixtures.

| Project selector after dismissal | Tool activity | Multiline composer |
| --- | --- | --- |
| ![Intact glass selector](screenshots/mobile-polish/home-glass.png) | ![Desktop-style tool activity](screenshots/mobile-polish/tool-activity.png) | ![Tail above keyboard and composer](screenshots/mobile-polish/multiline-keyboard.png) |

Additional captures: [project menu](screenshots/mobile-polish/project-menu.png),
[send runway](screenshots/mobile-polish/send-runway.png), and
[warm 600-turn reopen](screenshots/mobile-polish/warm-600-turns.png), and
[landscape keyboard](screenshots/mobile-polish/landscape-keyboard.png).
