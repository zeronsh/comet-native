# macOS display freeze investigation

Reported symptom: after the app has been open for a while, its contents stop
updating. The user can still send a message and does not see a beachball;
restarting the app restores rendering. Sleep/wake is a suspected trigger.

## Identified failure

The pinned macOS backend (`8a32c0e`) queues a display-link restart only when
`frame_requested` changes from false to true. `WindowFrameSource::start` set
that flag before its fallible subscription; stopping a source did not clear
it. A failed restart therefore left no subscription but a true request flag.
Later input and streaming updates changed application state while their
invalidations could never queue another restart.

The backend also only registered for system wake when the application supplied
an optional callback. Comet supplies none, and window frame sources were never
explicitly reconnected on wake. CoreVideo can stop across session changes
without the expected window screen/occlusion notifications. This is consistent
with the report, but the original incident has not been reproduced on macOS
hardware or confirmed by a process sample.

The fix is in [zeronsh/zui#3](https://github.com/zeronsh/zui/pull/3), pinned by
this PR. It clears the request flag on every stop, including the no-screen
path before a source exists, and sets it only after a successful subscription.
CoreVideo start/stop errors are accepted only when its actual running state
already matches the requested state. System wake, screen wake, and session
activation reconnect the window sources outside AppKit callbacks and locks.
All subscribers stop before any restart, so multiple windows sharing one
display really restart its shared link.

Idle parking, vsync pacing, the immortal display-link lifetime, and registry
lock ordering remain in place. This adds no polling timer or continuous
redrawing. The change is confined to the macOS backend; it does not alter
transcript scrolling or runway geometry.

## Validation

The PR's macOS CI job compiles the application and runs the native
`frame_source_recovery` integration test in the pinned dependency. Its custom
harness runs on the native main thread, creates a real dispatch source, and
checks that stopping it clears the latch. The test executable overrides the
imported `CVDisplayLinkStart` symbol to force two start errors while using real
CoreVideo links and dispatch sources. It verifies that each failure allows the
next invalidation to request a restart, and that the retry calls CoreVideo
again. The old implementation leaves the latch set. A null display ID was
initially tried as a fault injector, but CoreVideo accepted it on the CI runner.

The existing Linux UI suite covers the transcript changes. Its recordings are
linked in [transcript-selection-regressions.md](transcript-selection-regressions.md);
they are not evidence of macOS sleep/wake recovery.

Hardware follow-up: leave an idle window and an actively streaming window open,
sleep and wake the Mac repeatedly, then repeat with display sleep and session
switching. In each case check new composer text, message submission, live output,
selection, and scrolling. Repeat with two windows on the same display and with
an external monitor removed/reconnected. Verify that idle CPU returns to its
previous level after each cycle.

If the original freeze persists, capture Activity Monitor's **Sample Process**
for the app before restarting, together with its rotating app log. This will
distinguish another frame-source failure from a blocked main thread.

## References

- [Chromium's CVDisplayLink start/stop handling](https://chromium.googlesource.com/chromium/src/+/refs/heads/main/ui/display/mac/cv_display_link_mac.mm)
  checks actual running state when CoreVideo returns an error.
- [Mozilla's display-link freeze investigation](https://bugzilla.mozilla.org/show_bug.cgi?id=1422855#c97)
  documents input processing with stalled display updates across user switches.
- Apple documents [screen wake](https://developer.apple.com/documentation/appkit/nsworkspace/screensdidwakenotification)
  and [session activation](https://developer.apple.com/documentation/appkit/nsworkspace/sessiondidbecomeactivenotification)
  notifications through the workspace notification center.
