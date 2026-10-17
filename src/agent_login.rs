//! Two-step login for agents and scripts: `jet login start` sends the code and leaves a small
//! background process holding the login window; `jet login code <code>` finishes it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Notify};

use crate::auth::webview::{self, LoginOptions, LoginOutcome, LoginSession};
use crate::commands::{Ctx, display_name};
use crate::config::{self, Config, Paths};
use crate::errors;
use crate::ui;

/// How long a started login waits for its code.
const PENDING_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Serialize, Deserialize)]
struct Pending {
    port: u16,
    nonce: String,
    pid: u32,
    email: String,
}

fn pending_file(paths: &Paths) -> std::path::PathBuf {
    paths.config_dir.join("login-pending.json")
}

/// The email to log in with: given, else the last one used.
pub fn pick_email(config: &Config, email: Option<String>) -> Result<String> {
    email
        .or_else(|| config.email.clone())
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .ok_or_else(|| errors::input_error("which email address? pass --email"))
}

pub fn login_options(config: &Config, email: &str, fresh: bool, show_window: bool) -> LoginOptions {
    // A different account than last time needs a fresh login rather than the remembered session.
    let switching = config
        .email
        .as_deref()
        .is_some_and(|prev| !prev.eq_ignore_ascii_case(email));
    let now = chrono::Utc::now().timestamp();
    LoginOptions {
        force_login: fresh || switching,
        show_window,
        start_visible: config.login_window_until.is_some_and(|t| t > now),
        unattended: false,
    }
}

/// Saves tokens and login preferences; returns the account's display name.
pub fn record_success(
    paths: &Paths,
    config: &mut Config,
    email: &str,
    outcome: &LoginOutcome,
) -> Result<String> {
    if outcome.needed_window {
        config.login_window_until = Some(chrono::Utc::now().timestamp() + 7 * 24 * 3600);
    }
    if !email.is_empty() {
        config.email = Some(email.to_string());
    }
    outcome.tokens.save(paths)?;
    config.save(paths)?;
    Ok(display_name(&outcome.tokens.claims()?))
}

fn report(json_out: bool, value: serde_json::Value, text: String) -> Result<()> {
    if json_out {
        ui::print_json(&value)
    } else {
        eprintln!("{text}");
        Ok(())
    }
}

/// `jet login link`: a login link to open on any device.
pub fn link(ctx: &mut Ctx, fresh: bool) -> Result<()> {
    let url = crate::auth::start_link(&ctx.paths, &ctx.api.tenant, fresh)?;
    show_link(ctx.json, &url)
}

fn show_link(json_out: bool, url: &str) -> Result<()> {
    if json_out {
        return ui::print_json(&link_reply(url));
    }
    crate::auth::print_link(url);
    eprintln!("Then run: jet login code '<address>'");
    Ok(())
}

pub fn link_reply(url: &str) -> serde_json::Value {
    json!({
        "status": "open_link",
        "url": url,
        "next": "The user opens url on their phone or computer and logs in. They end on a plain \
                 text page (robots.txt); they copy that page's full address and give it back. \
                 Finish with `jet login code '<address>'` (MCP: login_complete).",
    })
}

/// `jet login start`: launches the background login and returns once the code was emailed.
/// Without a screen (server, sandbox) it hands out a login link instead.
pub async fn start(ctx: &mut Ctx, email: Option<String>, fresh: bool) -> Result<()> {
    if !webview::can_show_window() {
        return link(ctx, fresh);
    }
    let email = pick_email(&ctx.config, email)?;
    // Only one login at a time.
    if let Some(p) = config::read_json::<Pending>(&pending_file(&ctx.paths))? {
        let _ = std::process::Command::new(if cfg!(windows) { "taskkill" } else { "kill" })
            .args(if cfg!(windows) {
                vec!["/PID".to_string(), p.pid.to_string(), "/F".into()]
            } else {
                vec![p.pid.to_string()]
            })
            .status();
        let _ = std::fs::remove_file(pending_file(&ctx.paths));
    }

    let exe = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["login", "daemon", "--email", &email]);
    if fresh {
        cmd.arg("--fresh");
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // Keep running after this command exits.
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
    let mut child = cmd.spawn().context("couldn't start the background login")?;
    let stdout = child.stdout.take().context("no stdout")?;

    let line = ui::with_spinner("Requesting a login code…", async {
        let mut lines = BufReader::new(stdout).lines();
        tokio::time::timeout(Duration::from_secs(180), lines.next_line()).await
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for the login page"))??
    .context("the background login stopped unexpectedly")?;
    let reply: serde_json::Value = serde_json::from_str(&line).context("unexpected reply")?;
    match reply["status"].as_str() {
        Some("code_sent") => report(
            ctx.json,
            reply.clone(),
            format!("A login code was sent to {email}. Finish with: jet login code <code>"),
        ),
        Some("logged_in") => report(
            ctx.json,
            reply.clone(),
            format!(
                "Logged in as {}.",
                reply["name"].as_str().unwrap_or_default()
            ),
        ),
        Some("open_link") => show_link(ctx.json, reply["url"].as_str().unwrap_or_default()),
        _ => Err(anyhow!(
            "{}",
            reply["message"].as_str().unwrap_or("login failed")
        )),
    }
}

/// Finishes a link login with the pasted address; returns the display name.
pub async fn finish_link(ctx: &mut Ctx, pasted: &str) -> Result<String> {
    let tokens = crate::auth::finish_link(&ctx.api.http, &ctx.paths, pasted).await?;
    let outcome = LoginOutcome {
        tokens,
        needed_window: false,
    };
    let email = outcome.tokens.claims()?["email"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    record_success(&ctx.paths, &mut ctx.config, &email, &outcome)
}

/// `jet login code <code>`: hands the code to the background login, or finishes a link login
/// with the pasted address.
pub async fn submit(ctx: &mut Ctx, code: &str) -> Result<()> {
    let pending: Option<Pending> = config::read_json(&pending_file(&ctx.paths))?;
    if code.contains("code=") || (pending.is_none() && crate::auth::has_pending_link(&ctx.paths)) {
        let name = ui::with_spinner("Logging in…", finish_link(ctx, code)).await?;
        return report(
            ctx.json,
            json!({ "status": "logged_in", "name": name }),
            format!("Logged in as {name}."),
        );
    }
    let pending = pending
        .ok_or_else(|| errors::input_error("no login in progress; run `jet login start` first"))?;
    let url = format!("http://127.0.0.1:{}/{}/code", pending.port, pending.nonce);
    let resp = ui::with_spinner(
        "Checking code…",
        ctx.api
            .http
            .post(&url)
            .json(&json!({ "code": code }))
            .timeout(Duration::from_secs(90))
            .send(),
    )
    .await
    .map_err(|_| {
        errors::input_error("the started login is gone (expired?); run `jet login start` again")
    })?;
    let reply: serde_json::Value = resp.json().await?;
    match reply["status"].as_str() {
        Some("logged_in") => report(
            ctx.json,
            reply.clone(),
            format!(
                "Logged in as {}.",
                reply["name"].as_str().unwrap_or_default()
            ),
        ),
        Some("code_rejected") => Err(errors::input_error(format!(
            "that code didn't work ({}); try `jet login code <code>` again",
            reply["message"].as_str().unwrap_or_default()
        ))),
        _ => Err(anyhow!(
            "{}",
            reply["message"].as_str().unwrap_or("login failed")
        )),
    }
}

struct DaemonState {
    session: Mutex<LoginSession>,
    paths: Paths,
    email: String,
    done: Notify,
}

/// The background half of `jet login start` (hidden `jet login daemon`). Prints one JSON line on
/// stdout when the code was sent (or the login ended), then waits for `jet login code`.
pub async fn daemon(ctx: Ctx, email: String, fresh: bool) -> Result<()> {
    let say = |v: serde_json::Value| println!("{v}");
    let mut opts = login_options(&ctx.config, &email, fresh, false);
    opts.unattended = true;
    let mut session = LoginSession::start(
        ctx.api.http.clone(),
        ctx.paths.clone(),
        ctx.api.tenant.clone(),
        email.clone(),
        opts,
    );
    match session.until_code_sent().await {
        Ok(None) => {}
        Ok(Some(outcome)) => {
            // Remembered session: logged in without a code.
            let mut config = ctx.config.clone();
            let name = record_success(&ctx.paths, &mut config, &email, &outcome)?;
            say(json!({ "status": "logged_in", "name": name }));
            return Ok(());
        }
        Err(e) if e.is::<webview::Unavailable>() => {
            let url = crate::auth::start_link(&ctx.paths, &ctx.api.tenant, fresh)?;
            say(json!({ "status": "open_link", "url": url }));
            return Ok(());
        }
        Err(e) => {
            say(json!({ "status": "error", "message": format!("{e:#}") }));
            return Ok(());
        }
    }

    let nonce = crate::auth::random_token(18);
    let state = Arc::new(DaemonState {
        session: Mutex::new(session),
        paths: ctx.paths.clone(),
        email: email.clone(),
        done: Notify::new(),
    });
    let app = Router::new()
        .route(&format!("/{nonce}/code"), post(handle_code))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    config::write_json(
        &pending_file(&ctx.paths),
        &Pending {
            port,
            nonce,
            pid: std::process::id(),
            email,
        },
    )?;
    say(json!({ "status": "code_sent", "email": state.email }));

    let done = state.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move { done.done.notified().await });
    let _ = tokio::time::timeout(PENDING_TIMEOUT, server).await;
    let _ = std::fs::remove_file(pending_file(&ctx.paths));
    Ok(())
}

#[derive(Deserialize)]
struct CodeRequest {
    code: String,
}

async fn handle_code(
    State(s): State<Arc<DaemonState>>,
    Json(req): Json<CodeRequest>,
) -> Json<serde_json::Value> {
    let mut session = s.session.lock().await;
    session.submit_code(&req.code);
    let reply = match session.until_code_checked().await {
        Ok(Ok(outcome)) => {
            let saved = Config::load(&s.paths)
                .and_then(|mut config| record_success(&s.paths, &mut config, &s.email, &outcome));
            match saved {
                Ok(name) => json!({ "status": "logged_in", "name": name }),
                Err(e) => json!({ "status": "error", "message": format!("{e:#}") }),
            }
        }
        Ok(Err(message)) => return Json(json!({ "status": "code_rejected", "message": message })),
        Err(e) => json!({ "status": "error", "message": format!("{e:#}") }),
    };
    s.done.notify_one();
    Json(reply)
}

// ---- Moving a login between machines ----

const SESSION_PREFIX: &str = "jet1.";

/// `jet auth export`: this login as a portable string (the refresh token).
pub async fn export(ctx: &mut Ctx) -> Result<()> {
    ctx.api.require_token().await?;
    let tokens = ctx.api.tokens().await.context("not logged in")?;
    let body = json!({ "rt": tokens.refresh_token, "t": ctx.api.tenant.code });
    let encoded = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        body.to_string(),
    );
    let session = format!("{SESSION_PREFIX}{encoded}");
    if ctx.json {
        return ui::print_json(&json!({ "session": session }));
    }
    eprintln!("This logs in as you: keep it secret (e.g. as a sandbox's JET_SESSION secret).");
    println!("{session}");
    Ok(())
}

/// Tokens from a `jet auth export` string; they refresh on first use.
pub fn tokens_from_session(s: &str) -> Result<crate::auth::Tokens> {
    let raw = s
        .trim()
        .strip_prefix(SESSION_PREFIX)
        .context("not a session string from `jet auth export`")?;
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, raw)
        .context("damaged session string")?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).context("damaged session string")?;
    let rt = v["rt"].as_str().context("damaged session string")?;
    Ok(crate::auth::Tokens {
        access_token: String::new(),
        refresh_token: rt.to_string(),
        expires_at: 0,
        id_token: None,
    })
}

/// `jet auth import <session>`.
pub async fn import(ctx: &mut Ctx, session: &str) -> Result<()> {
    let stale = tokens_from_session(session)?;
    let fresh = crate::auth::refresh(&ctx.api.http, &ctx.api.tenant, &stale).await?;
    fresh.save(&ctx.paths)?;
    let name = display_name(&fresh.claims()?);
    report(
        ctx.json,
        json!({ "status": "logged_in", "name": name }),
        format!("Logged in as {name}."),
    )
}
