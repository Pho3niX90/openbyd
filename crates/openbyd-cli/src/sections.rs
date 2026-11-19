//! The read-only sections of a vehicle's data, for `get` and `dump`.

use openbyd_client::client::{BydClient, PollSettings, ProductionEnv};
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::latest_config::VehicleCapabilities;
use openbyd_protocol::models::vehicle::Vehicle;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::cli::Section;

type Client = BydClient<ProductionEnv>;

/// One section as fetched: the parsed model and the raw API payload.
pub struct Fetched {
    /// The parsed model, without its `raw` field.
    pub parsed: Value,
    /// The payload the API sent.
    pub raw: Value,
}

impl Fetched {
    /// The section as printed: the parsed model, or with `raw` a
    /// `{"parsed": ..., "raw": ...}` pair.
    pub fn into_value(self, with_raw: bool) -> Value {
        if with_raw {
            json!({"parsed": self.parsed, "raw": self.raw})
        } else {
            self.parsed
        }
    }
}

/// Splits a serialised model into what it holds and its `raw` payload.
fn split(model: &impl Serialize) -> Fetched {
    let mut parsed = serde_json::to_value(model).expect("models serialise to JSON");
    let raw = parsed
        .as_object_mut()
        .and_then(|map| map.remove("raw"))
        .unwrap_or(Value::Null);
    Fetched { parsed, raw }
}

/// A serialised model and its `raw` payload, as a pair.
pub fn split_value(model: &impl Serialize) -> (Value, Value) {
    let part = split(model);
    (part.parsed, part.raw)
}

/// A list of models as one section.
fn split_all<'a, M: Serialize + 'a>(models: impl IntoIterator<Item = &'a M>) -> Fetched {
    let (parsed, raw): (Vec<Value>, Vec<Value>) = models
        .into_iter()
        .map(|model| {
            let part = split(model);
            (part.parsed, part.raw)
        })
        .unzip();
    Fetched {
        parsed: Value::Array(parsed),
        raw: Value::Array(raw),
    }
}

/// A vehicle as printed by `vehicles` and as the head of a `dump` entry.
pub fn vehicle_summary(vehicle: &Vehicle) -> Value {
    use openbyd_protocol::coerce::IntEnum;
    use openbyd_protocol::models::base::PyEnum;
    json!({
        "vin": vehicle.vin,
        "model_name": vehicle.model_name,
        "auto_alias": vehicle.auto_alias,
        "energy_type": vehicle.energy_type.name(),
        "energy_type_code": vehicle.energy_type.value(),
    })
}

/// Fetches one section for one vehicle.
pub async fn fetch(client: &Client, section: Section, vin: &str) -> Result<Fetched, Error> {
    Ok(match section {
        Section::Realtime => split(
            &client
                .get_vehicle_realtime(vin, PollSettings::default())
                .await?,
        ),
        Section::Gps => split(&client.get_gps_info(vin, PollSettings::default()).await?),
        Section::Charging => split(&client.get_charging_status(vin).await?),
        Section::Hvac => split(&client.get_hvac_status(vin).await?),
        Section::Energy => split(&client.get_energy_consumption(vin).await?),
        Section::ChargingSchedule => split(&client.get_charging_schedule(vin).await?),
        Section::LatestConfig => split(&client.get_latest_config(vin).await?),
        Section::Capabilities => split(&capabilities(client, vin).await?),
        Section::PushState => split(&client.get_push_state(vin).await?),
        Section::Bookings => split(&client.get_booking_list(vin).await?),
        Section::Notifications => notifications(client).await?,
        Section::AcCurrentLimit => split(&client.get_ac_current_limit(vin, None).await?),
    })
}

/// The client turns a failed fetch into "unknown" capabilities; ask again
/// for the latest config so the real error is reported.
async fn capabilities(client: &Client, vin: &str) -> Result<VehicleCapabilities, Error> {
    let capabilities = client.get_vehicle_capabilities(vin, false).await;
    if capabilities.source.starts_with("unknown") {
        let latest = client.get_latest_config(vin).await?;
        return Ok(VehicleCapabilities::from_latest_config(vin, &latest));
    }
    Ok(capabilities)
}

async fn notifications(client: &Client) -> Result<Fetched, Error> {
    let mut parsed = Map::new();
    let mut raw = Map::new();
    for (category, messages) in client.get_all_notifications().await? {
        let part = split_all(&messages);
        parsed.insert(category.name().to_owned(), part.parsed);
        raw.insert(category.name().to_owned(), part.raw);
    }
    Ok(Fetched {
        parsed: Value::Object(parsed),
        raw: Value::Object(raw),
    })
}

/// A failure as printed in a section: `{"error": {"class", "code", "message"}}`.
pub fn error_value(error: &Error) -> Value {
    json!({"error": {
        "class": error.class_name(),
        "code": error.code(),
        "message": error.message(),
    }})
}

/// Whether a failure ends the whole `dump` rather than one section: the
/// credentials stopped working, or the server cannot be reached at all.
pub fn is_fatal(error: &Error) -> bool {
    error.is_a(ErrorKind::BydAuthenticationError)
        || (error.is_a(ErrorKind::BydTransportError) && error.status_code().is_none())
}
