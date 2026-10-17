use anyhow::Result;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, REST};
use crate::config::{Location, Paths, read_json, write_json};
use crate::discovery::ServiceType;

/// The basket we're currently building, persisted between commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Current {
    pub basket_id: String,
    pub restaurant_slug: String,
    pub restaurant_name: String,
    pub service_type: String,
}

impl Current {
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        read_json(&paths.basket_file())
    }
    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_json(&paths.basket_file(), self)
    }
    pub fn clear(paths: &Paths) -> Result<()> {
        match std::fs::remove_file(paths.basket_file()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// A product with chosen options, ready to put in a basket.
#[derive(Debug, Clone, Serialize)]
pub struct NewProduct {
    pub variation_id: String,
    pub quantity: u32,
    /// (modifier group id, modifier ids)
    pub modifiers: Vec<(String, Vec<String>)>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Basket {
    pub basket_id: String,
    pub restaurant_seo_name: String,
    pub service_type: String,
    pub basket_summary: Summary,
    #[serde(default)]
    pub is_orderable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Summary {
    #[serde(default)]
    pub products: Vec<Product>,
    #[serde(default)]
    pub delivery_charge: f64,
    #[serde(default)]
    pub adjustments: Vec<Adjustment>,
    pub basket_totals: Totals,
    #[serde(default)]
    pub prompts: Option<Prompts>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Product {
    pub basket_product_ids: Vec<String>,
    pub name: String,
    #[serde(default)]
    pub variation: String,
    pub quantity: u32,
    pub total_price: f64,
    #[serde(default)]
    pub customer_notes: String,
    pub product_id: String,
    #[serde(default)]
    pub modifier_groups: Vec<ProductModifierGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProductModifierGroup {
    pub modifier_group_id: String,
    pub modifiers: Vec<ProductModifier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProductModifier {
    pub modifier_id: String,
    pub name: String,
    pub quantity: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Adjustment {
    pub name: String,
    pub adjustment: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Totals {
    pub sub_total: f64,
    #[serde(default)]
    pub fees_total: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Prompts {
    #[serde(default)]
    pub spend_more: Option<SpendMore>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SpendMore {
    pub minimum_order_amount: f64,
    pub to_spend: f64,
}

impl Product {
    pub fn id(&self) -> &str {
        self.basket_product_ids
            .first()
            .map(String::as_str)
            .unwrap_or_default()
    }

    pub fn options(&self) -> Vec<String> {
        self.modifier_groups
            .iter()
            .flat_map(|g| g.modifiers.iter())
            .map(|m| m.name.clone())
            .collect()
    }

    fn modifier_payload(&self) -> Value {
        Value::Array(
            self.modifier_groups
                .iter()
                .map(|g| {
                    json!({
                        "modifierGroupId": g.modifier_group_id,
                        "modifiers": g.modifiers.iter()
                            .map(|m| json!({ "modifierId": m.modifier_id, "quantity": m.quantity }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    }
}

fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn product_json(p: &NewProduct) -> Value {
    json!({
        "date": now(),
        "productId": p.variation_id,
        "quantity": p.quantity,
        "customerNotes": p.note,
        "dealGroups": [],
        "modifierGroups": p.modifiers.iter().map(|(group, mods)| json!({
            "modifierGroupId": group,
            "modifiers": mods.iter().map(|m| json!({ "modifierId": m, "quantity": 1 })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn geo(loc: &Location) -> Value {
    json!({ "latitude": loc.latitude, "longitude": loc.longitude })
}

pub async fn create(
    api: &Api,
    slug: &str,
    menu_group_id: &str,
    service: ServiceType,
    loc: &Location,
    product: &NewProduct,
) -> Result<Basket> {
    let body = json!({
        "deals": [],
        "products": [product_json(product)],
        "orderDetails": { "location": { "zipCode": loc.postcode, "geoLocation": geo(loc) } },
        "menuGroupId": menu_group_id,
        "restaurantSeoName": slug,
        "serviceType": service.as_str(),
        "consents": [],
    });
    let rb = api
        .request_auth(Method::POST, &format!("{REST}/basket"))
        .await?
        .json(&body);
    api.send(rb).await
}

pub async fn get(api: &Api, id: &str) -> Result<Basket> {
    api.send(
        api.request_auth(Method::GET, &format!("{REST}/basket/{id}"))
            .await?,
    )
    .await
}

async fn put(api: &Api, id: &str, body: Value) -> Result<Basket> {
    let rb = api
        .request_auth(Method::PUT, &format!("{REST}/basket/{id}"))
        .await?
        .json(&body);
    api.send(rb).await
}

pub async fn add(
    api: &Api,
    current: &Current,
    loc: &Location,
    product: &NewProduct,
) -> Result<Basket> {
    put(
        api,
        &current.basket_id,
        json!({
            "basketId": current.basket_id,
            "deal": { "added": [] },
            "product": { "added": [product_json(product)] },
            "orderDetails": { "location": { "zipCode": { "value": loc.postcode }, "geoLocation": { "value": geo(loc) } } },
            "selectedServiceType": { "date": now(), "value": current.service_type },
            "consents": [],
            "restaurantSeoName": current.restaurant_slug,
        }),
    )
    .await
}

pub async fn set_quantity(
    api: &Api,
    basket_id: &str,
    product: &Product,
    quantity: u32,
) -> Result<Basket> {
    put(
        api,
        basket_id,
        json!({
            "basketId": basket_id,
            "product": { "updated": [{
                "date": now(),
                "basketProductId": product.id(),
                "quantity": { "total": quantity },
                "modifierGroups": product.modifier_payload(),
                "dealGroups": [],
            }]},
        }),
    )
    .await
}

pub async fn remove(api: &Api, basket_id: &str, product: &Product) -> Result<Basket> {
    put(
        api,
        basket_id,
        json!({
            "basketId": basket_id,
            "product": { "removed": [{
                "date": now(),
                "basketProductId": product.id(),
                "modifierGroups": product.modifier_payload(),
                "dealGroups": [],
            }]},
        }),
    )
    .await
}
