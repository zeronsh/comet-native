//! Real shell + native WebKit smoke test and screenshot fixture. Synthetic
//! chat data, isolated temp storage, loopback-only website, no engine services.
use gpui::{AppContext, AsyncApp, Bounds, WindowBounds, WindowOptions, px, size};
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};
use zeron_ui::*;

async fn pause(cx: &mut AsyncApp, ms: u64) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

fn capture(directory: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let path = directory.join(format!("{name}.png"));
    #[cfg(target_os = "macos")]
    let status = {
        let app = objc2_app_kit::NSApplication::sharedApplication(
            objc2::MainThreadMarker::new().unwrap(),
        );
        let window = app
            .keyWindow()
            .or_else(|| app.mainWindow())
            .ok_or_else(|| anyhow::anyhow!("fixture window is not available"))?;
        std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-o", "-l", &window.windowNumber().to_string()])
            .arg(&path)
            .status()?
    };
    #[cfg(not(target_os = "macos"))]
    let status = {
        let windows = std::process::Command::new("xdotool")
            .args([
                "search",
                "--onlyvisible",
                "--pid",
                &std::process::id().to_string(),
            ])
            .output()?;
        let id = String::from_utf8(windows.stdout)?
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("fixture window not visible"))?
            .to_owned();
        std::process::Command::new("import")
            .args(["-window", &id])
            .arg(&path)
            .status()?
    };
    anyhow::ensure!(status.success(), "screenshot capture failed");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let output = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/tmp/zeron-browser-captures".into()),
    );
    std::fs::create_dir_all(&output)?;
    let temp = tempfile::tempdir()?;
    let data = temp.path().to_path_buf();
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let _origin = format!("http://{}", listener.local_addr()?);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut request = [0; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            let (title, html) = if request.starts_with("GET /two ") {
                (
                    "Details",
                    "<a href='/'>Back to overview</a><h1>A closer look.</h1><p>Independent navigation, right beside your work.</p>",
                )
            } else {
                (
                    "Fieldnotes",
                    "<div class='eyebrow'>FIELDNOTES / WORKSPACE</div><h1>Make room<br>for good work.</h1><p>A quieter place to collect ideas, follow your progress, and build something that matters.</p><a class='button' id='details' href='/two'>Explore the workspace →</a><div class='cards'><article><small>01 / COLLECT</small><h2>Keep the good ideas.</h2><p>One place for the things you want to come back to.</p></article><article><small>02 / CREATE</small><h2>Find your next step.</h2><p>Small, thoughtful progress. Every single day.</p></article></div>",
                )
            };
            let body = format!(
                "<!doctype html><meta charset=utf-8><meta name='viewport' content='width=device-width'><title>{title}</title><style>body{{margin:0;padding:42px 32px;background:#f5f2eb;color:#263d35;font:15px/1.6 system-ui}}.eyebrow,small{{font-size:10px;letter-spacing:2px;color:#6d7c70}}h1{{font:500 45px/1.1 Georgia;margin:30px 0 20px}}p{{color:#6d776f;max-width:350px}}a{{color:inherit}}.button{{display:inline-block;margin:14px 0 30px;padding:10px 17px;background:#29483b;color:#fff;border-radius:7px;text-decoration:none;font-size:12px}}.cards{{display:grid;gap:14px}}article{{border:1px solid #d9ddd0;padding:20px;border-radius:10px}}h2{{font:500 21px Georgia;margin:12px 0}}article p{{font-size:12px;margin-bottom:0}}</style>{html}"
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    let failure = std::sync::Arc::new(std::sync::Mutex::new(None));
    let result = failure.clone();
    gpui_platform::application().with_assets(icons::Assets).run(move |cx| {
        gpui_tokio::init(cx); gpui_base::init(cx);
        let settings = settings::UiSettings::default();
        settings::init(settings.clone(), data.clone(), cx);
        let fonts = typography::register_fonts(cx);
        typography::init(settings.ui_font_family.clone(), settings.ui_font_size, fonts, cx);
        theme_library::init(data.clone(), cx);
        appearance::init(appearance::AppearanceMode::Dark, settings.theme_selection, settings.accent, settings.surface, cx);
        history::init(settings.git_history_columns, settings.git_history_column_widths,
            settings.git_history_column_order, settings.git_history_author_display, cx);
        composer::init(cx); terminal::panel::init(cx); app_menus::init(cx);
        let state = cx.new(|_| {
            let mut s = state::AppState::new();
            s.connection = zeron_proto::view::ConnectionStatus::Ready;
            s.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
            s.local_device_id = Some("local".into());
            s.devices = vec![serde_json::from_value(serde_json::json!({"id":"local","name":"This device","platform":std::env::consts::OS,"lastSeenAt":null})).unwrap()];
            s.selected_chat = Some("browser-fixture".into()); s.selected_space = Some("project".into());
            s.auto_selected = true; s.chats_synced = true; s.spaces_synced = true;
            s.spaces = vec![serde_json::from_value(serde_json::json!({"id":"project","deviceId":"local","path":"/tmp/fieldnotes","createdAt":"2026-09-08T00:00:00Z"})).unwrap()];
            s.chats = vec![serde_json::from_value(serde_json::json!({"id":"browser-fixture","deviceId":"local","spaceId":"project","title":"Build the Fieldnotes workspace","archived":false,"createdAt":"2026-09-08T00:00:00Z","config":{"harness":"claude-code","model":"claude-sonnet-4-6","reasoning":null,"sandbox":"workspace-write"}})).unwrap()];
            s
        });
        let boot = EngineBootConfig { data_dir: data, ipc_port: 0, edge_url: String::new(), edge_token: None, org_id: None, workos_client_id: None, default_harness: HarnessId::ClaudeCode };
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(gpui::point(px(20.),px(30.)), size(px(1320.),px(880.))))),
            titlebar: Some(gpui::TitlebarOptions { title: None, appears_transparent: true, traffic_light_position: Some(gpui::point(px(14.),px(14.))) }),
            app_owns_titlebar_drag: true,
            ..Default::default()
        }, |_, cx| cx.new(|cx| shell::Shell::new(state.clone(), boot, cx))).unwrap();
        state.update(cx, |_, cx| cx.notify());
        cx.activate(true);
        cx.spawn(async move |cx| {
            let run: anyhow::Result<()> = async {
                pause(cx, 1200).await;
                state.update(cx, |s, cx| {
                    let entries = serde_json::from_value(serde_json::json!([
                        {"id":"fixture-user","role":"user","parts":[{"id":"text","kind":"text","text":"Build a calm, thoughtful workspace for Fieldnotes. Let’s preview the landing page beside this conversation."}],"createdAt":1788900000000_i64,"deviceId":"local"},
                        {"id":"fixture-assistant","role":"assistant","parts":[{"id":"text","kind":"text","text":"The first layout is ready to review.\n\nIt uses warm neutrals, generous spacing, and a simple hierarchy. The workspace cards stay readable as the preview gets narrower.\n\nOpen **Browser** from the sidebar’s **+** menu to keep the page beside your work."}],"createdAt":1788900001000_i64,"deviceId":"local","status":"complete"}
                    ])).unwrap();
                    s.receive_transcript_frame(zeron_doc::TranscriptFrame::Reset { reset: entries }, cx).unwrap();
                });
                let (first_id, first) = window.update(cx, |shell, w, cx| shell.fixture_open_browser(None, w, cx))?;
                pause(cx, 500).await;
                capture(&output, "browser-empty-dark")?;
                #[cfg(target_os = "macos")]
                {
                    window.update(cx, |_, w, cx| first.update(cx, |b, cx| b.navigate(&_origin, w, cx)))?;
                    let deadline = std::time::Instant::now() + Duration::from_secs(25);
                    while !first.read_with(cx, |b, _| b.page.title == "Fieldnotes" && !b.page.loading && b.fixture_native_visible()) {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "first page did not load: {:?}", first.read_with(cx, |b, _| b.page.clone()));
                        pause(cx, 50).await;
                    }
                    pause(cx, 500).await;
                    capture(&output, "browser-preview-dark")?;
                    // Real DOM click, native navigation and history.
                    first.read_with(cx, |b, _| b.fixture_eval("document.getElementById('details').click()"));
                    let deadline = std::time::Instant::now() + Duration::from_secs(15);
                    while !first.read_with(cx, |b, _| b.page.title == "Details" && b.page.can_back) {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "DOM link navigation failed"); pause(cx, 50).await;
                    }
                    first.update(cx, |b, _| b.fixture_history(false));
                    let deadline = std::time::Instant::now() + Duration::from_secs(15);
                    while !first.read_with(cx, |b, _| b.page.title == "Fieldnotes" && b.page.can_forward) {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "back/forward history failed"); pause(cx, 50).await;
                    }
                    first.read_with(cx, |b, _| b.fixture_eval("history.pushState({}, '', '/same-document'); document.title = 'Updated title'"));
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while !first.read_with(cx, |b, _| b.page.title == "Updated title" && b.page.url.as_deref().is_some_and(|u| u.ends_with("/same-document"))) {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "same-document state did not update"); pause(cx, 50).await;
                    }
                }
                #[cfg(target_os = "macos")]
                let mut recording;
                #[cfg(target_os = "macos")]
                {
                    pause(cx, 400).await;
                    let (left, top) = first.read_with(cx, |b, _| b.fixture_origin());
                    first.read_with(cx, |b, _| b.fixture_focus());
                    pause(cx, 100).await;
                    first.read_with(cx, |b,_| b.fixture_eval("(() => { let live = document.createElement('div'); live.style='position:fixed;bottom:16px;right:16px;padding:8px 12px;background:#29483b;color:white;border-radius:8px;font:12px monospace;z-index:99999'; document.body.append(live); let start=performance.now(); function frame() { live.textContent='LIVE  '+((performance.now()-start)/1000).toFixed(2)+'s'; requestAnimationFrame(frame); } frame(); })()"));
                    recording = std::process::Command::new("/usr/sbin/screencapture").args(["-v","-V","24","-C","-k","-D","1"]).arg(output.join("browser-hover.mov")).spawn()?;
                    pause(cx, 1000).await;
                    let before = first.read_with(cx, |b, _| b.fixture_stats());
                    // Actual GPUI mouse dispatch exercises tab and toolbar hover
                    // listeners and tooltip timers, including rapid cancellation.
                    for _ in 0..12 {
                        for (x,y) in [(left + 35.,20.), (left - 20.,100.), (left + 78.,56.)] {
                            first.read_with(cx, |b,_| b.fixture_move_cursor(x as f64,y as f64));
                            window.update(cx, |_, w, cx| { w.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position:gpui::point(px(x),px(y)), pressed_button:None,modifiers:Default::default() }),cx); })?;
                            pause(cx, 80).await;
                            anyhow::ensure!(first.read_with(cx, |b,_| b.fixture_native_visible() && b.fixture_stats() == before), "hover hid or snapshotted the live webview");
                        }
                    }
                    // Dwell over Reload until its real GPUI tooltip paints above
                    // the page; the passive overlay must leave native input alone.
                    pause(cx, 1000).await;
                    anyhow::ensure!(first.read_with(cx, |b,_| b.fixture_overlay_visible()), "toolbar tooltip did not paint on the overlay plane");
                    anyhow::ensure!(first.read_with(cx, |b,_| b.fixture_focused() && !b.fixture_overlay_at((left+100.) as f64,(top+100.) as f64)), "tooltip stole native focus or page hit testing");
                    anyhow::ensure!(first.read_with(cx, |b,_| b.fixture_stats() == before && !b.fixture_snapshot()), "tooltip captured or hid the page");
                    capture(&output, "browser-tooltip-dark")?;
                    first.read_with(cx, |b,_| b.fixture_move_cursor((left-20.) as f64,150.));
                    window.update(cx, |_,w,cx| { w.dispatch_event(gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {position:gpui::point(px(left-20.),px(150.)),pressed_button:None,modifiers:Default::default()}),cx); })?;
                    pause(cx, 400).await;
                    anyhow::ensure!(!first.read_with(cx, |b,_| b.fixture_overlay_visible()), "dismissed tooltip left stale overlay pixels");
                }
                let (second_id, second) = window.update(cx, |shell, w, cx| shell.fixture_open_browser(None, w, cx))?;
                pause(cx, 250).await;
                anyhow::ensure!(!first.read_with(cx, |b, _| b.fixture_native_visible()), "background page stayed visible");
                #[cfg(target_os = "macos")]
                {
                    first.read_with(cx, |b, _| b.fixture_eval("document.cookie = 'browserfixture=shared; path=/'"));
                    window.update(cx, |_, w, cx| second.update(cx, |b, cx| b.navigate(&_origin, w, cx)))?;
                    let deadline = std::time::Instant::now() + Duration::from_secs(15);
                    while !second.read_with(cx, |b, _| b.page.title == "Fieldnotes" && !b.page.loading) {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "second page did not load"); pause(cx, 50).await;
                    }
                    second.read_with(cx, |b, _| b.fixture_eval("document.title = document.cookie.includes('browserfixture=shared') ? 'Shared login' : 'Missing cookie'"));
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !second.read_with(cx, |b, _| b.page.title == "Shared login") {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "tabs did not share ephemeral website data"); pause(cx, 50).await;
                    }
                    anyhow::ensure!(first.read_with(cx, |b, _| b.page.title == "Updated title"), "second tab replaced first tab state");
                }
                window.update(cx, |shell, _, cx| shell.fixture_select_browser(first_id, cx))?;
                pause(cx, 250).await;
                window.update(cx, |shell, w, cx| shell.fixture_close_browser(second_id, w, cx))?;
                drop(second);
                window.update(cx, |shell, _, cx| shell.fixture_browser_menu(true, cx))?;
                pause(cx, 700).await;
                #[cfg(target_os = "macos")]
                {
                    let (left, top) = first.read_with(cx, |b,_| b.fixture_origin());
                    anyhow::ensure!(first.read_with(cx, |b,_| b.fixture_native_visible() && !b.fixture_snapshot()), "menu froze or hid the live page");
                    anyhow::ensure!(first.read_with(cx, |b,_| b.fixture_overlay_at((left+100.) as f64,(top+150.) as f64)), "menu failed to intercept outside clicks above the browser");
                    capture(&output, "browser-menu-dark")?;
                    first.read_with(cx, |b,_| b.fixture_eval("document.addEventListener('click', () => { document.title = 'Unexpected page click'; })"));
                    pause(cx, 100).await;
                    first.read_with(cx, |b,_| b.fixture_click((left+100.) as f64,(top+150.) as f64));
                    pause(cx, 600).await;
                    anyhow::ensure!(first.read_with(cx, |b,_| !b.fixture_overlay_at((left+100.) as f64,(top+150.) as f64) && b.page.title != "Unexpected page click"), "outside click did not dismiss the menu cleanly, or leaked into the page");
                    // The next native click must reach the page now that the
                    // interactive overlay is gone.
                    first.read_with(cx, |b,_| b.fixture_click((left+100.) as f64,(top+150.) as f64));
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !first.read_with(cx, |b,_| b.page.title == "Unexpected page click") {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "page mouse input was not restored after menu dismissal"); pause(cx,50).await;
                    }
                    first.read_with(cx, |b,_| b.fixture_eval("document.title = 'Fieldnotes'"));
                }
                #[cfg(not(target_os = "macos"))]
                {
                    capture(&output, "browser-menu-dark")?;
                    window.update(cx, |shell, _, cx| shell.fixture_browser_menu(false, cx))?;
                }
                pause(cx, 500).await;
                #[cfg(target_os = "macos")]
                {
                    // Finish the continuous screen recording while the page is
                    // still live. This is not a montage of captured screenshots.
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    loop {
                        if let Some(status) = recording.try_wait()? { anyhow::ensure!(status.success(), "native screen recording failed"); break; }
                        anyhow::ensure!(std::time::Instant::now() < deadline, "screen recording did not finish");
                        pause(cx, 100).await;
                    }
                    anyhow::ensure!(std::fs::metadata(output.join("browser-hover.mov"))?.len() > 10000, "screen recording is empty");
                }
                for width in [380., 640., 520.] {
                    window.update(cx, |shell, _, cx| shell.fixture_resize_browser(width, cx))?;
                    pause(cx, 150).await;
                }
                window.update(cx, |shell, _, cx| shell.fixture_expand_browser(cx))?;
                pause(cx, 400).await;
                window.update(cx, |shell, _, cx| shell.fixture_expand_browser(cx))?;
                cx.update(|cx| appearance::set_mode(appearance::AppearanceMode::Light, cx));
                pause(cx, 600).await;
                capture(&output, "browser-light")?;
                #[cfg(target_os = "macos")]
                {
                    anyhow::ensure!(first.read_with(cx, |b, _| b.fixture_native_visible()), "page not restored after overlays/takeover");
                    // Use an ordinary closed localhost port. Port 1 is on
                    // WebKit's restricted-port list and does not exercise a
                    // failed connection to a development server.
                    let closed = std::net::TcpListener::bind("127.0.0.1:0")?;
                    let unavailable = format!("http://{}/unavailable", closed.local_addr()?);
                    drop(closed);
                    window.update(cx, |_, w, cx| first.update(cx, |b, cx| b.navigate(&unavailable, w, cx)))?;
                    let deadline = std::time::Instant::now() + Duration::from_secs(15);
                    while !first.read_with(cx, |b, _| b.page.error.is_some()) {
                        anyhow::ensure!(std::time::Instant::now() < deadline, "load failure was not reported: {:?}", first.read_with(cx, |b, _| b.page.clone())); pause(cx, 50).await;
                    }
                    pause(cx, 300).await; capture(&output, "browser-error-light")?;
                }
                // Reject arbitrary schemes while preserving the existing page.
                let before = first.read_with(cx, |b, _| b.page.url.clone());
                window.update(cx, |_, w, cx| first.update(cx, |b, cx| b.navigate("javascript:alert(1)", w, cx)))?;
                anyhow::ensure!(first.read_with(cx, |b, _| b.page.url == before), "unsupported address navigated");
                window.update(cx, |shell, w, cx| shell.fixture_close_browser(first_id, w, cx))?;
                pause(cx, 200).await;
                anyhow::ensure!(!first.read_with(cx, |b, _| b.fixture_native_visible()), "closed tab retained its native view");
                std::fs::write(output.join("result.txt"), "PASS: real shell browser fixture; address rejection, tab switching/close, overlays, resizing, takeover and appearance. On macOS: live DOM navigation, history, same-document state, native visibility, rapid hover/tooltip focus and hit testing, overlay outside-click isolation/restoration, and load failure.\n")?;
                Ok(())
            }.await;
            if let Err(error) = run { eprintln!("Browser fixture failed: {error:#}"); *result.lock().unwrap() = Some(error.to_string()); }
            let _ = window.update(cx, |shell, window, cx| shell.fixture_blur_browser(window, cx));
            pause(cx, 200).await;
            drop(state);
            let _ = window.update(cx, |_, window, _| window.remove_window());
            pause(cx, 100).await;
            cx.update(|cx| cx.quit());
        }).detach();
    });
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
