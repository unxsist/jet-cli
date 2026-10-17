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
    config.email = Some(email.to_string());
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

/// `jet login start`: launches the background login and returns once the code was emailed.
pub async fn start(ctx: &mut Ctx, email: Option<String>, fresh: bool) -> Result<()> {
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
        _ => Err(anyhow!(
            "{}",
            reply["message"].as_str().unwrap_or("login failed")
        )),
    }
}

/// `jet login code <code>`: hands the code to the background login.
pub async fn submit(ctx: &mut Ctx, code: &str) -> Result<()> {
    let pending: Pending = config::read_json(&pending_file(&ctx.paths))?
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
    let opts = login_options(&ctx.config, &email, fresh, false);
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
        Err(e) => {
            let unavailable = e.is::<webview::Unavailable>();
            say(json!({
                "status": "error",
                "message": if unavailable {
                    format!("{e:#}; run `jet login` in a terminal instead")
                } else {
                    format!("{e:#}")
                },
            }));
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
