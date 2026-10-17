//! Checkout and payment. See docs/RESEARCH.md for the API flow.

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::commands::Ctx;

pub async fn preview_json(_ctx: &mut Ctx) -> Result<Value> {
    Err(anyhow!("checkout isn't available yet"))
}

pub async fn place_json(
    _ctx: &mut Ctx,
    _method: &str,
    _expected_total: f64,
    _note: Option<String>,
) -> Result<Value> {
    Err(anyhow!("ordering isn't available yet"))
}
