//! UI-only replay with native CoreText/Metal and real animation clocks.
//! Input is frames.json from scripts/resource-profile.mjs. An offscreen target
//! replaces the window compositor; this is not a whole-app CPU measurement.
use gpui::{AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use zeron_ui::*;

#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(serde::Deserialize)]
struct Frame {
    at: u64,
    frame: zeron_doc::TranscriptFrame,
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("This profiler requires macOS")
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 3,
        "Usage: macos-resource-profile FRAMES_JSON OUTPUT_DIR"
    );
    let frames: Vec<Frame> = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    anyhow::ensure!(!frames.is_empty(), "Empty replay");
    let output = std::path::PathBuf::from(&args[2]);
    std::fs::create_dir_all(&output)?;
    let native = gpui_platform::current_platform(true);
    let platform = gpui::bench_platform(
        Some(Box::new(gpui_platform::current_headless_renderer)),
        native.text_system(),
    );
    let executor = platform.background_executor();
    let handles = Rc::new(RefCell::new(None));
    let captured = handles.clone();
    let data = output.clone();
    let app = gpui::Application::with_platform(platform).with_assets(icons::Assets)
        .run_embedded(move |cx| {
            gpui_tokio::init(cx);
            let settings = settings::UiSettings::default();
            settings::init(settings.clone(), data.clone(), cx);
            let fonts = typography::register_fonts(cx);
            typography::init(settings.ui_font_family.clone(), settings.ui_font_size, fonts, cx);
            theme_library::init(data.clone(), cx);
            appearance::init(appearance::AppearanceMode::Dark, settings.theme_selection,
                settings.accent, settings.surface, cx);
            composer::init(cx);
            terminal::panel::init(cx);
            app_menus::init(cx);
            let state = cx.new(|_| {
                let mut state = state::AppState::new();
                state.connection = zeron_proto::view::ConnectionStatus::Ready;
                state.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
                state.selected_chat = Some("profile".into());
                state.selected_space = Some("project".into());
                state.auto_selected = true;
                state.chats_synced = true;
                state.spaces_synced = true;
                state.spaces = vec![serde_json::from_value(serde_json::json!({
                    "id":"project", "deviceId":"local", "path":"/tmp/resource-profile",
                    "createdAt":"2026-09-05T00:00:00Z"
                })).unwrap()];
                state.chats = vec![serde_json::from_value(serde_json::json!({
                    "id":"profile", "deviceId":"local", "spaceId":"project", "title":"Resource profile",
                    "archived":false, "createdAt":"2026-09-05T00:00:00Z",
                    "config":{"harness":"claude-code", "model":"claude-haiku-4-5", "reasoning":null, "sandbox":"workspace-write"}
                })).unwrap()];
                let background_chats = std::env::var("ZERON_PROFILE_BACKGROUND_CHATS")
                    .ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                for index in 0..background_chats {
                    let mut chat = state.chats[0].clone();
                    chat.id = format!("background-{index}");
                    chat.title = Some(format!("Background conversation {}", index + 1));
                    state.chats.push(chat);
                }
                if std::env::var_os("ZERON_VERIFY_SIDEBAR_ROWS").is_some() {
                    state.chats[0].title = Some("Short".into());
                    state.chats[0].branch = Some("main".into());
                    for (index, chat) in state.chats.iter_mut().skip(1).enumerate() {
                        chat.title = Some(if index == 0 { "Brief".into() } else {
                            "A deliberately long conversation title that must truncate inside the sidebar".into()
                        });
                        chat.branch = Some(if index == 0 { "main".into() } else {
                            "feature/a-deliberately-long-branch-name-that-must-also-truncate".into()
                        });
                    }
                    for chat in &mut state.chats {
                        chat.source_context = Some(zeron_proto::ConversationSourceContext {
                            checkout_id: "layout-checkout".into(), repo_root: "/tmp/resource-profile".into(),
                            cwd: "/tmp/resource-profile".into(), branch: chat.branch.clone().unwrap(),
                            head_sha: None, observed_at: chrono::Utc::now(),
                        });
                    }
                }
                state
            });
            let boot = EngineBootConfig { data_dir:data, ipc_port:0,
                edge_url:String::new(), edge_token:None, org_id:None,
                workos_client_id:None, default_harness:HarnessId::ClaudeCode };
            let window = cx.open_window(WindowOptions {
                window_bounds:Some(WindowBounds::Windowed(Bounds::new(
                    gpui::point(px(0.),px(0.)),size(px(1320.),px(880.))))),
                ..Default::default()
            }, |_,cx| cx.new(|cx| shell::Shell::new(state.clone(),boot,cx))).unwrap();
            *captured.borrow_mut() = Some((state,window));
        });
    let (state, window) = handles.borrow_mut().take().unwrap();
    let notifications = Rc::new(std::cell::Cell::new(0usize));
    let observed = notifications.clone();
    let _notification_probe = app.update(|cx| cx.observe(&state, move |_, _| {
        observed.set(observed.get() + 1);
    }));
    let dispatcher = executor.dispatcher().as_bench().unwrap();
    let start = Instant::now();
    let first_at = frames[0].at;
    let mut frames = frames.into_iter().peekable();
    let mut completed_at = None;
    eprintln!("pid={} phase=replay", std::process::id());
    loop {
        objc::rc::autoreleasepool(|| {
            let elapsed = start.elapsed().as_millis() as u64;
            while frames.peek().is_some_and(|f| f.at - first_at <= elapsed) {
                let frame = frames.next().unwrap();
                let text_only = matches!(&frame.frame, zeron_doc::TranscriptFrame::Delta {
                    upsert, append, remove, ..
                } if upsert.is_empty() && remove.is_empty() && !append.is_empty());
                let before = notifications.get();
                app.update(|cx| {
                    state.update(cx, |state, cx| {
                        state.receive_transcript_frame(frame.frame, cx).unwrap();
                        let streaming = state
                            .transcript
                            .iter()
                            .any(|entry| entry.status == Some(zeron_doc::MessageStatus::Streaming));
                        state.apply_sessions(vec![zeron_proto::Session {
                            chat_id: "profile".into(),
                            device_id: "local".into(),
                            status: if streaming {
                                zeron_proto::SessionStatus::Working
                            } else {
                                zeron_proto::SessionStatus::Idle
                            },
                            started_at: Some(chrono::Utc::now()),
                            updated_at: chrono::Utc::now(),
                        }]);
                    })
                });
                if text_only {
                    assert_eq!(notifications.get(), before,
                        "text growth must not notify unrelated app-state observers");
                }
            }
            app.update(|cx| {
                window
                    .update(cx, |_, window, cx| {
                        window.simulate_next_frame(cx);
                    })
                    .unwrap()
            });
            dispatcher.run_until_idle();
            app.update(|cx| {
                window
                    .update(cx, |_, window, _| window.present_if_needed())
                    .unwrap()
            });
        });
        if frames.peek().is_none() && completed_at.is_none() {
            eprintln!("phase=settled elapsed={:?}", start.elapsed());
            completed_at = Some(Instant::now());
            app.update(|cx| {
                window
                    .update(cx, |_, window, _| {
                        window
                            .render_to_image()
                            .unwrap()
                            .save(output.join("complete.png"))
                            .unwrap();
                    })
                    .unwrap()
            });
        }
        if completed_at.is_some_and(|t| t.elapsed() > Duration::from_secs(15)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    // Opt-in correctness check: a reused transcript scene must match a fresh
    // layout after idle, panel transitions and scrolling. Keep this outside
    // measured phases; screenshot readback and forced refreshes add work.
    if std::env::var_os("ZERON_VERIFY_CACHE").is_some() {
        let mut scenarios = vec!["settled", "sidebar-hidden", "sidebar-restored", "scrolled"];
        if std::env::var_os("ZERON_VERIFY_SIDEBAR_ROWS").is_some() {
            scenarios.extend(["sidebar-hover-short", "sidebar-hover-long"]);
        }
        // These hit coordinates target the bundled 80-section fixture. General
        // frame replays can still use the cache checks above on their own.
        if std::env::var_os("ZERON_VERIFY_INTERACTIONS").is_some() {
            scenarios.extend(["selected", "typed", "model-menu", "menu-dismissed"]);
        }
        for scenario in scenarios {
            app.update(|cx| {
                cx.update_window(window.into(), |_, window, cx| match scenario {
                        "sidebar-hidden" | "sidebar-restored" => {
                            window.dispatch_action(Box::new(shell::ToggleSidebar), cx)
                        }
                        "scrolled" => {
                            window.dispatch_event(
                                gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                    position: gpui::point(px(700.), px(400.)),
                                    delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(500.))),
                                    modifiers: gpui::Modifiers::default(),
                                    touch_phase: gpui::TouchPhase::Moved,
                                }),
                                cx,
                            );
                        }
                        "selected" => {
                            window.dispatch_event(gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                                position: gpui::point(px(430.), px(283.)),
                                click_count: 1,
                                ..Default::default()
                            }), cx);
                            window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                position: gpui::point(px(580.), px(349.)),
                                pressed_button: Some(gpui::MouseButton::Left),
                                ..Default::default()
                            }), cx);
                            window.dispatch_event(gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                                position: gpui::point(px(580.), px(349.)),
                                click_count: 1,
                                ..Default::default()
                            }), cx);
                            assert!(markdown::selection::selected_text().is_some_and(|text| text.len() > 20),
                                "The fixed 80-section fixture must support dragging across text after scene reuse");
                        }
                        "sidebar-hover-short" | "sidebar-hover-long" => {
                            window.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                position: gpui::point(px(100.), px(if scenario == "sidebar-hover-short" { 115. } else { 180. })),
                                ..Default::default()
                            }), cx);
                        }
                        "typed" => {
                            click(window, cx, 550., 839.);
                            for key in ["c", "a", "c", "h", "e", "space", "c", "h", "e", "c", "k"] {
                                assert!(window.dispatch_keystroke(gpui::Keystroke::parse(key).unwrap(), cx));
                            }
                        }
                        "model-menu" => click(window, cx, 1020., 839.),
                        "menu-dismissed" => click(window, cx, 1250., 500.),
                        _ => {}
                    })
                    .unwrap()
            });
            let settle = Instant::now();
            while settle.elapsed() < Duration::from_secs(1) {
                app.update(|cx| {
                    window
                        .update(cx, |_, window, cx| {
                            window.simulate_next_frame(cx);
                        })
                        .unwrap()
                });
                dispatcher.run_until_idle();
                std::thread::sleep(Duration::from_millis(8));
            }
            let cached = app.update(|cx| {
                window
                    .update(cx, |_, window, _| window.render_to_image().unwrap())
                    .unwrap()
            });
            app.update(|cx| window.update(cx, |_, window, _| window.refresh()).unwrap());
            dispatcher.run_until_idle();
            let fresh = app.update(|cx| {
                window
                    .update(cx, |_, window, _| window.render_to_image().unwrap())
                    .unwrap()
            });
            cached.save(output.join(format!("{scenario}-cached.png")))?;
            fresh.save(output.join(format!("{scenario}-fresh.png")))?;
            if std::env::var_os("ZERON_VERIFY_SIDEBAR_ROWS").is_some()
                && scenario != "sidebar-hidden"
            {
                // This fixture uses a 256-point sidebar at 2x scale. Check
                // actual geometry, not just equality with another render of
                // the same potentially broken layout. Text must leave the
                // right padding clear and selected/hovered fills must span it.
                let mut painted = 0;
                for y in 180..800 {
                    let left = cached.get_pixel(20, y).0;
                    if left[0] >= 30 && left[0] <= 60
                        && left[0] == left[1] && left[1] == left[2]
                    {
                        anyhow::ensure!(cached.get_pixel(491, y).0 == left,
                            "Sidebar row does not fill its width at y={y}: {scenario}");
                        painted += 1;
                    }
                }
                anyhow::ensure!(painted > 40, "Selected sidebar row missing: {scenario}");
                eprintln!("sidebar row widths match: {scenario}");
            }
            if scenario == "model-menu" {
                // With no engine catalog the menu animates its loading bars.
                // Their phase advances between readbacks; compare the rest of
                // the window, and retain both full images for visual review.
                for (x, y, pixel) in cached.enumerate_pixels() {
                    let in_menu = (1550..2170).contains(&x) && (1190..1640).contains(&y);
                    anyhow::ensure!(
                        in_menu || pixel == fresh.get_pixel(x, y),
                        "Scene outside the animated model menu differs at {x},{y}"
                    );
                }
                eprintln!("cache pixels match outside animated model menu");
            } else {
                anyhow::ensure!(
                    cached == fresh,
                    "Cached scene differs from fresh layout: {scenario}"
                );
                eprintln!("cache pixels match: {scenario}");
            }
        }
    }
    eprintln!("elapsed={:?}", start.elapsed());
    drop(state);
    app.update(|cx| cx.quit());
    drop(app);
    Ok(())
}

#[cfg(target_os = "macos")]
fn click(window: &mut gpui::Window, cx: &mut gpui::App, x: f32, y: f32) {
    let position = gpui::point(px(x), px(y));
    window.dispatch_event(
        gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
    window.dispatch_event(
        gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
            position,
            click_count: 1,
            ..Default::default()
        }),
        cx,
    );
}
