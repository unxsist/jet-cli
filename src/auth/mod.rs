pub mod browser;
pub mod webview;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::{Paths, Tenant, read_json, write_json};

pub const CLIENT_ID: &str = "consumer_web_je";
const SCOPE: &str = "openid profile mobile_scope offline_access";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
    #[serde(default)]
    pub id_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

impl Tokens {
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        read_json(&paths.auth_file())
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_json(&paths.auth_file(), self)
    }

    pub fn needs_refresh(&self) -> bool {
        chrono::Utc::now().timestamp() > self.expires_at - 60
    }

    pub fn claims(&self) -> Result<serde_json::Value> {
        jwt_claims(&self.access_token)
    }
}

pub fn jwt_claims(jwt: &str) -> Result<serde_json::Value> {
    let payload = jwt.split('.').nth(1).context("malformed token")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("malformed token")?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// One login attempt: PKCE verifier + state, and the URL that starts it.
#[derive(Serialize, Deserialize)]
pub struct LoginRequest {
    auth_base: String,
    pub verifier: String,
    pub state: String,
    pub redirect_uri: String,
    pub url: String,
}

pub(crate) fn random_token(len: usize) -> String {
    let bytes: Vec<u8> = (0..len).map(|_| rand::random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

impl LoginRequest {
    pub fn new(tenant: &Tenant, force_login: bool) -> Self {
        // The web client only accepts callbacks on the site itself, not a loopback address.
        Self::with_redirect(
            tenant,
            force_login,
            format!("https://{}/signin-oidc", tenant.site_host),
        )
    }

    /// For logins finished on another device: the browser ends on a static page (no script
    /// redirects away), so the address with the code stays visible to copy. The code is useless
    /// without this request's PKCE verifier, so it's safe to paste into a chat.
    pub fn for_link(tenant: &Tenant, force_login: bool) -> Self {
        Self::with_redirect(
            tenant,
            force_login,
            format!("https://{}/robots.txt", tenant.site_host),
        )
    }

    fn with_redirect(tenant: &Tenant, force_login: bool, redirect_uri: String) -> Self {
        let verifier = random_token(48);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_token(16);
        let auth_base = tenant.auth_base();
        let mut url = Url::parse(&format!("{auth_base}/connect/authorize")).unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", SCOPE)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("acr_values", &format!("tenant:{}", tenant.code))
            .append_pair("ui_locales", tenant.language);
        if force_login {
            url.query_pairs_mut().append_pair("prompt", "login");
        }
        Self {
            auth_base,
            verifier,
            state,
            redirect_uri,
            url: url.into(),
        }
    }

    /// Extracts the authorization code from the URL the browser was redirected to (or takes a
    /// bare code).
    pub fn code_from_redirect(&self, redirected: &str) -> Result<String> {
        let pasted = redirected
            .trim()
            .trim_matches(|c| c == '\'' || c == '"' || c == '<' || c == '>');
        if !pasted.contains("://") && !pasted.contains("code=") && !pasted.is_empty() {
            return Ok(pasted.to_string());
        }
        let url = Url::parse(pasted).context("that doesn't look like the address of the page")?;
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if let Some(err) = param("error") {
            bail!(
                "login failed: {err} {}",
                param("error_description").unwrap_or_default()
            );
        }
        let code = param("code").context("no ?code= in that URL")?;
        if param("state").as_deref() != Some(self.state.as_str()) {
            bail!("state mismatch; this URL belongs to a different login attempt");
        }
        Ok(code)
    }

    pub async fn exchange(&self, http: &reqwest::Client, code: &str) -> Result<Tokens> {
        token_request(
            http,
            &self.auth_base,
            &[
                ("client_id", CLIENT_ID),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.redirect_uri),
                ("code_verifier", &self.verifier),
            ],
            None,
        )
        .await
    }
}

pub async fn refresh(http: &reqwest::Client, tenant: &Tenant, tokens: &Tokens) -> Result<Tokens> {
    token_request(
        http,
        &tenant.auth_base(),
        &[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", &tokens.refresh_token),
        ],
        Some(tokens),
    )
    .await
    .context("session expired, run `jet login`")
}

async fn token_request(
    http: &reqwest::Client,
    auth_base: &str,
    form: &[(&str, &str)],
    previous: Option<&Tokens>,
) -> Result<Tokens> {
    let resp = http
        .post(format!("{auth_base}/connect/token"))
        .form(form)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        if let Ok(e) = serde_json::from_str::<TokenError>(&body) {
            bail!(
                "token request failed: {} {}",
                e.error,
                e.error_description.unwrap_or_default()
            );
        }
        bail!("token request failed ({status}): {body}");
    }
    let t: TokenResponse = serde_json::from_str(&body).context("unexpected token response")?;
    let refresh_token = t
        .refresh_token
        .or_else(|| previous.map(|p| p.refresh_token.clone()))
        .context("no refresh token returned")?;
    Ok(Tokens {
        access_token: t.access_token,
        refresh_token,
        expires_at: chrono::Utc::now().timestamp() + t.expires_in,
        id_token: t
            .id_token
            .or_else(|| previous.and_then(|p| p.id_token.clone())),
    })
}

/// Prints a login link (and QR code) to open on any device.
pub fn print_link(url: &str) {
    eprintln!("Open this link on your phone or computer and log in:\n\n  {url}\n");
    if let Ok(code) = qrcode::QrCode::new(url.as_bytes()) {
        let qr = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        eprintln!("{qr}");
    }
    eprintln!(
        "After logging in you end on a page of plain text (robots.txt).\n\
         Copy the full address of that page and paste it here."
    );
}

/// Login in any browser (another device too): open the link, log in, paste the final address.
pub async fn login_manual(
    http: &reqwest::Client,
    tenant: &Tenant,
    force_login: bool,
) -> Result<Tokens> {
    let req = LoginRequest::for_link(tenant, force_login);
    print_link(&req.url);
    let _ = open::that_detached(&req.url);
    loop {
        let pasted: String = dialoguer::Input::new()
            .with_prompt("Address")
            .interact_text()?;
        match req.code_from_redirect(&pasted) {
            Ok(code) => return req.exchange(http, &code).await,
            Err(e) => eprintln!("{e:#}"),
        }
    }
}

/// A link login waiting for its pasted address (`jet login link` / MCP in a sandbox).
fn pending_link_file(paths: &Paths) -> std::path::PathBuf {
    paths.config_dir.join("login-link.json")
}

pub fn start_link(paths: &Paths, tenant: &Tenant, force_login: bool) -> Result<String> {
    let req = LoginRequest::for_link(tenant, force_login);
    write_json(&pending_link_file(paths), &req)?;
    Ok(req.url)
}

pub fn has_pending_link(paths: &Paths) -> bool {
    pending_link_file(paths).exists()
}

pub async fn finish_link(http: &reqwest::Client, paths: &Paths, pasted: &str) -> Result<Tokens> {
    let req: LoginRequest = read_json(&pending_link_file(paths))?.ok_or_else(|| {
        crate::errors::input_error("no login link was started; run `jet login link` first")
    })?;
    let code = req
        .code_from_redirect(pasted)
        .map_err(|e| crate::errors::input_error(format!("{e:#}")))?;
    let tokens = req.exchange(http, &code).await?;
    let _ = std::fs::remove_file(pending_link_file(paths));
    Ok(tokens)
}
