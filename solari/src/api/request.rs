use std::usize;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::LatLng;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum TransferMode {
    #[serde(rename = "walking")]
    Walking,
    #[serde(rename = "cycling")]
    Cycling,
}

impl Default for TransferMode {
    fn default() -> Self {
        Self::Walking
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TransferQuantity(pub usize);

impl Default for TransferQuantity {
    fn default() -> Self {
        Self(usize::MAX)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SolariRequest {
    pub from: LatLng,
    pub to: LatLng,
    #[serde(
        default,
        serialize_with = "serialize_optional_millis",
        deserialize_with = "deserialize_optional_millis",
        skip_serializing_if = "Option::is_none"
    )]
    pub start_at: Option<OffsetDateTime>,

    #[serde(
        default,
        serialize_with = "serialize_optional_millis",
        deserialize_with = "deserialize_optional_millis",
        skip_serializing_if = "Option::is_none"
    )]
    pub end_at: Option<OffsetDateTime>,

    #[serde(default)]
    pub transfer_mode: TransferMode,
    #[serde(default)]
    pub max_transfers: TransferQuantity,
}

fn serialize_optional_millis<S: serde::Serializer>(
    value: &Option<OffsetDateTime>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(dt) => time::serde::timestamp::milliseconds::serialize(dt, serializer),
        None => serializer.serialize_none(),
    }
}

fn deserialize_optional_millis<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<OffsetDateTime>, D::Error> {
    let opt: Option<i64> = Option::deserialize(deserializer)?;
    match opt {
        Some(ms) => OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
            .map(Some)
            .map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}
