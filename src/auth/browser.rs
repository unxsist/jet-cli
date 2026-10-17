//! `jet login`: drives JET's hosted login page in a small Chromium app window.
//!
//! No DevTools protocol is used (Turnstile does not pass with it). Instead the window is started
//! with a tiny bundled extension that fills in the email, fills in the code the user types in the
//! terminal, and reports the final `/signin-oidc?code=` redirect back to a localhost server.
//! The browser profile is persistent, so later logins usually skip steps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use sha2::{Digest, Sha256};

use super::LoginRequest;
pub use super::webview::{LoginOptions, LoginOutcome};
use crate::config::{Paths, Tenant};
use crate::ui;

const MANIFEST: &str = include_str!("ext/manifest.json");
const BG_JS: &str = include_str!("ext/bg.js");
const CS_JS: &str = include_str!("ext/cs.js");

const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Deserialize)]
struct ExtEvent {
    kind: String,
    detail: Option<Value>,
}

enum Msg {
    Ext(ExtEvent),
    Callback(String),
    Code(String),
}

struct Shared {
    email: String,
    /// Where the local start page sends the browser next (the authorize URL).
    next_url: String,
    otp: Mutex<(u64, String)>,
    tx: mpsc::UnboundedSender<Msg>,
}

pub fn find_browser(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = configured {
        return Some(PathBuf::from(p));
    }
    // Branded Google Chrome and Edge ignore --load-extension, so only Chromium builds and Brave.
    let on_path = ["chromium", "chromium-browser", "brave", "brave-browser"];
    let mut fixed: Vec<PathBuf> = vec![
        "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser".into(),
    ];
    for var in ["LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)"] {
        if let Some(base) = std::env::var_os(var).map(PathBuf::from) {
            fixed.push(base.join(r"Chromium\Application\chrome.exe"));
            fixed.push(base.join(r"BraveSoftware\Brave-Browser\Application\brave.exe"));
        }
    }
    let exe = std::env::consts::EXE_SUFFIX;
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    on_path
        .iter()
        .flat_map(|name| {
            path_dirs
                .iter()
                .map(move |d| d.join(format!("{name}{exe}")))
        })
        .chain(fixed)
        .find(|p| p.is_file())
}

/// Writes the extension into a directory named after its content. Chromium caches an unpacked
/// extension's service worker in the profile, so changed code must get a new path (and ID).
fn write_extension(data_dir: &Path, tenant: &Tenant) -> Result<PathBuf> {
    let manifest = MANIFEST
        .replace("__SITE_HOST__", tenant.site_host)
        .replace("__AUTH_HOST__", tenant.auth_host);
    let files = [
        ("manifest.json", manifest.as_str()),
        ("bg.js", BG_JS),
        ("cs.js", CS_JS),
    ];
    let mut hasher = Sha256::new();
    for (name, body) in files {
        hasher.update(name);
        hasher.update(body);
    }
    let hash: String = hasher
        .finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    let dir = data_dir.join(format!("login-extension-{hash}"));
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
        for (name, body) in files {
            std::fs::write(dir.join(name), body)?;
        }
    }
    // Remove extensions written by older versions.
    for entry in std::fs::read_dir(data_dir)?.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("login-extension") && entry.path() != dir {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(dir)
}

fn html_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

async fn serve(shared: Arc<Shared>, nonce: &str) -> Result<u16> {
    let p = |s: &str| format!("/{nonce}{s}");
    let app = Router::new()
        // The extension reads jet-cli's address from this page's URL, then continues to next_url.
        .route(
            &p("/start"),
            get(|State(s): State<Arc<Shared>>| async move {
                let next = s.next_url.clone();
                Html(format!(
                    "<!doctype html><meta charset=utf-8><title>jet-cli login</title>\
                     <body data-auth-url=\"{}\" style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\">\
                     <p>Starting login…</p>",
                    html_attr(&next)
                ))
            }),
        )
        .route(&p("/config"), get(|State(s): State<Arc<Shared>>| async move { Json(json!({ "email": s.email })) }))
        .route(
            &p("/otp"),
            get(|State(s): State<Arc<Shared>>| async move {
                let (seq, code) = s.otp.lock().unwrap().clone();
                Json(json!({ "seq": seq, "code": code }))
            }),
        )
        .route(
            &p("/event"),
            post(|State(s): State<Arc<Shared>>, Json(ev): Json<ExtEvent>| async move {
                let _ = s.tx.send(Msg::Ext(ev));
                Json(json!({}))
            }),
        )
        .route(
            &p("/callback"),
            post(|State(s): State<Arc<Shared>>, Json(body): Json<Value>| async move {
                if let Some(url) = body["url"].as_str() {
                    let _ = s.tx.send(Msg::Callback(url.to_string()));
                }
                Json(json!({}))
            }),
        )
        .route(
            &p("/done"),
            get(|| async {
                Html(
                    "<!doctype html><meta charset=utf-8><title>jet-cli</title>\
                     <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\">\
                     <p>Logged in. You can return to the terminal.</p>",
                )
            }),
        )
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(port)
}

/// Asks for the emailed code on a plain thread, so a pending prompt never blocks shutdown
/// (e.g. when the user types the code in the window instead). Reads a normal line from the
/// terminal: no raw mode, so the terminal is never left without echo if we exit mid-prompt.
fn prompt_code(tx: mpsc::UnboundedSender<Msg>) {
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        eprint!("Code: ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin()
            .lock()
            .read_line(&mut line)
            .is_ok_and(|n| n > 0)
        {
            let code: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            if !code.is_empty() {
                let _ = tx.send(Msg::Code(code));
            }
        }
    });
}

fn launch(
    browser: &Path,
    paths: &Paths,
    extension: &Path,
    url: &str,
    headless: bool,
) -> Result<Child> {
    let mut cmd = Command::new(browser);
    cmd.arg(format!(
        "--user-data-dir={}",
        paths.browser_profile().display()
    ))
    .arg(format!("--load-extension={}", extension.display()))
    .arg("--disable-features=DisableLoadExtensionCommandLineSwitch")
    .arg("--no-first-run")
    .arg("--no-default-browser-check")
    .arg("--window-size=520,860")
    .arg(format!("--app={url}"));
    if headless {
        cmd.arg("--headless=new");
    } else if cfg!(target_os = "linux") {
        // The "auto" hint relies on XDG_SESSION_TYPE, which isn't always exported.
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            cmd.arg("--ozone-platform=wayland");
        } else {
            cmd.arg("--ozone-platform-hint=auto");
        }
    }
    // Distro launchers (e.g. Arch's) add flags from $XDG_CONFIG_HOME/chromium-flags.conf to every
    // launch; personal flags there (Wayland, extra extensions) break the headless login.
    cmd.env("XDG_CONFIG_HOME", paths.data_dir.join("browser-config"));
    let stderr = match std::env::var_os("JET_DEBUG") {
        Some(_) => {
            let log = paths.data_dir.join("browser.log");
            eprintln!("[debug] browser log: {}", log.display());
            cmd.arg("--enable-logging=stderr");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)?;
            std::process::Stdio::from(file)
        }
        None => std::process::Stdio::null(),
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .kill_on_drop(true);
    cmd.spawn()
        .with_context(|| format!("failed to start {}", browser.display()))
}

/// A login window left over from an interrupted run keeps the profile locked, and Chromium would
/// hand our new window to it (with a stale extension config). Close it first.
#[cfg(not(unix))]
async fn close_stale_window(_profile: &Path) {}

#[cfg(unix)]
async fn close_stale_window(profile: &Path) {
    let Ok(target) = std::fs::read_link(profile.join("SingletonLock")) else {
        return;
    };
    let Some(pid) = target
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|p| p.parse::<u32>().ok())
    else {
        return;
    };
    let Ok(out) = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
    else {
        return;
    };
    if !String::from_utf8_lossy(&out.stdout).contains(&*profile.to_string_lossy()) {
        return;
    }
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success());
        if !alive {
            return;
        }
    }
}

async fn terminated() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

async fn close(mut child: Child) {
    // SIGTERM lets Chromium flush the profile (cookies make the next login smoother).
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        if tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .is_ok()
        {
            return;
        }
    }
    let _ = child.kill().await;
}

/// How long the hidden browser may wait for the security check before we show it to the user.
const HEADLESS_CHECK_TIMEOUT: Duration = Duration::from_secs(8);

/// How long to wait for the login page to react to a submitted code.
const CODE_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn login(
    http: &reqwest::Client,
    paths: &Paths,
    tenant: &Tenant,
    browser: &Path,
    email: &str,
    opts: LoginOptions,
) -> Result<LoginOutcome> {
    let LoginOptions {
        force_login,
        show_window,
        start_visible,
    } = opts;
    let req = LoginRequest::new(tenant, force_login);
    let nonce = super::random_token(18);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        email: email.to_string(),
        next_url: req.url.clone(),
        otp: Mutex::new((0, String::new())),
        tx: tx.clone(),
    });
    let port = serve(shared.clone(), &nonce).await?;
    std::fs::create_dir_all(&paths.data_dir)?;
    let extension = write_extension(&paths.data_dir, tenant)?;

    close_stale_window(&paths.browser_profile()).await;
    let start_url = format!("http://127.0.0.1:{port}/{nonce}/start");
    // The browser runs hidden; it's only shown if the security check needs a human.
    let mut headless = !show_window && !start_visible;
    let mut needed_window = false;
    let mut child = launch(browser, paths, &extension, &start_url, headless)?;
    let mut status = ui::Status::new("Connecting…");

    let deadline = tokio::time::sleep(LOGIN_TIMEOUT);
    tokio::pin!(deadline);
    let ready_deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(ready_deadline);
    let mut ready = false;
    // While hidden: when to give up waiting for the security check and show the window.
    let mut check_deadline: Option<tokio::time::Instant> = None;
    let mut told_turnstile = false;
    // Set once the server accepted the code; the page then needs a few seconds to redirect.
    let mut code_accepted = false;
    let mut prompted = false;
    // After submitting a code we expect an answer quickly; never wait silently.
    let mut code_deadline: Option<tokio::time::Instant> = None;

    let result: Result<String> = loop {
        let mut show_reason: Option<&str> = None;
        tokio::select! {
            _ = &mut deadline => break Err(anyhow::anyhow!("login timed out")),
            _ = terminated() => break Err(anyhow::anyhow!("login cancelled")),
            _ = &mut ready_deadline, if !ready => break Err(anyhow::anyhow!(
                "the login helper didn't start in {}; try `jet login --manual`",
                browser.display()
            )),
            _ = tokio::time::sleep_until(check_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if headless && check_deadline.is_some() =>
            {
                show_reason = Some("The security check needs you; opening a login window.");
            }
            _ = tokio::time::sleep_until(code_deadline.unwrap_or_else(tokio::time::Instant::now)),
                if code_deadline.is_some() =>
            {
                break Err(anyhow::anyhow!(
                    "no response from the login page after submitting the code; \
                     run `JET_DEBUG=1 jet login` to see what it's doing"
                ));
            }
            status = child.wait() => {
                break Err(anyhow::anyhow!("the login window was closed before finishing ({})", status?));
            }
            msg = rx.recv() => match msg {
                None => break Err(anyhow::anyhow!("internal error: login server stopped")),
                Some(Msg::Callback(url)) => break req.code_from_redirect(&url),
                Some(Msg::Code(code)) => {
                    let mut otp = shared.otp.lock().unwrap();
                    otp.0 += 1;
                    otp.1 = code;
                    status.set("Checking code…");
                    code_deadline = Some(tokio::time::Instant::now() + CODE_TIMEOUT);
                }
                Some(Msg::Ext(ev)) => {
                    ready = true;
                    if std::env::var_os("JET_DEBUG").is_some() {
                        status.println(&format!("[ext] {} {}", ev.kind, ev.detail.clone().unwrap_or_default()));
                    }
                    let detail = ev.detail.as_ref().and_then(Value::as_str).unwrap_or_default().to_string();
                    match ev.kind.as_str() {
                        "page" if detail.contains("registration") => {
                            if headless {
                                show_reason = Some("There's no account for this email yet; opening a window to sign up.");
                            } else {
                                status.println(&format!(
                                    "There's no account for {email} yet. Finish signing up in the window."
                                ));
                            }
                        }
                        "page" if detail.contains("/login") => {
                            status.set(&format!("Logging in as {email}…"));
                            if headless && check_deadline.is_none() {
                                check_deadline = Some(tokio::time::Instant::now() + HEADLESS_CHECK_TIMEOUT);
                            }
                        }
                        "turnstile_wait" if !headless && !told_turnstile => {
                            told_turnstile = true;
                            status.set("Waiting for the security check. If the window shows a checkbox, click it.");
                        }
                        "email_submitted" => {
                            check_deadline = None;
                            status.set("Requesting a login code…");
                        }
                        "api" if detail.ends_with(" 429") => {
                            break Err(anyhow::anyhow!(
                                "JET is rate-limiting login attempts; try again in a few minutes"
                            ));
                        }
                        "api" if detail.contains("/email/validate") && !detail.ends_with(" 200") && headless => {
                            show_reason = Some("The security check needs you; opening a login window.");
                        }
                        "email_error" => status.println(&format!("The login page reported: {detail}")),
                        "otp_needed" => {
                            if !prompted {
                                prompted = true;
                                status.pause();
                                eprintln!("A login code was sent to {email}.");
                                prompt_code(tx.clone());
                            }
                        }
                        "api" if detail.contains("/otp/validate") => {
                            if detail.ends_with(" 200") {
                                code_accepted = true;
                                status.set("Finishing login…");
                                // Allow time for the redirect back to the site.
                                code_deadline = Some(tokio::time::Instant::now() + CODE_TIMEOUT);
                            }
                        }
                        "otp_error" if !code_accepted => {
                            code_deadline = None;
                            status.pause();
                            eprintln!("That code didn't work ({detail}).");
                            prompt_code(tx.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some(reason) = show_reason {
            needed_window = true;
            status.println(reason);
            close(child).await;
            headless = false;
            check_deadline = None;
            child = launch(browser, paths, &extension, &start_url, false)?;
        }
    };

    status.pause();
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            close(child).await;
            return Err(e);
        }
    };
    // Give the "done" page a moment to render, then close the window.
    if !headless {
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    close(child).await;
    let tokens = req.exchange(http, &code).await?;
    if tokens.access_token.is_empty() {
        bail!("login returned an empty token");
    }
    Ok(LoginOutcome {
        tokens,
        needed_window,
    })
}
