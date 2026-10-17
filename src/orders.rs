use anyhow::Result;
use reqwest::Method;
use serde::{Deserialize, Deserializer, Serialize};

use crate::api::{Api, REST};

/// Treats `null` like a missing field; the order API uses both.
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct Page {
    orders: Vec<Order>,
    #[serde(rename = "_pagination", default)]
    pagination: Option<Pagination>,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub id: String,
    pub friendly_id: String,
    pub information: Information,
    pub basket: OrderBasket,
    pub restaurant: OrderRestaurant,
    #[serde(default, deserialize_with = "nullable")]
    pub status: Option<Status>,
    #[serde(default, deserialize_with = "nullable")]
    pub consumer: Option<Consumer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Consumer {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub contact_number: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Information {
    pub created_at: String,
    #[serde(default, deserialize_with = "nullable")]
    pub service_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBasket {
    pub total: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub basket_id: String,
    pub summary: OrderSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderSummary {
    #[serde(default, deserialize_with = "nullable")]
    pub items: Vec<OrderItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderItem {
    /// The menu variation id at the time of ordering.
    pub id: String,
    pub name: String,
    pub quantity: u32,
    pub price: f64,
    #[serde(default, deserialize_with = "nullable")]
    pub accessories: Vec<Accessory>,
    #[serde(default, deserialize_with = "nullable")]
    pub customer_notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Accessory {
    pub accessory_id: String,
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub quantity: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRestaurant {
    pub seo_name: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub value: String,
}

impl Order {
    pub fn is_pickup(&self) -> bool {
        self.information
            .service_type
            .eq_ignore_ascii_case("collection")
    }
}

/// Most recent orders first; follows pagination until `limit` orders are collected.
pub async fn recent(api: &Api, limit: usize) -> Result<Vec<Order>> {
    let token = api.require_token().await?;
    let url = format!("{REST}/consumer/me/orders/{}", api.tenant.code);
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let mut rb = api
            .request(Method::GET, &url)
            .bearer_auth(&token)
            .query(&[("ratingsOutOfFive", "true")]);
        if let Some(t) = &page_token {
            rb = rb.query(&[("pagination", t)]);
        }
        let page: Page = api.send(rb).await?;
        let got = page.orders.len();
        out.extend(page.orders);
        page_token = page.pagination.and_then(|p| p.token);
        if out.len() >= limit || got == 0 || page_token.is_none() {
            break;
        }
    }
    out.truncate(limit);
    Ok(out)
}
