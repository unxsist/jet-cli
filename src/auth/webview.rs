//! `jet login` via the OS web view, driven through the `jet-webview` helper binary.
//!
//! The login page runs in a hidden window. It's only shown when the site's security check
//! needs a human, and hidden again once the email step is done; the code is typed in the
//! terminal. No browser has to be installed.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{LoginRequest, Tokens};
use crate::config::{Paths, Tenant};
use crate::ui;

const PAGE_SCRIPT: &str = include_str!("webview.js");

const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long the hidden window may wait for the security check before it's shown.
const CHECK_TIMEOUT: Duration = Duration::from_secs(4);
/// How long to wait for the login page to react to a submitted code.
const CODE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct LoginOptions {
    pub force_login: bool,
    /// Keep the window visible for the whole login (`--show-browser`).
    pub show_window: bool,
    /// Show the window right away (the security check needed it last time).
    pub start_visible: bool,
}

pub struct LoginOutcome {
    pub tokens: Tokens,
    /// The security check needed the window to be shown.
    pub needed_window: bool,
}

/// The helper couldn't run (missing, or e.g. no WebKitGTK on Linux); try another login method.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "login window unavailable: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

/// `jet-webview` next to the running `jet` (release archives and `cargo install` put them
/// together), else on PATH.
pub fn find_helper() -> Option<PathBuf> {
    let name = format!("jet-webview{}", std::env::consts::EXE_SUFFIX);
    let beside = std::env::current_exe().ok()?.parent()?.join(&name);
    if beside.is_file() {
        return Some(beside);
    }
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(&name))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.is_file())
}

#[derive(Deserialize)]
struct HelperEvent {
    event: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct PageEvent {
    kind: String,
    #[serde(default)]
    detail: Value,
}

enum Msg {
    Helper(HelperEvent),
    HelperGone,
    Code(String),
}

struct Helper {
    stdin: ChildStdin,
}

impl Helper {
    async fn send(&mut self, cmd: Value) {
        let _ = self.stdin.write_all(format!("{cmd}\n").as_bytes()).await;
        let _ = self.stdin.flush().await;
    }
    async fn show(&mut self) {
        self.send(json!({ "cmd": "show" })).await;
    }
    async fn hide(&mut self) {
        self.send(json!({ "cmd": "hide" })).await;
    }
    async fn give_code(&mut self, seq: u64, code: &str) {
        let js = format!(
            "window.__jetCode = {};",
            json!({ "seq": seq, "code": code })
        );
        self.send(json!({ "cmd": "eval", "js": js })).await;
    }
}

/// Reads the code from the terminal on a plain thread: a normal line read never disables echo,
/// so exiting mid-prompt can't leave the terminal broken.
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

fn until(deadline: Option<Instant>) -> tokio::time::Sleep {
    tokio::time::sleep_until(deadline.unwrap_or_else(|| Instant::now() + LOGIN_TIMEOUT))
}

pub async fn login(
    http: &reqwest::Client,
    paths: &Paths,
    tenant: &Tenant,
    email: &str,
    opts: LoginOptions,
) -> Result<LoginOutcome> {
    let helper_path = find_helper().ok_or_else(|| Unavailable("jet-webview not found".into()))?;
    let req = LoginRequest::new(tenant, opts.force_login);
    let debug = std::env::var_os("JET_DEBUG").is_some();

    let mut child = Command::new(&helper_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if debug {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Unavailable(format!("{}: {e}", helper_path.display())))?;
    let mut helper = Helper {
        stdin: child.stdin.take().context("helper stdin")?,
    };
    let stdout = child.stdout.take().context("helper stdout")?;

    let mut visible = opts.show_window || opts.start_visible;
    let script = format!(
        "const JET = {};\n{PAGE_SCRIPT}",
        json!({ "email": email, "authHost": tenant.auth_host })
    );
    helper
        .send(json!({
            "url": req.url,
            "dataDir": paths.webview_data(),
            "script": script,
            "intercept": req.redirect_uri,
            "visible": visible,
            "title": format!("Log in to {}", tenant.site_host.trim_start_matches("www.")),
        }))
        .await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(ev) = serde_json::from_str::<HelperEvent>(&line) {
                    let _ = tx.send(Msg::Helper(ev));
                }
            }
            let _ = tx.send(Msg::HelperGone);
        });
    }

    let mut status = ui::Status::new("Connecting…");
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let mut ready = false;
    let ready_deadline = Instant::now() + Duration::from_secs(15);
    let mut check_deadline: Option<Instant> = None;
    let mut code_deadline: Option<Instant> = None;
    // Shown because of the security check (not --show-browser): hide again after the email step.
    let mut shown_for_check = opts.start_visible && !opts.show_window;
    let mut needed_window = false;
    let mut prompted = false;
    let mut code_seq = 0u64;
    let mut last_code = String::new();

    let result: Result<String> = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break Err(anyhow!("login timed out")),
            _ = tokio::signal::ctrl_c() => break Err(anyhow!("login cancelled")),
            _ = tokio::time::sleep_until(ready_deadline), if !ready => {
                break Err(Unavailable("the login window didn't start".into()).into());
            }
            _ = until(check_deadline), if check_deadline.is_some() => {
                check_deadline = None;
                if !visible {
                    visible = true;
                    shown_for_check = true;
                    needed_window = true;
                    helper.show().await;
                    status.set("Security check: if the login window shows a checkbox, click it.");
                }
            }
            _ = until(code_deadline), if code_deadline.is_some() => {
                break Err(anyhow!(
                    "no response from the login page after submitting the code; \
                     run `JET_DEBUG=1 jet login` to see what it's doing"
                ));
            }
            msg = rx.recv() => match msg {
                None | Some(Msg::HelperGone) => {
                    if ready {
                        break Err(anyhow!("the login window closed before finishing"));
                    }
                    break Err(Unavailable("the login window couldn't start".into()).into());
                }
                Some(Msg::Code(code)) => {
                    code_seq += 1;
                    last_code = code;
                    helper.give_code(code_seq, &last_code).await;
                    status.set("Checking code…");
                    code_deadline = Some(Instant::now() + CODE_TIMEOUT);
                }
                Some(Msg::Helper(ev)) => match ev.event.as_str() {
                    "ready" => ready = true,
                    "closed" => break Err(anyhow!("the login window was closed")),
                    "redirect" => break req.code_from_redirect(ev.url.as_deref().unwrap_or_default()),
                    "ipc" => {
                        let Some(page) = ev.data.as_deref().and_then(|d| serde_json::from_str::<PageEvent>(d).ok())
                        else { continue };
                        let detail = page.detail.as_str().unwrap_or_default().to_string();
                        if debug {
                            status.println(&format!("[page] {} {detail}", page.kind));
                        }
                        match page.kind.as_str() {
                            "page" if detail.contains("registration") => {
                                if !visible {
                                    visible = true;
                                    helper.show().await;
                                }
                                status.println(&format!(
                                    "There's no account for {email} yet. Finish signing up in the window."
                                ));
                            }
                            "page" if detail.contains("/login") => {
                                status.set(&format!("Logging in as {email}…"));
                                if !visible && check_deadline.is_none() {
                                    check_deadline = Some(Instant::now() + CHECK_TIMEOUT);
                                }
                            }
                            "email_submitted" => {
                                check_deadline = None;
                                status.set("Requesting a login code…");
                                if shown_for_check {
                                    shown_for_check = false;
                                    visible = false;
                                    helper.hide().await;
                                }
                            }
                            "email_error" => status.println(&format!("The login page reported: {detail}")),
                            "otp_needed" => {
                                if !prompted {
                                    prompted = true;
                                    status.pause();
                                    eprintln!("A login code was sent to {email}.");
                                    prompt_code(tx.clone());
                                } else if code_seq > 0 {
                                    // The page reloaded; hand it the code again.
                                    helper.give_code(code_seq, &last_code).await;
                                }
                            }
                            "otp_error" => {
                                code_deadline = None;
                                status.pause();
                                eprintln!("That code didn't work ({detail}).");
                                prompt_code(tx.clone());
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                },
            }
        }
    };

    status.pause();
    helper.send(json!({ "cmd": "close" })).await;
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    let code = result?;
    let tokens = req.exchange(http, &code).await?;
    Ok(LoginOutcome {
        tokens,
        needed_window,
    })
}
