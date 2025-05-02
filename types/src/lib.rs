use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub use fioul;
use fioul::{Price, Station};

#[derive(Serialize, Deserialize)]
#[serde(tag = "status")]
#[serde(rename_all = "snake_case")]
pub enum Response<T> {
    Error { code: u64, message: String },
    Ok(T),
}

#[derive(Serialize, Deserialize)]
pub struct Stations {
    pub stations: Vec<Station>,
}

#[derive(Serialize, Deserialize)]
pub struct FuelList {
    pub result: HashMap<u64, Vec<Price>>,
}
