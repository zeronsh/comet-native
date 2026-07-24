//! `comet login` / `comet logout` / `comet status` — the standalone auth surface.
//!
//! Sign-in used to live only inside `comet headless`, coupling authentication to
//! the long-running daemon. These commands work on the persisted session
//! (`{data_dir}/session.json`) and exit, so a service-managed `comet headless`
//! only ever *loads* credentials. While an engine is running it owns the session
//! (WorkOS refresh tokens are single-use and rotate on every refresh), so login
//! and logout take the same data-dir lock the engine holds and refuse politely
//! when it is busy.

use std::io::IsTerminal;

use comet_engine::{AuthState, Engine, EngineConfig, InstanceLock};

/// `comet login`: authenticate via the paste-code flow (and workspace
/// onboarding), persist `session.json`, and exit.
pub async fn login(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let auth = Engine::build_auth(&config).await;
    if !auth.workos_enabled() {
        println!("Auth is in dev mode (no WorkOS client id) — there is nothing to sign in to.");
        return Ok(());
    }
    if let AuthState::SignedIn { user, org_id } = auth.state() {
        println!(
            "Already signed in as {}{}.",
            user.email,
            org_id
                .map(|org| format!(" (workspace {org})"))
                .unwrap_or_default()
        );
        println!("Run `comet logout` first to switch accounts.");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("comet login needs an interactive terminal");
    }
    let _lock = engine_lock(&config, "sign in")?;
    comet_engine::terminal_sign_in(&auth).await?;
    match auth.state() {
        AuthState::SignedIn { user, org_id } => {
            println!(
                "\nSigned in as {}{}.",
                user.email,
                org_id
                    .map(|org| format!(" (workspace {org})"))
                    .unwrap_or_default()
            );
            println!("Session saved — `comet headless` (and the daemon) will use it.");
        }
        // terminal_sign_in only returns Ok once signed in; keep an honest fallback.
        _ => println!("Sign-in did not complete."),
    }
    Ok(())
}

/// `comet logout`: remove the persisted session.
pub async fn logout(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let auth = Engine::build_auth(&config).await;
    let _lock = engine_lock(&config, "sign out")?;
    if !auth.workos_enabled() {
        // Dev mode has no live session, but clear any stale session.json from a
        // previous WorkOS-mode run so the next real run starts signed out.
        auth.sign_out();
        println!("Auth is in dev mode — cleared any saved session.");
        return Ok(());
    }
    match auth.state() {
        AuthState::SignedOut => println!("No saved session."),
        state => {
            let email = state
                .user()
                .map(|u| u.email.clone())
                .unwrap_or_else(|| "<unknown>".into());
            auth.sign_out();
            println!(
                "Signed out {email} — removed {}.",
                config.data_dir.join("session.json").display()
            );
        }
    }
    Ok(())
}

/// `comet status`: report auth + engine liveness. Exits nonzero when a sign-in
/// is needed, so scripts (and service health checks) can gate on it.
pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let auth = Engine::build_auth(&config).await;
    println!("Data dir: {}", config.data_dir.display());
    println!("Edge:     {}", config.edge_url);
    let signed_in = match (auth.workos_enabled(), auth.state()) {
        (false, _) => {
            println!("Auth:     dev mode (bearer = user id)");
            true
        }
        (true, AuthState::SignedIn { user, org_id }) => {
            println!(
                "Auth:     signed in as {}{}",
                user.email,
                org_id
                    .map(|org| format!(" (workspace {org})"))
                    .unwrap_or_default()
            );
            true
        }
        (true, AuthState::NeedsOrganization { user }) => {
            println!(
                "Auth:     signed in as {} but no workspace selected — run `comet login`",
                user.email
            );
            false
        }
        (true, AuthState::SignedOut) => {
            println!("Auth:     signed out — run `comet login`");
            false
        }
    };
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.ipc_port));
    let ipc = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
    println!(
        "IPC:      {} 127.0.0.1:{}",
        if ipc.is_ok() { "listening on" } else { "not listening on" },
        config.ipc_port
    );
    if !signed_in {
        std::process::exit(1);
    }
    Ok(())
}

/// The same exclusive data-dir lock the engine holds for its lifetime: taken for
/// the whole login/logout mutation so we never rotate or delete a session out
/// from under a running engine (whose in-memory copy would fight back — the next
/// token refresh re-persists it).
fn engine_lock(config: &EngineConfig, verb: &str) -> anyhow::Result<InstanceLock> {
    InstanceLock::acquire(&config.data_dir).map_err(|err| {
        anyhow::anyhow!(
            "{err}\nCannot {verb} while an engine is running — stop it first \
             (`comet daemon stop`, or quit the Comet app), or use the running UI instead."
        )
    })
}
