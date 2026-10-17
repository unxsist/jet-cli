use anyhow::{Context, Result};
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

use crate::api::{Api, REST};
use crate::config::Location;

#[derive(Deserialize)]
struct Feature {
    geometry: Geometry,
    properties: Properties,
}

#[derive(Deserialize)]
struct Geometry {
    /// [longitude, latitude]
    coordinates: [f64; 2],
}

#[derive(Deserialize)]
struct Properties {
    #[serde(rename = "structuredAddress")]
    address: Address,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Address {
    street: Option<String>,
    #[serde(rename = "streetNumberOrBuilding")]
    number: Option<String>,
    #[serde(rename = "formattedPostcode")]
    postcode: Option<String>,
    city: Option<String>,
}

/// Resolves free text ("Damrak 1 Amsterdam", "1012AB") with JET's own geocoder.
pub async fn geocode(api: &Api, query: &str) -> Result<Location> {
    let rb = api
        .request(Method::POST, &format!("{REST}/geocode/{}", api.tenant.code))
        .json(&json!({ "addressLines": [query], "noPostcodeFilter": false }));
    let f: Feature = api
        .send(rb)
        .await
        .with_context(|| format!("couldn't find an address for '{query}'"))?;
    let a = f.properties.address;
    let street = [a.street.unwrap_or_default(), a.number.unwrap_or_default()]
        .join(" ")
        .trim()
        .to_string();
    let postcode = a.postcode.unwrap_or_default();
    let city = a.city.unwrap_or_default();
    let label = [
        street.clone(),
        format!("{postcode} {city}").trim().to_string(),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(", ");
    Ok(Location {
        label,
        postcode,
        latitude: f.geometry.coordinates[1],
        longitude: f.geometry.coordinates[0],
        street,
        city,
    })
}
