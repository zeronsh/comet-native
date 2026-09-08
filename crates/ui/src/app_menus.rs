//! Native menu bar + app-level window actions (macOS-first).
//!
//! zeron never called `cx.set_menus`, so on macOS `NSApp.mainMenu` stayed nil:
//! no app menu, no ⌘Q quit, and nothing for the auto-hidden system menu bar to
//! reveal on hover (gpui only calls `setMainMenu_` from `set_menus` —
//! gpui_macos/src/platform.rs `fn set_menus`). Structure ported from zed's
//! `crates/zed/src/zed/app_menus.rs` and the gpui `set_menus.rs` example at the
//! pinned rev (f14fea9bf3c9).
//!
//! Wiring: [`init`] registers the global action handlers (run once at boot),
//! [`bind_keys`] installs the fixed application shortcuts (re-run by
//! `shell::apply_keymap`, which clears every binding first), and
//! [`app_menus`] builds the menu bar handed to `cx.set_menus` in `run_app`.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, Window, actions};

use crate::appearance::{self, AppearanceMode};
use crate::composer;
use crate::shell;

actions!(
    zeron,
    [
        About,
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        CloseWindow,
        AppearanceSystem,
        AppearanceLight,
        AppearanceDark,
    ]
);

/// Register the global handlers backing the menu bar and its shortcuts. Call
/// once at boot, before `cx.set_menus`.
pub fn init(cx: &mut App) {
    #[cfg(all(target_os = "macos", not(test)))]
    native_quit::init(cx);
    cx.on_action(quit);
    // Application-menu verbs — gpui wraps NSApp `hide` / `hideOtherApplications`
    // / `unhideAllApplications` (zed registers the same trio in
    // crates/zed/src/zed.rs `init`).
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    // Window verbs route to the active window. zeron is single-window, so a
    // global handler suffices where zed registers these per-workspace
    // (crates/zed/src/zed.rs `register_action(Minimize/Zoom)`).
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window| window.zoom_window()));
    cx.on_action(close_window);
    // Appearance. Each verb persists and repaints every window; see
    // `appearance::set_mode`.
    cx.on_action(|_: &AppearanceSystem, cx| appearance::set_mode(AppearanceMode::System, cx));
    cx.on_action(|_: &AppearanceLight, cx| appearance::set_mode(AppearanceMode::Light, cx));
    cx.on_action(|_: &AppearanceDark, cx| appearance::set_mode(AppearanceMode::Dark, cx));
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(window) = cx.active_window() {
        window.update(cx, |_, window, _| f(window)).ok();
    }
}

/// All app-owned quit paths must flush file buffers before GPUI destroys windows.
fn quit(_: &Quit, cx: &mut App) {
    request_quit(cx);
}

pub(crate) fn request_quit(cx: &mut App) {
    // Actions may arrive while GPUI has the active window borrowed. Inspect
    // roots only after that dispatch completes.
    cx.defer(prepare_quit);
}

fn prepare_quit(cx: &mut App) {
    let mut ready = true;
    for window in cx.windows() {
        if let Some(window) = window.downcast::<shell::Shell>() {
            ready &= window
                .update(cx, |shell, _, cx| shell.prepare_quit(cx))
                .unwrap_or(false);
        }
    }
    if ready {
        quit_after_save(cx);
    }
}

pub(crate) fn quit_after_save(cx: &mut App) {
    #[cfg(target_os = "macos")]
    native_quit::allow();
    cx.quit();
}

fn close_window(_: &CloseWindow, cx: &mut App) {
    cx.defer(close_active_window);
}

fn close_active_window(cx: &mut App) {
    if let Some(window) = cx.active_window() {
        if let Some(window) = window.downcast::<shell::Shell>() {
            window
                .update(cx, |shell, window, cx| {
                    if shell.prepare_window_close(cx) {
                        window.remove_window();
                    }
                })
                .ok();
        } else {
            window
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        }
    }
}

/// Fixed app-level shortcuts backing the menu key equivalents. These live
/// outside the customizable keymap; `shell::apply_keymap` calls this after its
/// `clear_key_bindings` so they survive keymap re-application. Settings follows
/// the platform convention everywhere (Cmd+, on macOS, Ctrl+, elsewhere);
/// window/application lifecycle shortcuts remain macOS-only.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(app_key_bindings(cfg!(target_os = "macos")));
}

/// The binding table behind [`bind_keys`] — `KeyBinding` construction is pure
/// (no `App`), so unit tests can inspect it directly.
fn app_key_bindings(macos: bool) -> Vec<KeyBinding> {
    let mut bindings = vec![KeyBinding::new(
        if macos { "cmd-," } else { "ctrl-," },
        shell::OpenSettings,
        None,
    )];
    if macos {
        bindings.extend([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("alt-cmd-h", HideOthers, None),
            KeyBinding::new("cmd-m", Minimize, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
        ]);
    }
    bindings
}

/// The zeron menu bar. macOS renders this natively; mac-only entries are gated
/// at runtime (`cfg!`) so the whole module compiles and tests on Linux.
pub fn app_menus() -> Vec<Menu> {
    let macos = cfg!(target_os = "macos");

    // macOS titles the first menu with the bundle/process name regardless of
    // what we pass, but gpui still wants a name.
    let mut app_items = vec![
        // Placeholder until a real about dialog exists (explicitly disabled).
        MenuItem::action("About Zeron", About).disabled(true),
        MenuItem::separator(),
        MenuItem::action("Settings", shell::OpenSettings),
        MenuItem::separator(),
    ];
    if macos {
        app_items.extend([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Zeron", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
        ]);
    }
    app_items.push(MenuItem::action("Quit Zeron", Quit));

    let mut menus = vec![
        Menu::new("Zeron").items(app_items),
        // Standard clipboard verbs tied to the composer's existing actions via
        // their native selectors (`OsAction` → cut:/copy:/paste:/selectAll:),
        // so the OS Edit menu routes through the responder chain to the focused
        // input — zed wires its editor actions identically
        // (crates/zed/src/zed/app_menus.rs, Edit/Selection menus).
        Menu::new("Edit").items([
            // Undo/Redo have no `OsAction` counterpart — they dispatch as plain
            // actions to the focused input, same as the composer keymap.
            MenuItem::action("Undo", composer::Undo),
            MenuItem::action("Redo", composer::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", composer::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", composer::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", composer::Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", composer::SelectAll, OsAction::SelectAll),
        ]),
    ];
    // Appearance lives under View on every platform — it is the only View verb
    // today, but "Appearance" as a top-level menu would read oddly next to Edit.
    menus.push(Menu::new("View").items([
        MenuItem::action("Appearance: System", AppearanceSystem),
        MenuItem::action("Appearance: Light", AppearanceLight),
        MenuItem::action("Appearance: Dark", AppearanceDark),
    ]));
    if macos {
        // Standard Window menu; macOS appends the open-window list itself.
        menus.push(Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]));
    }
    menus
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action as _, Keystroke};

    fn action_names(menu: &Menu) -> Vec<&'static str> {
        menu.items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.name()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn app_menu_ends_with_quit() {
        let menus = app_menus();
        assert_eq!(menus[0].name.as_ref(), "Zeron");
        let Some(MenuItem::Action { name, action, .. }) = menus[0].items.last() else {
            panic!("last app-menu item must be an action");
        };
        assert_eq!(name.as_ref(), "Quit Zeron");
        assert_eq!(action.name(), Quit.name());
    }

    #[test]
    fn app_menu_offers_settings() {
        let menus = app_menus();
        assert!(
            menus[0].items.iter().any(|item| matches!(
                item,
                MenuItem::Action { name, action, .. }
                    if name.as_ref() == "Settings" && action.name() == shell::OpenSettings.name()
            )),
            "the application menu should expose Settings"
        );
    }

    #[test]
    fn about_is_disabled_placeholder() {
        let menus = app_menus();
        let first = &menus[0].items[0];
        assert!(
            first.is_disabled(),
            "About stays disabled until implemented"
        );
    }

    #[test]
    fn edit_menu_uses_composer_clipboard_os_actions() {
        let menus = app_menus();
        let edit = menus
            .iter()
            .find(|m| m.name.as_ref() == "Edit")
            .expect("Edit menu present");
        // `OsAction` has no `Debug` impl at the pinned rev, so compare
        // per-field.
        let expect = [
            (composer::Cut.name(), OsAction::Cut),
            (composer::Copy.name(), OsAction::Copy),
            (composer::Paste.name(), OsAction::Paste),
            (composer::SelectAll.name(), OsAction::SelectAll),
        ];
        let got: Vec<(&str, OsAction)> = edit
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action {
                    action,
                    os_action: Some(os_action),
                    ..
                } => Some((action.name(), *os_action)),
                _ => None,
            })
            .collect();
        assert_eq!(got.len(), expect.len());
        for ((got_name, got_os), (want_name, want_os)) in got.iter().zip(expect.iter()) {
            assert_eq!(got_name, want_name);
            assert!(got_os == want_os, "OsAction mismatch for {want_name}");
        }
    }

    #[test]
    fn view_menu_offers_all_three_appearance_modes() {
        let menus = app_menus();
        let view = menus
            .iter()
            .find(|m| m.name.as_ref() == "View")
            .expect("View menu present");
        assert_eq!(
            action_names(view),
            vec![
                AppearanceSystem.name(),
                AppearanceLight.name(),
                AppearanceDark.name()
            ]
        );
    }

    #[test]
    fn app_bindings_use_platform_settings_convention() {
        // `KeyBinding::new` panics on unparseable combos, so constructing the
        // table is itself the parse check.
        let find = |bindings: &[KeyBinding], name: &str| {
            bindings
                .iter()
                .find(|binding| binding.action().name() == name)
                .map(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .map(|ks| ks.inner().clone())
                        .collect::<Vec<_>>()
                })
        };
        let combo = |source: &str| vec![Keystroke::parse(source).unwrap()];
        let macos = app_key_bindings(true);
        assert_eq!(
            find(&macos, shell::OpenSettings.name()),
            Some(combo("cmd-,"))
        );
        assert_eq!(find(&macos, Quit.name()), Some(combo("cmd-q")));
        assert_eq!(find(&macos, CloseWindow.name()), Some(combo("cmd-w")));
        assert_eq!(find(&macos, Minimize.name()), Some(combo("cmd-m")));

        let other = app_key_bindings(false);
        assert_eq!(
            find(&other, shell::OpenSettings.name()),
            Some(combo("ctrl-,"))
        );
        assert_eq!(find(&other, Quit.name()), None);
    }
}

// GPUI's on_app_quit runs after AppKit has committed to termination. Catch
// Dock / system Quit at the earlier delegate decision, just like our menu
// action, so failed saves can still cancel termination. The pinned GPUI
// delegate does not implement applicationShouldTerminate:.
#[cfg(target_os = "macos")]
mod native_quit {
    use super::*;
    use objc::runtime::{Class, Object, Sel, class_addMethod};
    use objc::{class, msg_send, sel, sel_impl};
    use std::cell::{Cell, RefCell};

    thread_local! {
        static ALLOWED: Cell<bool> = const { Cell::new(false) };
        static REQUEST: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    }

    extern "C" fn should_terminate(_: &Object, _: Sel, _: *mut Object) -> usize {
        if ALLOWED.get() {
            return 1; // NSTerminateNow
        }
        REQUEST.with_borrow(|request| {
            if let Some(request) = request {
                request();
            }
        });
        0 // NSTerminateCancel; re-request only after buffers are safe.
    }

    pub(super) fn allow() {
        ALLOWED.set(true);
    }

    pub(super) fn init(cx: &mut App) {
        let executor = cx.foreground_executor().clone();
        let app = cx.to_async();
        REQUEST.with_borrow_mut(|request| {
            *request = Some(Box::new(move || {
                let mut app = app.clone();
                executor
                    .spawn(async move {
                        let _ = app.update(request_quit);
                    })
                    .detach();
            }));
        });
        // AppKit invokes delegate methods on the main thread. Add only the
        // missing selector: never replace GPUI's existing quit observer.
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let delegate: *mut Object = msg_send![app, delegate];
            assert!(!delegate.is_null(), "GPUI application delegate must exist");
            let class = (*delegate).class() as *const Class as *mut Class;
            let added = class_addMethod(
                class,
                sel!(applicationShouldTerminate:),
                std::mem::transmute::<
                    extern "C" fn(&Object, Sel, *mut Object) -> usize,
                    unsafe extern "C" fn(),
                >(should_terminate),
                c"Q@:@".as_ptr(),
            );
            assert!(
                added == objc::runtime::YES,
                "GPUI termination delegate contract changed"
            );
        }
    }
}
