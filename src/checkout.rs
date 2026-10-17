//! Checkout and payment (see docs/RESEARCH.md for the API flow).
//!
//! `prepare` fills in what the checkout needs (contact, address, time) from config and the last
//! order. `pay` starts an online payment: JET answers with a payment page URL (iDEAL, card,
//! PayPal) that the user opens to pay. Until they pay there is no order; the restaurant only
//! gets it after payment.

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, REST};
use crate::basket::Current;
use crate::commands::Ctx;
use crate::config::{self, Contact, Location};
use crate::errors::{self, needs_input};
use crate::orders;

// ---- API shapes (only what we use) ----

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkout {
    pub restaurant: CheckoutRestaurant,
    pub purchase: Purchase,
    #[serde(default)]
    pub service_type: String,
    #[serde(default)]
    pub fulfilment: Option<Fulfilment>,
    #[serde(default)]
    pub is_fulfillable: bool,
    #[serde(default)]
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckoutRestaurant {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Purchase {
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub line_items: Vec<LineItem>,
    pub total: Total,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Group {
    #[serde(default)]
    pub products: Vec<CheckoutProduct>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckoutProduct {
    pub name: String,
    pub quantity: u32,
    pub price: Price,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LineItem {
    pub name: String,
    pub price: Price,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Total {
    pub price: Price,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Price {
    /// Cents.
    pub amount: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fulfilment {
    pub time: Option<FulfilmentTime>,
    pub location: Option<FulfilmentLocation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FulfilmentTime {
    #[serde(default)]
    pub asap: bool,
    pub scheduled: Option<Slot>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Slot {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FulfilmentLocation {
    pub address: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub code: String,
}

#[derive(Debug, Deserialize)]
struct Times {
    times: Vec<Slot>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentType {
    pub payment_type: String,
    pub display_name: String,
    #[serde(default)]
    pub behaviour: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaymentOptions {
    #[serde(default)]
    available_payment_types: Vec<PaymentType>,
}

/// An online payment waiting for the user, remembered for `jet order status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOrder {
    pub basket_id: String,
    pub restaurant: String,
    pub total_cents: i64,
    pub payment_method: String,
    pub payment_url: String,
    pub status_url: Option<String>,
    pub created_at: i64,
}

// ---- API calls ----

fn url(api: &Api, basket_id: &str, rest: &str) -> String {
    format!("{REST}/checkout/{}/{basket_id}{rest}", api.tenant.code)
}

async fn patch(api: &Api, basket_id: &str, ops: Vec<Value>) -> Result<Checkout> {
    let token = api.require_token().await?;
    let rb = api
        .request(Method::PATCH, &url(api, basket_id, ""))
        .bearer_auth(token)
        .header("Content-Type", "application/json-patch+json")
        .body(serde_json::to_vec(&ops)?);
    api.send(rb).await
}

pub async fn available_times(api: &Api, basket_id: &str) -> Result<Vec<Slot>> {
    let token = api.require_token().await?;
    let t: Times = api
        .send(
            api.request(
                Method::GET,
                &url(api, basket_id, "/fulfilment/availabletimes"),
            )
            .bearer_auth(token),
        )
        .await?;
    Ok(t.times)
}

/// Payment methods that end on a payment page we can hand to the user (Apple/Google Pay and the
/// like need a native app).
pub async fn payment_methods(
    api: &Api,
    checkout: &Checkout,
    basket_id: &str,
) -> Result<Vec<PaymentType>> {
    let token = api.require_token().await?;
    let service = if checkout.service_type.is_empty() {
        "delivery"
    } else {
        &checkout.service_type
    };
    let rb = api
        .request(
            Method::GET,
            &format!(
                "{REST}/payment/{}/partners/{}/options",
                api.tenant.code, checkout.restaurant.id
            ),
        )
        .bearer_auth(token)
        .query(&[("serviceType", service), ("checkoutId", basket_id)]);
    let opts: PaymentOptions = api.send(rb).await?;
    Ok(opts
        .available_payment_types
        .into_iter()
        .filter(|p| {
            p.status == "available"
                && p.behaviour != "native"
                && !["voucher", "jetpay"].contains(&p.payment_type.as_str())
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct PayResponse {
    status: PayStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayStatus {
    code: String,
    #[serde(default)]
    redirect: Option<PayRedirect>,
    #[serde(default)]
    order_id: Option<String>,
    #[serde(default)]
    reasons: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayRedirect {
    url: String,
    #[serde(default)]
    polling_page_url: Option<String>,
}

pub enum PayOutcome {
    /// The user has to finish paying on this page.
    PaymentPage {
        url: String,
        status_url: Option<String>,
    },
    /// Placed without an online payment step (e.g. cash).
    Placed { order_id: Option<String> },
}

async fn pay(api: &Api, basket_id: &str, total_cents: i64, method: &str) -> Result<PayOutcome> {
    let token = api.require_token().await?;
    let body = json!({
        "currency": "EUR",
        "total": total_cents,
        "methods": [{ "type": method, "amount": total_cents, "fee": null }],
        // Where the payment page sends the user afterwards.
        "returnUrl": format!("https://{}/checkout#checkout", api.tenant.site_host),
    });
    let rb = api
        .request(Method::POST, &url(api, basket_id, "/payments"))
        .bearer_auth(token)
        // Identifies the web client family; required by the payments API.
        .header("x-jet-application-id", "16")
        .header(
            "x-jet-application-version",
            concat!("jet-cli-", env!("CARGO_PKG_VERSION")),
        )
        .json(&body);
    let resp: PayResponse = api.send(rb).await?;
    let st = resp.status;
    match st.code.as_str() {
        "awaitingAction" => {
            let r = st
                .redirect
                .context("JET asked for a payment step but sent no payment page")?;
            Ok(PayOutcome::PaymentPage {
                url: r.url,
                status_url: r.polling_page_url,
            })
        }
        "accepted" | "pending" => Ok(PayOutcome::Placed {
            order_id: st.order_id,
        }),
        other => {
            let reason = st
                .reasons
                .unwrap_or_default()
                .iter()
                .filter_map(|r| r.get("code").and_then(Value::as_str).map(str::to_string))
                .collect::<Vec<_>>()
                .join(", ");
            match reason.as_str() {
                "totalsDoNotMatch" => bail!("the total changed; check the order again"),
                "duplicate" => bail!("this order was already placed"),
                _ => bail!(
                    "payment {other}{}",
                    if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" ({reason})")
                    }
                ),
            }
        }
    }
}

// ---- Filling in the checkout ----

pub struct Prepare {
    /// "asap", or "HH:MM" for the first slot at or after that time (local).
    pub time: Option<String>,
    pub note: Option<String>,
    pub contact: Option<Contact>,
}

fn explain_issue(code: &str) -> String {
    match code {
        "FIRST_NAME_REQUIRED" | "LAST_NAME_REQUIRED" => "a name is needed (--name)".into(),
        "PHONE_NUMBER_REQUIRED" | "PHONE_NUMBER_INVALID" => {
            "a valid phone number is needed (--phone)".into()
        }
        "FULFILMENT_TIME_REQUIRED" | "FULFILMENT_TIME_UNAVAILABLE" => {
            "no delivery time available right now".into()
        }
        "ADDRESS_LINES_REQUIRED"
        | "LOCALITY_REQUIRED"
        | "POSTAL_CODE_REQUIRED"
        | "GEOLOCATION_REQUIRED" => {
            "the delivery address is incomplete; set it with `jet address`".into()
        }
        "MINIMUM_ORDER_VALUE_NOT_MET" | "MinimumOrderValueNotMet" => {
            "the minimum order amount isn't reached yet".into()
        }
        "RESTAURANT_NOT_TAKING_ORDERS" | "RESTAURANT_OFFLINE" => {
            "the restaurant isn't taking orders right now".into()
        }
        other => other.to_lowercase().replace('_', " "),
    }
}

/// Name/phone: explicit, saved, else from the most recent order.
async fn contact(ctx: &mut Ctx, given: Option<Contact>) -> Result<Contact> {
    if let Some(c) = given {
        ctx.config.contact = Some(c.clone());
        ctx.config.save(&ctx.paths)?;
        return Ok(c);
    }
    if let Some(c) = &ctx.config.contact {
        return Ok(c.clone());
    }
    let last = orders::recent(&ctx.api, 1)
        .await?
        .into_iter()
        .next()
        .and_then(|o| o.consumer);
    match last {
        Some(c) if !c.name.trim().is_empty() && !c.contact_number.trim().is_empty() => {
            let (first, last) = c.name.trim().split_once(' ').unwrap_or((c.name.trim(), ""));
            let contact = Contact {
                first_name: first.into(),
                last_name: last.into(),
                phone: c.contact_number,
            };
            ctx.config.contact = Some(contact.clone());
            ctx.config.save(&ctx.paths)?;
            Ok(contact)
        }
        _ => needs_input!("name and phone number needed for the order: pass --name and --phone"),
    }
}

async fn full_address(ctx: &mut Ctx) -> Result<Location> {
    let loc = ctx
        .config
        .location
        .clone()
        .ok_or_else(|| errors::input_error("no delivery address set; use `jet address`"))?;
    if !loc.street.is_empty() && !loc.city.is_empty() {
        return Ok(loc);
    }
    // Saved by an older version: look it up again for street and city.
    let fresh = crate::geo::geocode(&ctx.api, &loc.label).await?;
    ctx.config.location = Some(fresh.clone());
    ctx.config.save(&ctx.paths)?;
    Ok(fresh)
}

fn pick_slot(times: &[Slot], wanted: Option<&str>) -> Result<Option<Slot>> {
    let Some(w) = wanted.filter(|w| *w != "asap") else {
        return Ok(times.first().cloned());
    };
    let (h, m) = w
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
        .ok_or_else(|| {
            errors::input_error(format!("time '{w}' should look like 19:30 (or 'asap')"))
        })?;
    let want = h * 60 + m;
    Ok(times
        .iter()
        .find(|s| {
            chrono::DateTime::parse_from_rfc3339(&s.from)
                .map(|d| {
                    use chrono::Timelike;
                    let l = d.with_timezone(&chrono::Local);
                    l.hour() * 60 + l.minute() >= want
                })
                .unwrap_or(false)
        })
        .cloned())
}

pub fn local_time(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

/// Fills in contact, address and time, then returns the checkout as JET sees it.
pub async fn prepare(ctx: &mut Ctx, opts: Prepare) -> Result<(Current, Checkout)> {
    let current =
        Current::load(&ctx.paths)?.ok_or_else(|| errors::input_error("your basket is empty"))?;
    let c = contact(ctx, opts.contact).await?;
    let mut ops = vec![json!({ "op": "add", "path": "/customer", "value": {
        "firstName": c.first_name, "lastName": c.last_name, "phoneNumber": c.phone,
    }})];
    let delivery = current.service_type != "collection";
    if delivery {
        let loc = full_address(ctx).await?;
        ops.push(json!({ "op": "add", "path": "/fulfilment/location", "value": {
            "address": { "lines": [loc.street], "locality": loc.city, "administrativeArea": null, "postalCode": loc.postcode },
            "geolocation": { "latitude": loc.latitude, "longitude": loc.longitude },
        }}));
    }
    let want_asap = opts.time.as_deref().is_none_or(|t| t == "asap");
    ops.push(json!({ "op": "add", "path": "/fulfilment/time", "value": { "asap": want_asap, "scheduled": null } }));
    if let Some(note) = opts.note.filter(|n| !n.trim().is_empty()) {
        let path = if delivery {
            "/notes/courier"
        } else {
            "/notes/order"
        };
        ops.push(json!({ "op": "add", "path": path, "value": { "note": note } }));
    }
    let mut checkout = patch(&ctx.api, &current.basket_id, ops).await?;

    // ASAP not possible (restaurant opens later) or a specific time asked: use a slot.
    let time_issue = checkout
        .issues
        .iter()
        .any(|i| i.code.starts_with("FULFILMENT_TIME"));
    if time_issue || !want_asap {
        let times = available_times(&ctx.api, &current.basket_id).await?;
        let slot = pick_slot(&times, opts.time.as_deref())?.ok_or_else(|| {
            anyhow!(
                "{} has no delivery times left today",
                checkout.restaurant.name
            )
        })?;
        checkout = patch(
            &ctx.api,
            &current.basket_id,
            vec![json!({ "op": "add", "path": "/fulfilment/time", "value": { "asap": false, "scheduled": slot } })],
        )
        .await?;
    }
    Ok((current, checkout))
}

pub fn problems(checkout: &Checkout) -> Vec<String> {
    let mut p: Vec<String> = checkout
        .issues
        .iter()
        .map(|i| explain_issue(&i.code))
        .collect();
    if p.is_empty() && !checkout.is_fulfillable {
        p.push("JET can't accept this order right now".into());
    }
    p.dedup();
    p
}

pub fn when(checkout: &Checkout) -> String {
    match checkout.fulfilment.as_ref().and_then(|f| f.time.as_ref()) {
        Some(FulfilmentTime {
            scheduled: Some(s), ..
        }) => local_time(&s.from),
        Some(FulfilmentTime { asap: true, .. }) => "as soon as possible".into(),
        _ => "unknown".into(),
    }
}

pub fn address(checkout: &Checkout) -> Option<String> {
    let a = checkout
        .fulfilment
        .as_ref()?
        .location
        .as_ref()?
        .address
        .as_ref()?;
    let lines: Vec<&str> = a["lines"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    Some(format!(
        "{}, {} {}",
        lines.join(" "),
        a["postalCode"].as_str().unwrap_or_default(),
        a["locality"].as_str().unwrap_or_default()
    ))
}

/// A summary for people and agents.
pub fn summary(checkout: &Checkout, methods: &[PaymentType]) -> Value {
    json!({
        "restaurant": checkout.restaurant.name,
        "service": checkout.service_type,
        "items": checkout.purchase.groups.iter().flat_map(|g| &g.products).map(|p| json!({
            "quantity": p.quantity, "name": p.name, "price": p.price.amount as f64 / 100.0,
        })).collect::<Vec<_>>(),
        "costs": checkout.purchase.line_items.iter().map(|l| json!({ "name": l.name, "amount": l.price.amount as f64 / 100.0 })).collect::<Vec<_>>(),
        "total": checkout.purchase.total.price.amount as f64 / 100.0,
        "delivery_address": address(checkout),
        "time": when(checkout),
        "ready_to_order": checkout.is_fulfillable,
        "problems": problems(checkout),
        "payment_methods": methods.iter().map(|m| json!({ "id": m.payment_type, "name": m.display_name })).collect::<Vec<_>>(),
    })
}

/// Places the order with an online payment. `expected_total` (EUR) must match the current total:
/// the user confirmed that amount.
pub async fn place(
    ctx: &mut Ctx,
    method: &str,
    expected_total: f64,
    opts: Prepare,
) -> Result<(PendingOrder, PayOutcome)> {
    let (current, checkout) = prepare(ctx, opts).await?;
    let problems = problems(&checkout);
    if !problems.is_empty() {
        bail!("can't order yet: {}", problems.join("; "));
    }
    let total = checkout.purchase.total.price.amount;
    if (expected_total * 100.0).round() as i64 != total {
        return Err(errors::input_error(format!(
            "the total is € {:.2}, not € {expected_total:.2}; confirm the new total first",
            total as f64 / 100.0
        )));
    }
    let methods = payment_methods(&ctx.api, &checkout, &current.basket_id).await?;
    if !methods.iter().any(|m| m.payment_type == method) {
        return Err(errors::input_error(format!(
            "payment method '{method}' isn't available here; choose one of: {}",
            methods
                .iter()
                .map(|m| m.payment_type.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let outcome = pay(&ctx.api, &current.basket_id, total, method).await?;
    let pending = PendingOrder {
        basket_id: current.basket_id.clone(),
        restaurant: checkout.restaurant.name.clone(),
        total_cents: total,
        payment_method: method.to_string(),
        payment_url: match &outcome {
            PayOutcome::PaymentPage { url, .. } => url.clone(),
            PayOutcome::Placed { .. } => String::new(),
        },
        status_url: match &outcome {
            PayOutcome::PaymentPage { status_url, .. } => status_url.clone(),
            PayOutcome::Placed { .. } => None,
        },
        created_at: chrono::Utc::now().timestamp(),
    };
    config::write_json(&ctx.paths.pending_order_file(), &pending)?;
    Ok((pending, outcome))
}

/// Where the last placed order is: waiting for payment, or placed (then the basket is done).
pub async fn status(ctx: &mut Ctx) -> Result<Value> {
    let pending: PendingOrder = config::read_json(&ctx.paths.pending_order_file())?
        .ok_or_else(|| errors::input_error("no order placed from here yet"))?;
    let recent = orders::recent(&ctx.api, 5).await?;
    match recent
        .iter()
        .find(|o| o.basket.basket_id == pending.basket_id)
    {
        Some(o) => {
            // Paid and placed: this basket is finished.
            if Current::load(&ctx.paths)?.is_some_and(|c| c.basket_id == pending.basket_id) {
                Current::clear(&ctx.paths)?;
            }
            Ok(json!({
                "status": "placed",
                "order": o.friendly_id,
                "restaurant": o.restaurant.display_name,
                "total": o.basket.total,
                "order_status": o.status.as_ref().map(|s| &s.value),
            }))
        }
        None => Ok(json!({
            "status": "awaiting_payment",
            "restaurant": pending.restaurant,
            "total": pending.total_cents as f64 / 100.0,
            "payment_url": pending.payment_url,
            "status_url": pending.status_url,
            "note": "No order exists until the payment is completed.",
        })),
    }
}

// ---- MCP ----

pub async fn preview_json(ctx: &mut Ctx) -> Result<Value> {
    let (current, checkout) = prepare(
        ctx,
        Prepare {
            time: None,
            note: None,
            contact: None,
        },
    )
    .await?;
    let methods = payment_methods(&ctx.api, &checkout, &current.basket_id).await?;
    Ok(summary(&checkout, &methods))
}

pub async fn place_json(
    ctx: &mut Ctx,
    method: &str,
    expected_total: f64,
    note: Option<String>,
) -> Result<Value> {
    let (pending, outcome) = place(
        ctx,
        method,
        expected_total,
        Prepare {
            time: None,
            note,
            contact: None,
        },
    )
    .await?;
    Ok(match outcome {
        PayOutcome::PaymentPage { url, status_url } => json!({
            "status": "awaiting_payment",
            "payment_url": url,
            "status_url": status_url,
            "total": pending.total_cents as f64 / 100.0,
            "next": "Give the user the payment_url to pay. The order reaches the restaurant once they've paid; check with order_status.",
        }),
        PayOutcome::Placed { order_id } => json!({
            "status": "placed",
            "order_id": order_id,
            "total": pending.total_cents as f64 / 100.0,
        }),
    })
}
