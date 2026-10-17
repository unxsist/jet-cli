use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, FuzzySelect, Input, MultiSelect, Select};
use serde_json::json;

use crate::api::Api;
use crate::auth::{self, Tokens, browser};
use crate::basket::{self, Basket, Current, NewProduct};
use crate::config::{self, Config, Location, Paths};
use crate::discovery::{self, Restaurant, ServiceType};
use crate::menu::{self, Item, Menu};
use crate::orders;
use crate::ui::{self, bold, dim, euros, euros_cents};
use crate::{BasketAction, Command, Sort, service};

struct Ctx {
    paths: Paths,
    config: Config,
    api: Api,
    json: bool,
}

pub async fn run(command: Command, json: bool) -> Result<()> {
    let paths = Paths::new()?;
    let config = Config::load(&paths)?;
    let tenant = config::tenant(&config.tenant)?;
    let api = Api::new(paths.clone(), tenant)?;
    let mut ctx = Ctx {
        paths,
        config,
        api,
        json,
    };

    match command {
        Command::Login {
            email,
            manual,
            fresh,
            show_browser,
        } => login(&mut ctx, email, manual, fresh, show_browser).await,
        Command::Logout { forget_browser } => logout(&ctx, forget_browser),
        Command::Whoami => whoami(&ctx).await,
        Command::Address { query } => address(&mut ctx, &query.join(" ")).await,
        Command::Restaurants {
            query,
            all,
            pickup,
            sort,
            limit,
        } => {
            restaurants(
                &mut ctx,
                &query.join(" "),
                all,
                service(pickup),
                sort,
                limit,
            )
            .await
        }
        Command::Menu {
            restaurant,
            filter,
            pickup,
        } => show_menu(&mut ctx, restaurant, filter, pickup).await,
        Command::Add {
            item,
            restaurant,
            size,
            options,
            quantity,
            note,
            pickup,
            new,
        } => {
            let item = (!item.is_empty()).then(|| item.join(" "));
            add(
                &mut ctx,
                AddArgs {
                    item,
                    restaurant,
                    size,
                    options,
                    quantity,
                    note,
                    pickup,
                    new,
                },
            )
            .await
        }
        Command::Orders { limit } => orders(&mut ctx, limit).await,
        Command::Reorder { number, new } => reorder(&mut ctx, number, new).await,
        Command::Basket { action } => {
            basket_cmd(&mut ctx, action.unwrap_or(crate::BasketAction::Show)).await
        }
    }
}

// ---------------------------------------------------------------- auth

async fn login(
    ctx: &mut Ctx,
    email: Option<String>,
    manual: bool,
    fresh: bool,
    show_browser: bool,
) -> Result<()> {
    let tenant = ctx.api.tenant.clone();
    let browser = browser::find_browser(ctx.config.browser.as_deref());
    let tokens = if manual || browser.is_none() {
        if !manual {
            eprintln!(
                "No Chromium-based browser found (set `browser` in config.toml); using the manual flow."
            );
        }
        auth::login_manual(&ctx.api.http, &tenant, fresh).await?
    } else {
        let email: String = match (email, ctx.config.email.clone()) {
            (Some(e), _) => e,
            (None, Some(prev)) if ui::interactive() => Input::new()
                .with_prompt("Email")
                .default(prev)
                .interact_text()?,
            (None, Some(prev)) => prev,
            (None, None) => Input::new().with_prompt("Email").interact_text()?,
        };
        let email: String = email.trim().to_string();
        // A different account than last time needs a fresh login rather than the remembered session.
        let switching = ctx
            .config
            .email
            .as_deref()
            .is_some_and(|prev| !prev.eq_ignore_ascii_case(&email));
        let tokens = browser::login(
            &ctx.api.http,
            &ctx.paths,
            &tenant,
            browser.as_deref().unwrap(),
            &email,
            fresh || switching,
            show_browser,
        )
        .await?;
        ctx.config.email = Some(email);
        tokens
    };
    tokens.save(&ctx.paths)?;
    ctx.config.save(&ctx.paths)?;
    let claims = tokens.claims()?;
    eprintln!("Logged in as {}.", display_name(&claims));
    Ok(())
}

fn display_name(claims: &serde_json::Value) -> String {
    let s = |k: &str| claims[k].as_str().unwrap_or_default().to_string();
    let name = format!("{} {}", s("given_name"), s("family_name"))
        .trim()
        .to_string();
    match (name.is_empty(), s("email")) {
        (false, e) if !e.is_empty() => format!("{name} <{e}>"),
        (false, _) => name,
        (true, e) if !e.is_empty() => e,
        _ => s("sub"),
    }
}

fn logout(ctx: &Ctx, forget_browser: bool) -> Result<()> {
    if ctx.paths.auth_file().exists() {
        std::fs::remove_file(ctx.paths.auth_file())?;
    }
    if forget_browser && ctx.paths.browser_profile().exists() {
        std::fs::remove_dir_all(ctx.paths.browser_profile())?;
    }
    eprintln!("Logged out.");
    Ok(())
}

async fn whoami(ctx: &Ctx) -> Result<()> {
    ctx.api.require_token().await?;
    let tokens: Tokens = ctx.api.tokens().await.context("not logged in")?;
    let claims = tokens.claims()?;
    if ctx.json {
        return ui::print_json(&claims);
    }
    println!("{}", display_name(&claims));
    if let Some(loc) = &ctx.config.location {
        println!("{}", dim(&format!("Delivering to {}", loc.label)));
    }
    Ok(())
}

// ---------------------------------------------------------------- location

async fn address(ctx: &mut Ctx, query: &str) -> Result<()> {
    if query.trim().is_empty() {
        match &ctx.config.location {
            Some(l) if ctx.json => ui::print_json(l)?,
            Some(l) => println!("{}", l.label),
            None => {
                eprintln!("No address set. Use `jet address \"<street> <number>, <postcode>\"`.")
            }
        }
        return Ok(());
    }
    let loc = ui::with_spinner("Looking up address…", crate::geo::geocode(&ctx.api, query)).await?;
    ctx.config.location = Some(loc.clone());
    ctx.config.save(&ctx.paths)?;
    if ctx.json {
        ui::print_json(&loc)?;
    } else {
        eprintln!("Delivery address set to {}.", loc.label);
    }
    Ok(())
}

async fn location(ctx: &mut Ctx) -> Result<Location> {
    if let Some(l) = &ctx.config.location {
        return Ok(l.clone());
    }
    if !ui::interactive() {
        bail!("no delivery address set; run `jet address \"<street> <number>, <postcode>\"`");
    }
    let q: String = Input::new()
        .with_prompt("Delivery address (street + number, postcode)")
        .interact_text()?;
    address(ctx, &q).await?;
    Ok(ctx.config.location.clone().unwrap())
}

// ---------------------------------------------------------------- restaurants

async fn restaurants(
    ctx: &mut Ctx,
    query: &str,
    all: bool,
    svc: ServiceType,
    sort: Sort,
    limit: usize,
) -> Result<()> {
    let loc = location(ctx).await?;
    let mut list =
        ui::with_spinner("Finding restaurants…", discovery::near(&ctx.api, &loc, svc)).await?;
    let q = query.trim().to_lowercase();
    list.retain(|r| {
        (all || r.open)
            && (q.is_empty()
                || r.name.to_lowercase().contains(&q)
                || r.cuisines.iter().any(|c| c.to_lowercase().contains(&q)))
    });
    match sort {
        Sort::Relevance => {}
        Sort::Rating => list.sort_by(|a, b| weighted_rating(b).total_cmp(&weighted_rating(a))),
        Sort::Eta => list.sort_by_key(|r| r.eta_min.unwrap_or(u32::MAX)),
        Sort::Fee => list.sort_by_key(|r| r.delivery_fee_cents.unwrap_or(i64::MAX)),
        Sort::Distance => list.sort_by_key(|r| r.distance_m.unwrap_or(u32::MAX)),
    }
    // Stable sort: keeps the chosen order, open restaurants first.
    list.sort_by_key(|r| !r.open);
    list.truncate(limit);

    if ctx.json {
        return ui::print_json(&list);
    }
    if list.is_empty() {
        eprintln!(
            "No {}restaurants found near {}.",
            if all { "" } else { "open " },
            loc.label
        );
        return Ok(());
    }
    // Remember the listing so `jet menu 3` refers to the third restaurant shown.
    let slugs: Vec<&str> = list.iter().map(|r| r.slug.as_str()).collect();
    config::write_json(&ctx.paths.last_list_file(), &slugs)?;

    let w = ui::width();
    let num_w = list.len().to_string().len();
    let indent = " ".repeat(num_w + 2);
    let body_w = w.saturating_sub(indent.len());
    for (i, r) in list.iter().enumerate() {
        let mut name = bold(&r.name);
        if !r.open {
            name.push_str(&dim(" (closed)"));
        }
        let rating = r.rating.map(|x| format!("★ {x:.1} ({})", r.rating_count));
        let eta = match (r.eta_min, r.eta_max) {
            (Some(a), Some(b)) if a != b => Some(format!("{a}–{b} min")),
            (Some(a), _) => Some(format!("{a} min")),
            _ => None,
        };
        let fee = r.delivery_fee_cents.map(|c| match c {
            0 => "free delivery".to_string(),
            c => format!("{} delivery", euros_cents(c)),
        });
        let min = r
            .min_order_cents
            .filter(|c| *c > 0)
            .map(|c| format!("min. {}", euros_cents(c)));
        let facts: Vec<String> = [rating, eta, fee, min].into_iter().flatten().collect();
        let mut tags = vec![r.slug.clone()];
        for c in &r.cuisines {
            if !tags.contains(c) && tags.len() < 4 {
                tags.push(c.clone());
            }
        }

        println!(
            "{:>num_w$}  {}",
            dim(&(i + 1).to_string()),
            ui::fit(&name, body_w)
        );
        println!("{indent}{}", ui::fit(&facts.join(" · "), body_w));
        println!("{indent}{}", dim(&ui::fit(&tags.join(" · "), body_w)));
    }
    ui::hint(&format!(
        "Near {}. Next: jet menu <number or name>",
        loc.label
    ));
    Ok(())
}

/// Bayesian average, so 5.0 from 3 reviews doesn't outrank 4.7 from 400.
fn weighted_rating(r: &Restaurant) -> f32 {
    const PRIOR: f32 = 4.0;
    const WEIGHT: f32 = 25.0;
    let n = r.rating_count as f32;
    (PRIOR * WEIGHT + r.rating.unwrap_or(0.0) * n) / (WEIGHT + n)
}

/// Accepts a number from the last `jet restaurants` list, a slug, or a (partial) name of a restaurant near the delivery address.
async fn resolve_restaurant(ctx: &mut Ctx, query: &str, svc: ServiceType) -> Result<String> {
    let q = query.trim();
    if let Ok(n) = q.parse::<usize>() {
        let last: Vec<String> = config::read_json(&ctx.paths.last_list_file())?.unwrap_or_default();
        return last
            .get(n.wrapping_sub(1))
            .cloned()
            .with_context(|| format!("no restaurant {n} in the last `jet restaurants` list"));
    }
    let looks_like_slug = !q.contains(' ')
        && q.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let Some(loc) = ctx.config.location.clone() else {
        return Ok(q.to_string());
    };
    let list =
        ui::with_spinner("Finding restaurant…", discovery::near(&ctx.api, &loc, svc)).await?;
    if looks_like_slug && list.iter().any(|r| r.slug == q) {
        return Ok(q.to_string());
    }
    let ql = q.to_lowercase();
    let exact: Vec<&Restaurant> = list
        .iter()
        .filter(|r| r.name.to_lowercase() == ql)
        .collect();
    let matches: Vec<&Restaurant> = if exact.is_empty() {
        list.iter()
            .filter(|r| r.name.to_lowercase().contains(&ql))
            .collect()
    } else {
        exact
    };
    match matches.len() {
        0 if looks_like_slug => Ok(q.to_string()),
        0 => bail!("no restaurant matching '{q}' near {}", loc.label),
        1 => Ok(matches[0].slug.clone()),
        _ if ui::interactive() => {
            let labels: Vec<String> = matches
                .iter()
                .map(|r| {
                    format!(
                        "{}{} {}",
                        r.name,
                        if r.open { "" } else { " (closed)" },
                        dim(&r.slug)
                    )
                })
                .collect();
            let i = Select::new()
                .with_prompt("Which restaurant?")
                .items(ui::picker_labels(labels))
                .default(0)
                .interact()?;
            Ok(matches[i].slug.clone())
        }
        _ => bail!(
            "'{q}' matches several restaurants: {}",
            matches
                .iter()
                .map(|r| r.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

// ---------------------------------------------------------------- menu

async fn load_menu(ctx: &mut Ctx, restaurant: Option<String>, svc: ServiceType) -> Result<Menu> {
    let current = Current::load(&ctx.paths)?;
    let slug = match restaurant {
        Some(r) => resolve_restaurant(ctx, &r, svc).await?,
        None => match &current {
            Some(c) => c.restaurant_slug.clone(),
            None => bail!("which restaurant? Pass a slug or name (see `jet restaurants`)"),
        },
    };
    let loc = ctx.config.location.clone();
    ui::with_spinner(
        "Loading menu…",
        menu::fetch(&ctx.api, &slug, svc, loc.as_ref()),
    )
    .await
}

async fn show_menu(
    ctx: &mut Ctx,
    restaurant: Option<String>,
    filter: Option<String>,
    pickup: bool,
) -> Result<()> {
    let menu = load_menu(ctx, restaurant, service(pickup)).await?;
    let filter = filter.map(|f| f.to_lowercase());
    let matches = |i: &Item| {
        filter.as_ref().is_none_or(|f| {
            i.name.to_lowercase().contains(f) || i.description.to_lowercase().contains(f)
        })
    };
    if ctx.json {
        let items: Vec<&Item> = menu.items.iter().filter(|i| matches(i)).collect();
        let groups: Vec<_> = menu.modifier_groups.values().collect();
        return ui::print_json(&json!({
            "restaurant": { "id": menu.restaurant_id, "slug": menu.slug, "name": menu.name },
            "items": items,
            "optionGroups": groups,
        }));
    }
    // Cap the width so prices stay close to the item names on wide terminals.
    let w = ui::width().min(88);
    let num_w = menu.items.len().to_string().len();
    let indent = " ".repeat(num_w + 2);
    println!("{}", bold(&menu.name));
    for cat in &menu.categories {
        let items: Vec<&Item> = cat
            .items
            .iter()
            .map(|&i| &menu.items[i])
            .filter(|i| matches(i))
            .collect();
        if items.is_empty() {
            continue;
        }
        println!(
            "\n{}",
            console::style(ui::fit(&cat.name, w)).bold().underlined()
        );
        for item in items {
            let mut price = if item.variations.len() > 1 {
                format!("from {}", euros(item.min_price()))
            } else {
                euros(item.min_price())
            };
            if !item.available() {
                price = format!("sold out · {price}");
            }
            let left = format!("{:>num_w$}  {}", dim(&item.number.to_string()), item.name);
            println!("{}", ui::spread(&left, &dim(&price), w));
            if !item.description.is_empty() {
                let mut lines = ui::wrap(&item.description.replace('\n', " "), w, &indent);
                if lines.len() > 3 {
                    lines.truncate(3);
                    lines[2] = ui::fit(&format!("{}…", lines[2]), w);
                }
                for l in lines {
                    println!("{}", dim(&l));
                }
            }
        }
    }
    ui::hint(&format!(
        "\nAdd with: jet add -r {} <number or name>",
        menu.slug
    ));
    Ok(())
}

// ---------------------------------------------------------------- add

struct AddArgs {
    item: Option<String>,
    restaurant: Option<String>,
    size: Option<String>,
    options: Vec<String>,
    quantity: u32,
    note: String,
    pickup: bool,
    new: bool,
}

async fn add(ctx: &mut Ctx, a: AddArgs) -> Result<()> {
    let svc = match &Current::load(&ctx.paths)? {
        Some(c) if a.restaurant.is_none() && c.service_type == "collection" => {
            ServiceType::Collection
        }
        _ => service(a.pickup),
    };
    let loc = location(ctx).await?;
    let menu = load_menu(ctx, a.restaurant.clone(), svc).await?;

    let current = basket_for(ctx, &menu, a.new)?;
    let item = pick_item(&menu, a.item.as_deref())?;
    let product = configure(&menu, item, &a)?;
    let basket = put_products(
        ctx,
        &menu,
        svc,
        &loc,
        current,
        std::slice::from_ref(&product),
    )
    .await?;
    if !ctx.json {
        eprintln!("Added {}× {}.", product.quantity, item.name);
    }
    print_basket(ctx, &basket, &menu.name)
}

/// The current basket if it's at `menu`'s restaurant. A basket elsewhere is discarded, after
/// asking (or with `--new`).
fn basket_for(ctx: &Ctx, menu: &Menu, new: bool) -> Result<Option<Current>> {
    let Some(c) = Current::load(&ctx.paths)? else {
        return Ok(None);
    };
    if c.restaurant_slug == menu.slug {
        return Ok(Some(c));
    }
    let ok = new
        || (ui::interactive()
            && Confirm::new()
                .with_prompt(ui::fit(
                    &format!(
                        "Your basket is from {}. Start a new basket at {}?",
                        c.restaurant_name, menu.name
                    ),
                    ui::width().saturating_sub(4),
                ))
                .default(false)
                .interact()?);
    if !ok {
        bail!(
            "basket is from {}; pass --new to start over at {}",
            c.restaurant_name,
            menu.name
        );
    }
    Ok(None)
}

/// Adds products to `current`, creating a basket first when there is none (or it expired).
async fn put_products(
    ctx: &Ctx,
    menu: &Menu,
    svc: ServiceType,
    loc: &Location,
    mut current: Option<Current>,
    products: &[NewProduct],
) -> Result<Basket> {
    let mut basket = None;
    for product in products {
        let added = match &current {
            Some(c) => {
                match ui::with_spinner("Adding…", basket::add(&ctx.api, c, loc, product)).await {
                    Ok(b) => Some(b),
                    // Baskets expire server-side; start a fresh one transparently.
                    Err(e) if e.to_string().contains("404") => None,
                    Err(e) => return Err(e),
                }
            }
            None => None,
        };
        let b = match added {
            Some(b) => b,
            None => {
                let b = create(ctx, menu, svc, loc, product).await?;
                current = Current::load(&ctx.paths)?;
                b
            }
        };
        basket = Some(b);
    }
    basket.context("nothing to add")
}

async fn create(
    ctx: &Ctx,
    menu: &Menu,
    svc: ServiceType,
    loc: &Location,
    product: &NewProduct,
) -> Result<Basket> {
    let b = ui::with_spinner(
        "Creating basket…",
        basket::create(&ctx.api, &menu.slug, &menu.menu_group_id, svc, loc, product),
    )
    .await?;
    Current {
        basket_id: b.basket_id.clone(),
        restaurant_slug: menu.slug.clone(),
        restaurant_name: menu.name.clone(),
        service_type: svc.as_str().to_string(),
    }
    .save(&ctx.paths)?;
    Ok(b)
}

fn item_label(i: &Item) -> String {
    let price = euros(i.min_price());
    format!(
        "{:>3}. {} — {} {}",
        i.number,
        i.name,
        price,
        dim(&i.category)
    )
}

fn pick_item<'m>(menu: &'m Menu, query: Option<&str>) -> Result<&'m Item> {
    let candidates: Vec<&Item> = match query {
        Some(q) => menu.find(q),
        None => menu.items.iter().filter(|i| i.available()).collect(),
    };
    match candidates.len() {
        0 => bail!(
            "no item matching '{}' on the menu of {}",
            query.unwrap_or_default(),
            menu.name
        ),
        1 if query.is_some() => Ok(candidates[0]),
        _ if ui::interactive() => {
            let labels: Vec<String> = candidates.iter().map(|i| item_label(i)).collect();
            let i = FuzzySelect::new()
                .with_prompt("Item")
                .items(ui::picker_labels(labels))
                .default(0)
                .interact()?;
            Ok(candidates[i])
        }
        _ => bail!(
            "'{}' matches several items: {}",
            query.unwrap_or_default(),
            candidates
                .iter()
                .take(10)
                .map(|i| format!("{} ({})", i.name, i.number))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Picks size and options from flags, prompting for anything missing when interactive.
fn configure(menu: &Menu, item: &Item, a: &AddArgs) -> Result<NewProduct> {
    let variations: Vec<&menu::Variation> =
        item.variations.iter().filter(|v| v.available).collect();
    if variations.is_empty() {
        bail!("{} is sold out", item.name);
    }
    let variation = if variations.len() == 1 {
        variations[0]
    } else if let Some(size) = &a.size {
        let s = size.to_lowercase();
        let found: Vec<_> = variations
            .iter()
            .filter(|v| v.name.to_lowercase().contains(&s))
            .collect();
        match found.as_slice() {
            [v] => **v,
            _ => bail!(
                "size '{size}' doesn't match exactly one of: {}",
                variations
                    .iter()
                    .map(|v| v.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    } else if ui::interactive() {
        let labels: Vec<String> = variations
            .iter()
            .map(|v| format!("{} — {}", v.name, euros(v.price)))
            .collect();
        let i = Select::new()
            .with_prompt(ui::fit(
                &format!("{} size", item.name),
                ui::width().saturating_sub(4),
            ))
            .items(ui::picker_labels(labels))
            .default(0)
            .interact()?;
        variations[i]
    } else {
        bail!(
            "{} comes in several sizes; pass --size ({})",
            item.name,
            variations
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    if variation.is_deal {
        bail!("{} is a combo deal; deals aren't supported yet", item.name);
    }

    let wanted: Vec<String> = a.options.iter().map(|o| o.to_lowercase()).collect();
    let mut used = vec![false; wanted.len()];
    let mut modifiers = Vec::new();
    for gid in &variation.modifier_groups {
        let group = menu.group(gid)?;
        let available: Vec<&menu::ModifierOption> =
            group.options.iter().filter(|o| o.available).collect();
        // Flags: exact name match first, then substring.
        let mut chosen: Vec<&menu::ModifierOption> = Vec::new();
        for (wi, w) in wanted.iter().enumerate() {
            let hit = available
                .iter()
                .find(|o| o.name.to_lowercase() == *w)
                .or_else(|| {
                    available
                        .iter()
                        .find(|o| o.name.to_lowercase().contains(w.as_str()))
                });
            if let Some(o) = hit
                && !chosen.iter().any(|c| c.id == o.id)
            {
                chosen.push(o);
                used[wi] = true;
            }
        }
        let needs_prompt = chosen.is_empty() && (group.min > 0 || a.options.is_empty());
        if needs_prompt && ui::interactive() && !available.is_empty() {
            chosen = prompt_group(&item.name, group, &available)?;
        } else if chosen.is_empty() {
            chosen = available.iter().filter(|o| o.default).copied().collect();
        }
        let n = chosen.len() as u32;
        if n < group.min || n > group.max {
            bail!(
                "{}: choose {} for '{}' with -o ({})",
                item.name,
                range_text(group.min, group.max),
                group.name,
                available
                    .iter()
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !chosen.is_empty() {
            modifiers.push((
                group.id.clone(),
                chosen.iter().map(|o| o.id.clone()).collect(),
            ));
        }
    }
    if let Some(i) = used.iter().position(|u| !u) {
        bail!("option '{}' doesn't exist for {}", a.options[i], item.name);
    }
    Ok(NewProduct {
        variation_id: variation.id.clone(),
        quantity: a.quantity.max(1),
        modifiers,
        note: a.note.clone(),
    })
}

fn range_text(min: u32, max: u32) -> String {
    match (min, max) {
        (a, b) if a == b => format!("exactly {a}"),
        (0, b) => format!("up to {b}"),
        (a, b) => format!("{a} to {b}"),
    }
}

fn prompt_group<'g>(
    item: &str,
    group: &menu::ModifierGroup,
    options: &[&'g menu::ModifierOption],
) -> Result<Vec<&'g menu::ModifierOption>> {
    let label = |o: &menu::ModifierOption| {
        if o.price > 0.0 {
            format!("{} (+{})", o.name, euros(o.price))
        } else {
            o.name.clone()
        }
    };
    let labels: Vec<String> = options.iter().map(|o| label(o)).collect();
    if group.min == 1 && group.max == 1 {
        let default = options.iter().position(|o| o.default).unwrap_or(0);
        let i = Select::new()
            .with_prompt(ui::fit(
                &format!("{item}: {}", group.name),
                ui::width().saturating_sub(4),
            ))
            .items(ui::picker_labels(labels))
            .default(default)
            .interact()?;
        return Ok(vec![options[i]]);
    }
    loop {
        let defaults: Vec<bool> = options.iter().map(|o| o.default).collect();
        let prompt = ui::fit(
            &format!(
                "{item}: {} ({}, space to toggle)",
                group.name,
                range_text(group.min, group.max)
            ),
            ui::width().saturating_sub(4),
        );
        let picked = MultiSelect::new()
            .with_prompt(prompt)
            .items(ui::picker_labels(labels.clone()))
            .defaults(&defaults)
            .interact()?;
        let n = picked.len() as u32;
        if n >= group.min && n <= group.max {
            return Ok(picked.into_iter().map(|i| options[i]).collect());
        }
        eprintln!("Please choose {}.", range_text(group.min, group.max));
    }
}

// ---------------------------------------------------------------- orders

async fn orders(ctx: &mut Ctx, limit: usize) -> Result<()> {
    let list = ui::with_spinner("Loading orders…", orders::recent(&ctx.api, limit)).await?;
    config::write_json(&ctx.paths.last_orders_file(), &list)?;
    if ctx.json {
        return ui::print_json(&list);
    }
    if list.is_empty() {
        eprintln!("No orders yet.");
        return Ok(());
    }
    let w = ui::width().min(88);
    let num_w = list.len().to_string().len();
    let indent = " ".repeat(num_w + 2);
    for (i, o) in list.iter().enumerate() {
        let date = chrono::DateTime::parse_from_rfc3339(&o.information.created_at)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%a %e %b %Y")
                    .to_string()
            })
            .unwrap_or_else(|_| o.information.created_at.chars().take(10).collect());
        let left = format!(
            "{:>num_w$}  {}",
            dim(&(i + 1).to_string()),
            bold(&o.restaurant.display_name)
        );
        println!("{}", ui::spread(&left, &euros(o.basket.total), w));
        let mut facts = vec![date.trim().to_string()];
        if o.is_pickup() {
            facts.push("pickup".into());
        }
        if let Some(st) = o.status.as_ref().filter(|s| s.value != "Completed") {
            facts.push(st.value.clone());
        }
        println!(
            "{indent}{}",
            dim(&ui::fit(&facts.join(" · "), w.saturating_sub(indent.len())))
        );
        let items: Vec<String> = o
            .basket
            .summary
            .items
            .iter()
            .map(|it| format!("{}× {}", it.quantity, it.name))
            .collect();
        for l in ui::wrap(&items.join(", "), w, &indent) {
            println!("{l}");
        }
    }
    ui::hint("Put an order back in your basket with: jet reorder <number>");
    Ok(())
}

async fn reorder(ctx: &mut Ctx, number: usize, new: bool) -> Result<()> {
    let cached: Option<Vec<orders::Order>> = config::read_json(&ctx.paths.last_orders_file())?;
    let list = match cached {
        Some(l) if l.len() >= number => l,
        _ => ui::with_spinner("Loading orders…", orders::recent(&ctx.api, number.max(20))).await?,
    };
    let order = list
        .get(number.wrapping_sub(1))
        .with_context(|| format!("there's no order {number}; see `jet orders`"))?
        .clone();
    let svc = service(order.is_pickup());
    let loc = location(ctx).await?;
    let menu = load_menu(ctx, Some(order.restaurant.seo_name.clone()), svc).await?;

    let mut products = Vec::new();
    for it in &order.basket.summary.items {
        match product_from_order(&menu, it) {
            Ok(p) => products.push(p),
            Err(e) => eprintln!("Skipping {}: {e:#}", it.name),
        }
    }
    if products.is_empty() {
        bail!(
            "none of the items from that order are available at {} right now",
            menu.name
        );
    }
    let current = basket_for(ctx, &menu, new)?;
    let basket = put_products(ctx, &menu, svc, &loc, current, &products).await?;
    if !ctx.json {
        eprintln!(
            "Added {} of {} items from your order of {}.",
            products.len(),
            order.basket.summary.items.len(),
            order.restaurant.display_name
        );
    }
    print_basket(ctx, &basket, &menu.name)
}

/// Rebuilds an ordered item against today's menu (ids are stable; availability may not be).
fn product_from_order(menu: &Menu, it: &orders::OrderItem) -> Result<NewProduct> {
    let variation = menu
        .items
        .iter()
        .flat_map(|i| i.variations.iter())
        .find(|v| v.id == it.id)
        .context("no longer on the menu")?;
    if !variation.available {
        bail!("sold out");
    }
    let mut modifiers: Vec<(String, Vec<String>)> = Vec::new();
    for acc in &it.accessories {
        let group = variation
            .modifier_groups
            .iter()
            .filter_map(|g| menu.group(g).ok())
            .find(|g| {
                g.options
                    .iter()
                    .any(|o| o.id == acc.accessory_id && o.available)
            })
            .with_context(|| format!("option {} is no longer available", acc.name))?;
        match modifiers.iter_mut().find(|(g, _)| *g == group.id) {
            Some((_, ids)) => ids.push(acc.accessory_id.clone()),
            None => modifiers.push((group.id.clone(), vec![acc.accessory_id.clone()])),
        }
    }
    Ok(NewProduct {
        variation_id: variation.id.clone(),
        quantity: it.quantity.max(1),
        modifiers,
        note: it.customer_notes.clone(),
    })
}

// ---------------------------------------------------------------- basket

async fn basket_cmd(ctx: &mut Ctx, action: BasketAction) -> Result<()> {
    let Some(current) = Current::load(&ctx.paths)? else {
        if ctx.json {
            return ui::print_json(&serde_json::Value::Null);
        }
        eprintln!("Your basket is empty. Find something with `jet restaurants`.");
        return Ok(());
    };
    let fetched =
        match ui::with_spinner("Loading basket…", basket::get(&ctx.api, &current.basket_id)).await
        {
            Ok(b) => b,
            Err(e) if e.to_string().contains("404") => {
                Current::clear(&ctx.paths)?;
                bail!("your basket expired; start a new one with `jet add`");
            }
            Err(e) => return Err(e),
        };
    let line = |n: usize| -> Result<&basket::Product> {
        fetched
            .basket_summary
            .products
            .get(n.wrapping_sub(1))
            .with_context(|| format!("there's no line {n} in the basket"))
    };
    let updated = match action {
        BasketAction::Show => fetched.clone(),
        BasketAction::Rm { line: n } => {
            basket::remove(&ctx.api, &current.basket_id, line(n)?).await?
        }
        BasketAction::Qty {
            line: n,
            quantity: 0,
        } => basket::remove(&ctx.api, &current.basket_id, line(n)?).await?,
        BasketAction::Qty { line: n, quantity } => {
            basket::set_quantity(&ctx.api, &current.basket_id, line(n)?, quantity).await?
        }
        BasketAction::Clear => {
            Current::clear(&ctx.paths)?;
            if !ctx.json {
                eprintln!("Basket cleared.");
            }
            return Ok(());
        }
    };
    if updated.basket_summary.products.is_empty() {
        Current::clear(&ctx.paths)?;
    }
    print_basket(ctx, &updated, &current.restaurant_name)
}

fn print_basket(ctx: &Ctx, b: &Basket, restaurant: &str) -> Result<()> {
    if ctx.json {
        return ui::print_json(b);
    }
    let s = &b.basket_summary;
    if s.products.is_empty() {
        println!("Your basket is empty.");
        return Ok(());
    }
    let kind = if b.service_type.eq_ignore_ascii_case("collection") {
        "pickup"
    } else {
        "delivery"
    };
    let w = ui::width().min(64);
    let num_w = s.products.len().to_string().len();
    let indent = " ".repeat(num_w + 2);
    println!(
        "{} {}",
        bold(&ui::fit(restaurant, w.saturating_sub(12))),
        dim(&format!("· {kind}"))
    );
    println!();
    for (i, p) in s.products.iter().enumerate() {
        let mut name = format!("{}× {}", p.quantity, p.name);
        if !p.variation.is_empty() {
            name.push_str(&format!(" · {}", p.variation));
        }
        let left = format!("{:>num_w$}  {name}", dim(&(i + 1).to_string()));
        println!("{}", ui::spread(&left, &euros(p.total_price), w));
        let opts = p.options();
        if !opts.is_empty() {
            for l in ui::wrap(&opts.join(", "), w, &indent) {
                println!("{}", dim(&l));
            }
        }
        if !p.customer_notes.is_empty() {
            for l in ui::wrap(&format!("note: {}", p.customer_notes), w, &indent) {
                println!("{}", dim(&l));
            }
        }
    }
    println!("{}", dim(&"─".repeat(w)));
    let row = |label: &str, amount: f64| {
        println!(
            "{}",
            ui::spread(&format!("{indent}{label}"), &euros(amount), w)
        )
    };
    row("Subtotal", s.basket_totals.sub_total);
    if kind == "delivery" {
        row("Delivery", s.delivery_charge);
    }
    for adj in &s.adjustments {
        row(&adj.name, adj.adjustment);
    }
    println!(
        "{}",
        bold(&ui::spread(
            &format!("{indent}Total"),
            &euros(s.basket_totals.total),
            w
        ))
    );
    if let Some(sm) = s
        .prompts
        .as_ref()
        .and_then(|p| p.spend_more.as_ref())
        .filter(|sm| sm.to_spend > 0.0)
    {
        eprintln!();
        ui::hint(&format!(
            "Add {} more to reach the {} minimum order.",
            euros(sm.to_spend),
            euros(sm.minimum_order_amount)
        ));
    }
    Ok(())
}
