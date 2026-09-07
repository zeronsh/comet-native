// Foreground/window validation for the native resource replay. Fails closed
// on a locked display so suspended rendering cannot look like a CPU win.
import Cocoa
import ApplicationServices

func fail(_ message: String) -> Never {
    fputs("\(message)\n", stderr)
    exit(1)
}

let session = CGSessionCopyCurrentDictionary() as? [String: Any] ?? [:]
if session["CGSSessionScreenIsLocked"] as? Bool == true {
    fail("Unlock the Mac before profiling native window rendering")
}
if CGDisplayIsAsleep(CGMainDisplayID()) != 0 {
    fail("Wake the display before profiling native window rendering")
}
if CommandLine.arguments.count == 1 { exit(0) }
guard let pid = Int32(CommandLine.arguments[1]),
      let app = NSRunningApplication(processIdentifier: pid) else { fail("Missing app process") }
if CommandLine.arguments.dropFirst(2).first == "--submit" {
    guard CommandLine.arguments.count == 4,
          let prompt = try? String(contentsOfFile: CommandLine.arguments[3], encoding: .utf8),
          AXIsProcessTrusted(), app.isActive else {
        fail("Composer submission requires a prompt file and the focused profiled app")
    }
    // Target this process explicitly and retain the foreground guard for
    // every event. Unicode key events leave the user's clipboard untouched.
    for character in prompt {
        guard app.isActive else { fail("App lost focus during composer submission") }
        let units = Array(String(character).utf16)
        guard let down = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true),
              let up = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: false) else {
            fail("Could not create composer input event")
        }
        down.flags = []
        up.flags = []
        down.keyboardSetUnicodeString(stringLength: units.count, unicodeString: units)
        up.keyboardSetUnicodeString(stringLength: units.count, unicodeString: units)
        down.postToPid(pid)
        up.postToPid(pid)
        Thread.sleep(forTimeInterval: 0.015)
    }
    guard app.isActive else { fail("App lost focus before submitting") }
    Thread.sleep(forTimeInterval: 0.05)
    for pressed in [true, false] {
        guard let event = CGEvent(keyboardEventSource: nil, virtualKey: 36, keyDown: pressed) else {
            fail("Could not create submit event")
        }
        event.flags = []
        event.postToPid(pid)
    }
    exit(0)
}
if CommandLine.arguments.dropFirst(2).first == "--check" {
    guard app.isActive else { fail("Profiled app is no longer foreground; discard this run") }
    let rows = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    guard rows.contains(where: { ($0[kCGWindowOwnerPID as String] as? Int32) == pid
        && ($0[kCGWindowLayer as String] as? Int) == 0
        && ($0[kCGWindowAlpha as String] as? Double ?? 0) > 0 }) else {
        fail("Profiled window is not onscreen; discard this run")
    }
    exit(0)
}
guard AXIsProcessTrusted() else { fail("Window profiling requires existing Accessibility access") }
app.activate(options: [.activateAllWindows])
let element = AXUIElementCreateApplication(pid)
var value: CFTypeRef?
guard AXUIElementCopyAttributeValue(element, kAXWindowsAttribute as CFString, &value) == .success,
      let windows = value as? [AXUIElement], windows.count == 1 else { fail("Expected one app window") }
let window = windows[0]
var size = CGSize(width: 1320, height: 880)
guard let sizeValue = AXValueCreate(.cgSize, &size),
      AXUIElementSetAttributeValue(window, kAXSizeAttribute as CFString, sizeValue) == .success,
      AXUIElementPerformAction(window, kAXRaiseAction as CFString) == .success else {
    fail("Could not size and raise the profiled window")
}
