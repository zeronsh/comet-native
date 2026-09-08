//! Real shell + native WebKit smoke test and screenshot fixture. Synthetic
//! chat data, isolated temp storage, loopback-only website, no engine services.
use gpui::{AppContext, AsyncApp, Bounds, WindowBounds, WindowOptions, px, size};
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
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
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .arg("-x")
        .arg(&path)
        .status()?;
    #[cfg(not(target_os = "macos"))]
    let status = std::process::Command::new("import")
        .args(["-window", "root"])
        .arg(&path)
        .status()?;
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
    let origin = format!("http://{}", listener.local_addr()?);
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
                "<!doctype html><meta name='viewport' content='width=device-width'><title>{title}</title><style>body{{margin:0;padding:42px 32px;background:#f5f2eb;color:#263d35;font:15px/1.6 system-ui}}.eyebrow,small{{font-size:10px;letter-spacing:2px;color:#6d7c70}}h1{{font:500 45px/1.1 Georgia;margin:30px 0 20px}}p{{color:#6d776f;max-width:350px}}a{{color:inherit}}.button{{display:inline-block;margin:14px 0 30px;padding:10px 17px;background:#29483b;color:#fff;border-radius:7px;text-decoration:none;font-size:12px}}.cards{{display:grid;gap:14px}}article{{border:1px solid #d9ddd0;padding:20px;border-radius:10px}}h2{{font:500 21px Georgia;margin:12px 0}}article p{{font-size:12px;margin-bottom:0}}</style>{html}"
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
        composer::init(cx); terminal::panel::init(cx); app_menus::init(cx);
        let state = cx.new(|_| {
            let mut s = state::AppState::new();
            s.connection = zeron_proto::view::ConnectionStatus::Ready;
            s.workspace_scope = Some(zeron_proto::WorkspaceScope::Local);
            s.local_device_id = Some("local".into());
            s.selected_chat = Some("browser-fixture".into()); s.selected_space = Some("project".into());
            s.auto_selected = true; s.chats_synced = true; s.spaces_synced = true;
            s.spaces = vec![serde_json::from_value(serde_json::json!({"id":"project","deviceId":"local","path":"/tmp/fieldnotes","createdAt":"2026-09-08T00:00:00Z"})).unwrap()];
            s.chats = vec![serde_json::from_value(serde_json::json!({"id":"browser-fixture","deviceId":"local","spaceId":"project","title":"Build the Fieldnotes workspace","archived":false,"createdAt":"2026-09-08T00:00:00Z","config":{"harness":"claude-code","model":"claude-sonnet-4-6","reasoning":null,"sandbox":"workspace-write"}})).unwrap()];
            s
        });
        let boot = EngineBootConfig { data_dir: data, ipc_port: 0, edge_url: String::new(), edge_token: None, org_id: None, workos_client_id: None, default_harness: HarnessId::ClaudeCode };
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(gpui::point(px(20.),px(30.)), size(px(1320.),px(880.))))),
            ..Default::default()
        }, |_, cx| cx.new(|cx| shell::Shell::new(state.clone(), boot, cx))).unwrap();
        state.update(cx, |_, cx| cx.notify());
        cx.activate(true);
        cx.spawn(async move |cx| {
            let run: anyhow::Result<()> = async {
                pause(cx, 1200).await;
                let (first_id, first) = window.update(cx, |shell, w, cx| shell.fixture_open_browser(None, w, cx))?;
                pause(cx, 500).await;
                capture(&output, "browser-empty-dark")?;
                // A malformed address is rejected without creating a native page.
                window.update(cx, |_, w, cx| first.update(cx, |b, cx| b.navigate("javascript:alert(1)", w, cx)))?;
                anyhow::ensure!(first.read_with(cx, |b, _| b.page.url.is_none()), "unsupported address navigated");
                #[cfg(target_os = "macos")]
                {
                    window.update(cx, |_, w, cx| first.update(cx, |b, cx| b.navigate(&origin, w, cx)))?;
                    let deadline = Instant::now() + Duration::from_secs(25);
                    while !first.read_with(cx, |b, _| b.page.title == "Fieldnotes" && !b.page.loading && b.fixture_native_visible()) {
                        anyhow::ensure!(Instant::now() < deadline, "first page did not load: {:?}", first.read_with(cx, |b, _| b.page.clone()));
                        pause(cx, 50).await;
                    }
                    pause(cx, 500).await;
                    capture(&output, "browser-preview-dark")?;
                    // Real DOM click, native navigation and history.
                    first.read_with(cx, |b, _| b.fixture_eval("document.getElementById('details').click()"));
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while !first.read_with(cx, |b, _| b.page.title == "Details" && b.page.can_back) {
                        anyhow::ensure!(Instant::now() < deadline, "DOM link navigation failed"); pause(cx, 50).await;
                    }
                    first.update(cx, |b, _| b.fixture_history(false));
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while !first.read_with(cx, |b, _| b.page.title == "Fieldnotes" && b.page.can_forward) {
                        anyhow::ensure!(Instant::now() < deadline, "back/forward history failed"); pause(cx, 50).await;
                    }
                    first.read_with(cx, |b, _| b.fixture_eval("history.pushState({}, '', '/same-document'); document.title = 'Updated title'"));
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !first.read_with(cx, |b, _| b.page.title == "Updated title" && b.page.url.as_deref().is_some_and(|u| u.ends_with("/same-document"))) {
                        anyhow::ensure!(Instant::now() < deadline, "same-document state did not update"); pause(cx, 50).await;
                    }
                }
                let (second_id, second) = window.update(cx, |shell, w, cx| shell.fixture_open_browser(None, w, cx))?;
                pause(cx, 250).await;
                anyhow::ensure!(!first.read_with(cx, |b, _| b.fixture_native_visible()), "background page stayed visible");
                window.update(cx, |shell, _, cx| shell.fixture_select_browser(first_id, cx))?;
                pause(cx, 250).await;
                window.update(cx, |shell, w, cx| shell.fixture_close_browser(second_id, w, cx))?;
                drop(second);
                window.update(cx, |shell, _, cx| shell.fixture_browser_menu(true, cx))?;
                pause(cx, 700).await;
                anyhow::ensure!(!first.read_with(cx, |b, _| b.fixture_native_visible()), "page covered a menu");
                capture(&output, "browser-menu-dark")?;
                window.update(cx, |shell, _, cx| shell.fixture_browser_menu(false, cx))?;
                pause(cx, 500).await;
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
                    window.update(cx, |_, w, cx| first.update(cx, |b, cx| b.navigate("http://127.0.0.1:1/unavailable", w, cx)))?;
                    let deadline = Instant::now() + Duration::from_secs(15);
                    while !first.read_with(cx, |b, _| b.page.error.is_some()) {
                        anyhow::ensure!(Instant::now() < deadline, "load failure was not reported"); pause(cx, 50).await;
                    }
                    pause(cx, 300).await; capture(&output, "browser-error-light")?;
                }
                window.update(cx, |shell, w, cx| shell.fixture_close_browser(first_id, w, cx))?;
                pause(cx, 200).await;
                anyhow::ensure!(!first.read_with(cx, |b, _| b.fixture_native_visible()), "closed tab retained its native view");
                std::fs::write(output.join("result.txt"), "PASS: real shell browser fixture; address rejection, tab switching/close, overlays, resizing, takeover and appearance. On macOS: live DOM navigation, history, same-document state, native visibility and load failure.\n")?;
                Ok(())
            }.await;
            if let Err(error) = run { eprintln!("Browser fixture failed: {error:#}"); *result.lock().unwrap() = Some(error.to_string()); }
            cx.update(|cx| cx.quit());
        }).detach();
    });
    if let Some(error) = failure.lock().unwrap().take() {
        anyhow::bail!(error);
    }
    Ok(())
}
