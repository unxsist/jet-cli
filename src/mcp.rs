//! `jet mcp`: a Model Context Protocol server over stdio, so AI agents can use jet as tools.
//!
//! Stdout carries only JSON-RPC; nothing else may print there.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::auth::webview::LoginSession;
use crate::basket::Basket;
use crate::commands::{self, AddArgs, Ctx};
use crate::discovery::ServiceType;
use crate::errors::{JetError, Kind};
use crate::menu::{Item, Menu};
use crate::{BasketAction, Sort, agent_login, checkout, orders, ui};

const INSTRUCTIONS: &str = "\
Order food from Just Eat Takeaway (thuisbezorgd.nl) for the user.

Typical flow: get_status -> (set_delivery_address) -> search_restaurants -> get_menu -> \
get_menu_item (for sizes/options) -> add_to_basket -> view_basket -> checkout_preview -> \
place_order.

Login: if a tool reports auth_required, ask the user for their email (or use the one from \
get_status), call login_start, ask the user for the code they received by email, then call \
login_complete. A small login window may appear on the user's screen for a few seconds.

Ordering costs the user money. Before place_order, show the user the restaurant, items, \
delivery address and the total from checkout_preview and get an explicit yes. Pass that total \
as expected_total. For online payment, place_order returns a payment_url: give it to the user \
to pay; the order goes to the restaurant once they've paid.";

fn tools() -> Value {
    let s = |props: Value, required: &[&str]| json!({ "type": "object", "properties": props, "required": required });
    json!([
        { "name": "get_status", "description": "Login state, account, delivery address and the current basket.",
          "inputSchema": s(json!({}), &[]) },
        { "name": "login_start", "description": "Start logging in: JET emails the user a login code. Then ask the user for the code and call login_complete.",
          "inputSchema": s(json!({ "email": { "type": "string", "description": "Defaults to the last email used." } }), &[]) },
        { "name": "login_complete", "description": "Finish a login with the code from the user's email.",
          "inputSchema": s(json!({ "code": { "type": "string" } }), &["code"]) },
        { "name": "set_delivery_address", "description": "Set the delivery address (street + number, postcode or city).",
          "inputSchema": s(json!({ "address": { "type": "string" } }), &["address"]) },
        { "name": "search_restaurants", "description": "Restaurants that deliver to the delivery address, open ones first.",
          "inputSchema": s(json!({
              "query": { "type": "string", "description": "Name or cuisine, e.g. 'pizza'." },
              "sort": { "type": "string", "enum": ["relevance", "rating", "eta", "fee", "distance"] },
              "limit": { "type": "integer", "default": 10 },
              "pickup": { "type": "boolean", "description": "Pickup instead of delivery." },
              "include_closed": { "type": "boolean" }
          }), &[]) },
        { "name": "get_menu", "description": "A restaurant's menu: numbered items with prices. Use filter to narrow it down.",
          "inputSchema": s(json!({
              "restaurant": { "type": "string", "description": "Slug from search_restaurants, or name." },
              "filter": { "type": "string" },
              "pickup": { "type": "boolean" }
          }), &["restaurant"]) },
        { "name": "get_menu_item", "description": "Sizes and option groups (with required counts and prices) of one menu item.",
          "inputSchema": s(json!({
              "restaurant": { "type": "string" },
              "item": { "type": "string", "description": "Item number from get_menu, or name." }
          }), &["restaurant", "item"]) },
        { "name": "add_to_basket", "description": "Add an item. Required option groups must be filled via options (option names).",
          "inputSchema": s(json!({
              "restaurant": { "type": "string", "description": "Defaults to the current basket's restaurant." },
              "item": { "type": "string", "description": "Item number or name." },
              "size": { "type": "string" },
              "options": { "type": "array", "items": { "type": "string" } },
              "quantity": { "type": "integer", "default": 1 },
              "note": { "type": "string", "description": "Note for the restaurant about this item." },
              "pickup": { "type": "boolean" },
              "replace_basket": { "type": "boolean", "description": "Discard a basket at another restaurant." }
          }), &["item"]) },
        { "name": "view_basket", "description": "The current basket with totals.", "inputSchema": s(json!({}), &[]) },
        { "name": "update_basket", "description": "Change the quantity of a basket line (0 removes it).",
          "inputSchema": s(json!({ "line": { "type": "integer" }, "quantity": { "type": "integer" } }), &["line", "quantity"]) },
        { "name": "clear_basket", "description": "Empty the basket.", "inputSchema": s(json!({}), &[]) },
        { "name": "list_orders", "description": "The user's recent orders (1 = most recent).",
          "inputSchema": s(json!({ "limit": { "type": "integer", "default": 10 } }), &[]) },
        { "name": "reorder", "description": "Put a previous order's items back in the basket.",
          "inputSchema": s(json!({
              "order_number": { "type": "integer", "description": "From list_orders; 1 = most recent." },
              "replace_basket": { "type": "boolean" }
          }), &["order_number"]) },
        { "name": "checkout_preview", "description": "What placing the order would do: address, delivery time, total, payment methods and any problems (minimum order, closed, ...).",
          "inputSchema": s(json!({}), &[]) },
        { "name": "place_order", "description": "Place the order. Only after the user explicitly confirmed the total. Online payment returns a payment_url for the user to finish paying.",
          "inputSchema": s(json!({
              "payment_method": { "type": "string", "description": "A method id from checkout_preview, e.g. 'ideal', 'creditcard', 'paypal_rest', 'cash'." },
              "expected_total": { "type": "number", "description": "The total (EUR) the user confirmed; the order is refused if it differs." },
              "note": { "type": "string", "description": "Note for the restaurant/courier." }
          }), &["payment_method", "expected_total"]) }
    ])
}

/// State that lives across tool calls.
struct Server {
    login: Option<(LoginSession, String)>,
}

pub async fn serve() -> Result<()> {
    ui::set_quiet();
    let mut server = Server { login: None };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        }; // notification
        let method = msg["method"].as_str().unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "jet", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }}),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = match server.call(&name, &args).await {
                    Ok(v) => json!({
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v)? }],
                        "structuredContent": if v.is_object() { v.clone() } else { json!({ "result": v }) },
                    }),
                    Err(e) => {
                        let kind = e.downcast_ref::<JetError>().map(|j| j.kind);
                        let hint = match kind {
                            Some(Kind::AuthRequired) => {
                                " Ask the user to log in: login_start, then login_complete with the emailed code."
                            }
                            _ => "",
                        };
                        json!({
                            "content": [{ "type": "text", "text": format!("{}: {e:#}.{hint}", kind.map_or("error", |k| k.name())) }],
                            "isError": true,
                        })
                    }
                };
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            _ => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unknown method {method}") } })
            }
        };
        stdout.write_all(format!("{reply}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

fn bool_arg(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn int_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn service(args: &Value) -> ServiceType {
    crate::service(bool_arg(args, "pickup"))
}

pub fn basket_summary(b: &Basket, restaurant: &str) -> Value {
    let s = &b.basket_summary;
    json!({
        "restaurant": restaurant,
        "service": if b.service_type.eq_ignore_ascii_case("collection") { "pickup" } else { "delivery" },
        "lines": s.products.iter().enumerate().map(|(i, p)| json!({
            "line": i + 1,
            "quantity": p.quantity,
            "name": p.name,
            "size": p.variation,
            "options": p.options(),
            "note": p.customer_notes,
            "price": p.total_price,
        })).collect::<Vec<_>>(),
        "subtotal": s.basket_totals.sub_total,
        "delivery": s.delivery_charge,
        "fees": s.adjustments.iter().map(|a| json!({ "name": a.name, "amount": a.adjustment })).collect::<Vec<_>>(),
        "total": s.basket_totals.total,
        "minimum_order_remaining": s.prompts.as_ref().and_then(|p| p.spend_more.as_ref()).map(|m| m.to_spend).filter(|v| *v > 0.0),
    })
}

fn menu_overview(menu: &Menu, filter: Option<&str>) -> Value {
    let f = filter.map(str::to_lowercase);
    let keep = |i: &Item| {
        f.as_ref().is_none_or(|f| {
            i.name.to_lowercase().contains(f)
                || i.description.to_lowercase().contains(f)
                || i.category.to_lowercase().contains(f)
        })
    };
    let categories: Vec<Value> = menu
        .categories
        .iter()
        .filter_map(|c| {
            let items: Vec<Value> = c
                .items
                .iter()
                .map(|&i| &menu.items[i])
                .filter(|i| keep(i))
                .map(|i| {
                    json!({
                        "number": i.number,
                        "name": i.name,
                        "description": i.description,
                        "price": i.min_price(),
                        "sizes": if i.variations.len() > 1 { json!(i.variations.iter().map(|v| json!({ "name": v.name, "price": v.price })).collect::<Vec<_>>()) } else { Value::Null },
                        "has_options": i.variations.iter().any(|v| !v.modifier_groups.is_empty()),
                        "available": i.available(),
                    })
                })
                .collect();
            (!items.is_empty()).then(|| json!({ "category": c.name, "items": items }))
        })
        .collect();
    json!({ "restaurant": menu.name, "slug": menu.slug, "categories": categories })
}

fn item_details(menu: &Menu, item: &Item) -> Result<Value> {
    let variations = item
        .variations
        .iter()
        .map(|v| {
            let groups = v
                .modifier_groups
                .iter()
                .map(|g| {
                    let g = menu.group(g)?;
                    Ok(json!({
                        "group": g.name,
                        "min": g.min,
                        "max": g.max,
                        "options": g.options.iter().filter(|o| o.available).map(|o| json!({ "name": o.name, "price": o.price, "default": o.default })).collect::<Vec<_>>(),
                    }))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({ "size": v.name, "price": v.price, "available": v.available, "option_groups": groups }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(
        json!({ "number": item.number, "name": item.name, "description": item.description, "sizes": variations }),
    )
}

impl Server {
    async fn call(&mut self, name: &str, args: &Value) -> Result<Value> {
        // Fresh state each call: the CLI may have changed config, tokens or basket meanwhile.
        let mut ctx = Ctx::new(true)?;
        match name {
            "get_status" => {
                let account = match ctx.api.token().await {
                    Ok(Some(_)) => ctx
                        .api
                        .tokens()
                        .await
                        .and_then(|t| t.claims().ok())
                        .map(|c| commands::display_name(&c)),
                    _ => None,
                };
                let basket = commands::basket_core(&mut ctx, BasketAction::Show)
                    .await
                    .ok()
                    .flatten()
                    .map(|(b, r)| basket_summary(&b, &r));
                Ok(json!({
                    "logged_in": account.is_some(),
                    "account": account,
                    "email": ctx.config.email,
                    "delivery_address": ctx.config.location.as_ref().map(|l| &l.label),
                    "basket": basket,
                }))
            }
            "login_start" => {
                let email = agent_login::pick_email(&ctx.config, str_arg(args, "email"))?;
                let opts = agent_login::login_options(&ctx.config, &email, false, false);
                let mut session = LoginSession::start(
                    ctx.api.http.clone(),
                    ctx.paths.clone(),
                    ctx.api.tenant.clone(),
                    email.clone(),
                    opts,
                );
                match session.until_code_sent().await? {
                    None => {
                        self.login = Some((session, email.clone()));
                        Ok(
                            json!({ "status": "code_sent", "email": email, "next": "Ask the user for the code from the email, then call login_complete." }),
                        )
                    }
                    Some(outcome) => {
                        let name = agent_login::record_success(
                            &ctx.paths,
                            &mut ctx.config,
                            &email,
                            &outcome,
                        )?;
                        Ok(json!({ "status": "logged_in", "account": name }))
                    }
                }
            }
            "login_complete" => {
                let code = str_arg(args, "code").context("code is required")?;
                let (mut session, email) = self.login.take().ok_or_else(|| {
                    crate::errors::input_error("no login in progress; call login_start first")
                })?;
                session.submit_code(&code);
                match session.until_code_checked().await? {
                    Ok(outcome) => {
                        let name = agent_login::record_success(
                            &ctx.paths,
                            &mut ctx.config,
                            &email,
                            &outcome,
                        )?;
                        Ok(json!({ "status": "logged_in", "account": name }))
                    }
                    Err(message) => {
                        self.login = Some((session, email));
                        Err(crate::errors::input_error(format!(
                            "that code didn't work ({message}); ask the user to check it and call login_complete again"
                        )))
                    }
                }
            }
            "set_delivery_address" => {
                let query = str_arg(args, "address").context("address is required")?;
                let loc = crate::geo::geocode(&ctx.api, &query).await?;
                ctx.config.location = Some(loc.clone());
                ctx.config.save(&ctx.paths)?;
                Ok(json!({ "delivery_address": loc.label }))
            }
            "search_restaurants" => {
                let sort = match str_arg(args, "sort").as_deref() {
                    Some("rating") => Sort::Rating,
                    Some("eta") => Sort::Eta,
                    Some("fee") => Sort::Fee,
                    Some("distance") => Sort::Distance,
                    _ => Sort::Relevance,
                };
                let limit = int_arg(args, "limit").unwrap_or(10) as usize;
                let query = str_arg(args, "query").unwrap_or_default();
                let (loc, list) = commands::find_restaurants(
                    &mut ctx,
                    &query,
                    bool_arg(args, "include_closed"),
                    service(args),
                    sort,
                    limit,
                )
                .await?;
                Ok(json!({ "near": loc.label, "restaurants": list }))
            }
            "get_menu" => {
                let menu =
                    commands::load_menu(&mut ctx, str_arg(args, "restaurant"), service(args))
                        .await?;
                Ok(menu_overview(&menu, str_arg(args, "filter").as_deref()))
            }
            "get_menu_item" => {
                let menu =
                    commands::load_menu(&mut ctx, str_arg(args, "restaurant"), service(args))
                        .await?;
                let query = str_arg(args, "item").context("item is required")?;
                let found = menu.find(&query);
                match found.as_slice() {
                    [item] => item_details(&menu, item),
                    [] => Err(crate::errors::input_error(format!(
                        "no item '{query}' on the menu"
                    ))),
                    many => Err(crate::errors::input_error(format!(
                        "'{query}' matches several items: {}",
                        many.iter()
                            .take(15)
                            .map(|i| format!("{} ({})", i.name, i.number))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))),
                }
            }
            "add_to_basket" => {
                let a = AddArgs {
                    item: str_arg(args, "item"),
                    restaurant: str_arg(args, "restaurant"),
                    size: str_arg(args, "size"),
                    options: args
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    quantity: int_arg(args, "quantity").unwrap_or(1) as u32,
                    note: str_arg(args, "note").unwrap_or_default(),
                    pickup: bool_arg(args, "pickup"),
                    new: bool_arg(args, "replace_basket"),
                };
                let (basket, restaurant, added) = commands::add_core(&mut ctx, a).await?;
                Ok(json!({ "added": added, "basket": basket_summary(&basket, &restaurant) }))
            }
            "view_basket" => Ok(basket_value(
                commands::basket_core(&mut ctx, BasketAction::Show).await?,
            )),
            "update_basket" => {
                let line = int_arg(args, "line").context("line is required")? as usize;
                let quantity = int_arg(args, "quantity").context("quantity is required")? as u32;
                Ok(basket_value(
                    commands::basket_core(&mut ctx, BasketAction::Qty { line, quantity }).await?,
                ))
            }
            "clear_basket" => {
                commands::basket_core(&mut ctx, BasketAction::Clear).await?;
                Ok(json!({ "basket": null }))
            }
            "list_orders" => {
                let limit = int_arg(args, "limit").unwrap_or(10) as usize;
                let list = orders::recent(&ctx.api, limit).await?;
                crate::config::write_json(&ctx.paths.last_orders_file(), &list)?;
                Ok(
                    json!({ "orders": list.iter().enumerate().map(|(i, o)| json!({
                    "number": i + 1,
                    "date": o.information.created_at,
                    "restaurant": o.restaurant.display_name,
                    "total": o.basket.total,
                    "status": o.status.as_ref().map(|s| &s.value),
                    "items": o.basket.summary.items.iter().map(|it| format!("{}× {}", it.quantity, it.name)).collect::<Vec<_>>(),
                })).collect::<Vec<_>>() }),
                )
            }
            "reorder" => {
                let n = int_arg(args, "order_number").context("order_number is required")? as usize;
                let (basket, restaurant, summary) =
                    commands::reorder_core(&mut ctx, n, bool_arg(args, "replace_basket")).await?;
                Ok(json!({ "summary": summary, "basket": basket_summary(&basket, &restaurant) }))
            }
            "checkout_preview" => checkout::preview_json(&mut ctx).await,
            "place_order" => {
                let method =
                    str_arg(args, "payment_method").context("payment_method is required")?;
                let expected = args
                    .get("expected_total")
                    .and_then(Value::as_f64)
                    .context("expected_total is required")?;
                checkout::place_json(&mut ctx, &method, expected, str_arg(args, "note")).await
            }
            other => Err(anyhow!("unknown tool {other}")),
        }
    }
}

fn basket_value(b: Option<(Basket, String)>) -> Value {
    match b {
        Some((b, r)) => json!({ "basket": basket_summary(&b, &r) }),
        None => json!({ "basket": null }),
    }
}
