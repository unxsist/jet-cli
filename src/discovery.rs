use std::collections::HashMap;

use anyhow::Result;
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::api::{Api, REST};
use crate::config::Location;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryResponse {
    restaurants: Vec<RawRestaurant>,
    #[serde(default)]
    delivery_fees: Option<DeliveryFees>,
}

#[derive(Deserialize)]
struct DeliveryFees {
    #[serde(default)]
    restaurants: HashMap<String, FeeInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeeInfo {
    #[serde(default)]
    minimum_order_value: i64,
    #[serde(default)]
    bands: Vec<FeeBand>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeeBand {
    #[serde(default)]
    minimum_amount: i64,
    #[serde(default)]
    fee: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRestaurant {
    id: String,
    name: String,
    unique_name: String,
    #[serde(default)]
    rating: Option<Rating>,
    #[serde(default)]
    delivery_eta_minutes: Option<Eta>,
    #[serde(default)]
    is_open_now_for_delivery: bool,
    #[serde(default)]
    is_open_now_for_collection: bool,
    #[serde(default)]
    is_open_now_for_preorder: bool,
    #[serde(default)]
    is_temporarily_offline: bool,
    #[serde(default)]
    cuisines: Vec<Cuisine>,
    #[serde(default)]
    drive_distance_meters: Option<u32>,
    #[serde(default)]
    deals: Vec<Deal>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rating {
    #[serde(default)]
    count: u32,
    #[serde(default)]
    star_rating: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Eta {
    range_lower: Option<u32>,
    range_upper: Option<u32>,
    approximate: Option<u32>,
}

#[derive(Deserialize)]
struct Cuisine {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Deal {
    #[serde(default)]
    offer_type: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Restaurant {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub rating: Option<f32>,
    pub rating_count: u32,
    pub cuisines: Vec<String>,
    pub open: bool,
    pub preorder: bool,
    pub eta_min: Option<u32>,
    pub eta_max: Option<u32>,
    pub delivery_fee_cents: Option<i64>,
    pub min_order_cents: Option<i64>,
    pub distance_m: Option<u32>,
    pub offers: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ServiceType {
    Delivery,
    Collection,
}

impl ServiceType {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceType::Delivery => "delivery",
            ServiceType::Collection => "collection",
        }
    }
}

pub async fn near(api: &Api, loc: &Location, service: ServiceType) -> Result<Vec<Restaurant>> {
    let rb = api
        .request(
            Method::GET,
            &format!("{REST}/discovery/{}/restaurants/enriched", api.tenant.code),
        )
        .header("Accept", "application/json;v=3")
        .query(&[
            ("latitude", loc.latitude.to_string()),
            ("longitude", loc.longitude.to_string()),
            ("serviceType", service.as_str().to_string()),
            ("ratingsOutOfFive", "true".to_string()),
        ]);
    let resp: DiscoveryResponse = api.send(rb).await?;
    let fees = resp
        .delivery_fees
        .map(|f| f.restaurants)
        .unwrap_or_default();
    Ok(resp
        .restaurants
        .into_iter()
        .map(|r| {
            let fee = fees.get(&r.id);
            let open = !r.is_temporarily_offline
                && match service {
                    ServiceType::Delivery => r.is_open_now_for_delivery,
                    ServiceType::Collection => r.is_open_now_for_collection,
                };
            let eta = r.delivery_eta_minutes.as_ref();
            Restaurant {
                id: r.id,
                slug: r.unique_name,
                name: r.name,
                rating: r
                    .rating
                    .as_ref()
                    .filter(|x| x.count > 0)
                    .map(|x| x.star_rating),
                rating_count: r.rating.map(|x| x.count).unwrap_or(0),
                cuisines: r.cuisines.into_iter().map(|c| c.name).collect(),
                open,
                preorder: r.is_open_now_for_preorder,
                eta_min: eta.and_then(|e| e.range_lower.or(e.approximate)),
                eta_max: eta.and_then(|e| e.range_upper.or(e.approximate)),
                delivery_fee_cents: fee.and_then(|f| {
                    f.bands
                        .iter()
                        .min_by_key(|b| b.minimum_amount)
                        .map(|b| b.fee)
                }),
                min_order_cents: fee.map(|f| f.minimum_order_value),
                distance_m: r.drive_distance_meters,
                offers: r
                    .deals
                    .into_iter()
                    .map(|d| {
                        if d.description.is_empty() {
                            d.offer_type
                        } else {
                            d.description
                        }
                    })
                    .collect(),
            }
        })
        .collect())
}
