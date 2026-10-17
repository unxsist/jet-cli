use anyhow::{Context, Result, bail};
use reqwest::{Method, RequestBuilder};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::auth::{self, Tokens};
use crate::config::{Paths, Tenant};

pub const REST: &str = "https://rest.api.eu-central-1.production.jet-external.com";
pub const MENU_CDN: &str = "https://globalmenucdn.eu-central-1.production.jet-external.com";

pub struct Api {
    pub http: reqwest::Client,
    pub tenant: Tenant,
    paths: Paths,
    tokens: Mutex<Option<Tokens>>,
}

#[derive(Deserialize)]
struct JetErrors {
    errors: Vec<JetError>,
}

/// The basket API's error format.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BasketError {
    error_type: String,
    #[serde(default)]
    message: Option<String>,
}

fn explain(error_type: &str) -> Option<&'static str> {
    Some(match error_type {
        "RestaurantDoesNotDeliverToLocation" => "this restaurant doesn't deliver to your address",
        "RestaurantOffline" | "RestaurantClosed" => "this restaurant is closed right now",
        "ProductNotFound" | "ProductUnavailable" => "an item is no longer available",
        "BasketNotFound" => "basket not found (404)",
        _ => return None,
    })
}

#[derive(Deserialize)]
struct JetError {
    description: Option<String>,
    #[serde(rename = "errorCode")]
    error_code: Option<String>,
}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("jet-cli/", env!("CARGO_PKG_VERSION")))
        .gzip(true)
        .build()?)
}

impl Api {
    pub fn new(paths: Paths, tenant: Tenant) -> Result<Self> {
        let tokens = Tokens::load(&paths)?;
        Ok(Self {
            http: http_client()?,
            tenant,
            paths,
            tokens: Mutex::new(tokens),
        })
    }

    /// A valid access token if logged in, refreshing (and persisting) it when needed.
    pub async fn token(&self) -> Result<Option<String>> {
        let mut guard = self.tokens.lock().await;
        let Some(tokens) = guard.as_ref() else {
            return Ok(None);
        };
        if tokens.needs_refresh() {
            let fresh = auth::refresh(&self.http, &self.tenant, tokens).await?;
            fresh.save(&self.paths)?;
            *guard = Some(fresh);
        }
        Ok(guard.as_ref().map(|t| t.access_token.clone()))
    }

    pub async fn require_token(&self) -> Result<String> {
        self.token()
            .await?
            .context("not logged in, run `jet login`")
    }

    pub async fn tokens(&self) -> Option<Tokens> {
        self.tokens.lock().await.clone()
    }

    pub fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.http
            .request(method, url)
            .header("Accept", "application/json")
            .header("Accept-Language", self.tenant.language)
            .header("Accept-Tenant", self.tenant.code)
    }

    /// Like `request`, but sends the access token when logged in so the server ties
    /// baskets etc. to the account.
    pub async fn request_auth(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        let rb = self.request(method, url);
        Ok(match self.token().await? {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        })
    }

    pub async fn send<T: DeserializeOwned>(&self, rb: RequestBuilder) -> Result<T> {
        let resp = rb.send().await?;
        let status = resp.status();
        let url = resp.url().path().to_string();
        let body = resp.text().await?;
        if !status.is_success() {
            if let Ok(e) = serde_json::from_str::<JetErrors>(&body) {
                let msgs: Vec<String> = e
                    .errors
                    .into_iter()
                    .map(|e| e.description.or(e.error_code).unwrap_or_default())
                    .collect();
                bail!("{} ({status})", msgs.join("; "));
            }
            if let Ok(e) = serde_json::from_str::<BasketError>(&body) {
                match explain(&e.error_type) {
                    Some(text) => bail!("{text}"),
                    None => bail!(
                        "{} ({status}){}",
                        e.error_type,
                        e.message
                            .filter(|m| !m.is_empty())
                            .map(|m| format!(": {m}"))
                            .unwrap_or_default()
                    ),
                }
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                bail!("not authorized ({url}); try `jet login`");
            }
            bail!(
                "request to {url} failed ({status}): {}",
                body.chars().take(300).collect::<String>()
            );
        }
        serde_json::from_str(&body).with_context(|| format!("unexpected response from {url}"))
    }
}
