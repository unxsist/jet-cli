use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::api::{Api, MENU_CDN, REST};
use crate::config::Location;
use crate::discovery::ServiceType;

// ---- CDN payloads (PascalCase) ----

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Manifest {
    restaurant_id: String,
    restaurant_info: RestaurantInfo,
    menus: Vec<RawMenu>,
    items_url: String,
    item_details_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RestaurantInfo {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawMenu {
    menu_group_id: String,
    service_types: Vec<String>,
    categories: Vec<RawCategory>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawCategory {
    name: String,
    item_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsFile {
    items: Vec<RawItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawItem {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    variations: Vec<RawVariation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawVariation {
    id: String,
    #[serde(default)]
    name: Option<String>,
    base_price: f64,
    #[serde(default)]
    deal_only: bool,
    #[serde(default)]
    menu_group_ids: Vec<String>,
    #[serde(default)]
    modifier_groups_ids: Option<Vec<String>>,
    #[serde(default)]
    deal_groups_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DetailsFile {
    #[serde(default)]
    modifier_groups: Vec<RawModifierGroup>,
    #[serde(default)]
    modifier_sets: Vec<RawModifierSet>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawModifierGroup {
    id: String,
    name: String,
    min_choices: u32,
    max_choices: u32,
    modifiers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawModifierSet {
    id: String,
    modifier: RawModifier,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawModifier {
    id: String,
    name: String,
    #[serde(default)]
    addition_price: f64,
    #[serde(default)]
    default_choices: u32,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct Dynamic {
    offline_variation_ids: Vec<String>,
    offline_modifier_ids: Vec<String>,
}

// ---- Our model ----

#[derive(Debug, Clone, Serialize)]
pub struct Menu {
    pub restaurant_id: String,
    pub slug: String,
    pub name: String,
    pub menu_group_id: String,
    pub categories: Vec<Category>,
    pub items: Vec<Item>,
    #[serde(skip)]
    pub modifier_groups: HashMap<String, ModifierGroup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Category {
    pub name: String,
    /// Indexes into `Menu::items`.
    pub items: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    /// 1-based number shown in `jet menu`, usable as a reference in `jet add`.
    pub number: usize,
    pub id: String,
    pub name: String,
    pub category: String,
    pub description: String,
    pub variations: Vec<Variation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Variation {
    pub id: String,
    pub name: String,
    pub price: f64,
    pub available: bool,
    pub modifier_groups: Vec<String>,
    /// Combo deals use a different basket payload that isn't supported yet.
    pub is_deal: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModifierGroup {
    pub id: String,
    pub name: String,
    pub min: u32,
    pub max: u32,
    pub options: Vec<ModifierOption>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModifierOption {
    pub id: String,
    pub name: String,
    pub price: f64,
    pub default: bool,
    pub available: bool,
}

impl Item {
    pub fn min_price(&self) -> f64 {
        self.variations
            .iter()
            .map(|v| v.price)
            .fold(f64::INFINITY, f64::min)
    }
    pub fn available(&self) -> bool {
        self.variations.iter().any(|v| v.available)
    }
}

async fn cdn<T: for<'de> Deserialize<'de>>(api: &Api, file: &str) -> Result<T> {
    api.send(api.request(Method::GET, &format!("{MENU_CDN}/{file}")))
        .await
}

pub async fn fetch(
    api: &Api,
    slug: &str,
    service: ServiceType,
    loc: Option<&Location>,
) -> Result<Menu> {
    let t = &api.tenant.code;
    let manifest: Manifest = cdn(api, &format!("{slug}_{t}_manifest.json"))
        .await
        .with_context(|| {
            format!("no menu found for '{slug}' (use the slug from `jet restaurants`)")
        })?;
    let (items, details, dynamic) = tokio::join!(
        cdn::<ItemsFile>(api, &manifest.items_url),
        cdn::<DetailsFile>(api, &manifest.item_details_url),
        fetch_dynamic(api, slug, loc),
    );
    let (items, details) = (items?, details?);
    let offline_vars: HashSet<String> = dynamic.offline_variation_ids.into_iter().collect();
    let offline_mods: HashSet<String> = dynamic.offline_modifier_ids.into_iter().collect();

    let menu = manifest
        .menus
        .iter()
        .find(|m| {
            m.service_types
                .iter()
                .any(|s| s.eq_ignore_ascii_case(service.as_str()))
        })
        .with_context(|| {
            format!(
                "{} doesn't offer {}",
                manifest.restaurant_info.name,
                service.as_str()
            )
        })?;

    let sets: HashMap<&str, &RawModifier> = details
        .modifier_sets
        .iter()
        .map(|s| (s.id.as_str(), &s.modifier))
        .collect();
    let modifier_groups = details
        .modifier_groups
        .iter()
        .map(|g| {
            let options = g
                .modifiers
                .iter()
                .filter_map(|id| sets.get(id.as_str()))
                .map(|m| ModifierOption {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    price: m.addition_price,
                    default: m.default_choices > 0,
                    available: !offline_mods.contains(&m.id),
                })
                .collect();
            (
                g.id.clone(),
                ModifierGroup {
                    id: g.id.clone(),
                    name: g.name.clone(),
                    min: g.min_choices,
                    max: g.max_choices,
                    options,
                },
            )
        })
        .collect();

    let raw_items: HashMap<&str, &RawItem> =
        items.items.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut out_items = Vec::new();
    let mut categories = Vec::new();
    for cat in &menu.categories {
        let mut idxs = Vec::new();
        for id in &cat.item_ids {
            let Some(raw) = raw_items.get(id.as_str()) else {
                continue;
            };
            let variations: Vec<Variation> = raw
                .variations
                .iter()
                .filter(|v| {
                    !v.deal_only
                        && (v.menu_group_ids.is_empty()
                            || v.menu_group_ids.contains(&menu.menu_group_id))
                })
                .map(|v| Variation {
                    id: v.id.clone(),
                    name: v.name.clone().unwrap_or_default(),
                    price: v.base_price,
                    available: !offline_vars.contains(&v.id),
                    modifier_groups: v.modifier_groups_ids.clone().unwrap_or_default(),
                    is_deal: v.deal_groups_ids.as_ref().is_some_and(|d| !d.is_empty()),
                })
                .collect();
            if variations.is_empty() {
                continue;
            }
            idxs.push(out_items.len());
            out_items.push(Item {
                number: out_items.len() + 1,
                id: raw.id.clone(),
                name: raw.name.trim().to_string(),
                category: cat.name.clone(),
                description: raw
                    .description
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                variations,
            });
        }
        if !idxs.is_empty() {
            categories.push(Category {
                name: cat.name.clone(),
                items: idxs,
            });
        }
    }

    Ok(Menu {
        restaurant_id: manifest.restaurant_id,
        slug: slug.to_string(),
        name: manifest.restaurant_info.name,
        menu_group_id: menu.menu_group_id.clone(),
        categories,
        items: out_items,
        modifier_groups,
    })
}

/// Live availability (sold-out items). Best effort: the static menu is still usable without it.
async fn fetch_dynamic(api: &Api, slug: &str, loc: Option<&Location>) -> Dynamic {
    let mut rb = api.request(
        Method::GET,
        &format!("{REST}/restaurant/{}/{slug}/menu/dynamic", api.tenant.code),
    );
    if let Some(l) = loc {
        rb = rb.query(&[
            ("latLong", format!("{},{}", l.latitude, l.longitude)),
            ("areaId", l.postcode.clone()),
        ]);
    }
    api.send(rb).await.unwrap_or_default()
}

impl Menu {
    /// Finds items by number ("12") or by case-insensitive name match.
    pub fn find(&self, query: &str) -> Vec<&Item> {
        if let Ok(n) = query.trim().parse::<usize>() {
            return self.items.iter().filter(|i| i.number == n).collect();
        }
        let q = query.to_lowercase();
        let exact: Vec<&Item> = self
            .items
            .iter()
            .filter(|i| i.name.to_lowercase() == q)
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        self.items
            .iter()
            .filter(|i| i.name.to_lowercase().contains(&q))
            .collect()
    }

    pub fn group(&self, id: &str) -> Result<&ModifierGroup> {
        match self.modifier_groups.get(id) {
            Some(g) => Ok(g),
            None => bail!("menu data is inconsistent (unknown option group {id})"),
        }
    }
}
