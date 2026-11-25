//! Vehicle service history models (`pybyd/models/maintenance.py`).

use serde::Serialize;
use serde_json::{Map, Value};

use super::base::{self, BydModel, Fields, Plan, PyDateTime, aliased};
use crate::coerce::{self, ValidationError};
use crate::parity::Outcome;

/// Single workshop visit from `App.AppMaintenance.getInfoFromOthers`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MaintenanceRecord {
    /// Original API response dict (with personal contact/location keys scrubbed).
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Repair order number (`ro_no` on the wire).
    pub order_no: String,
    /// Odometer reading at service (`total_mileage` on the wire).
    pub mileage: Option<i64>,
    /// Service date (`last_balance_date` on the wire).
    #[serde(serialize_with = "base::ser_opt_datetime")]
    pub service_date: Option<PyDateTime>,
    /// Workshop name.
    pub shop_name: String,
    /// Dealer code.
    pub dealer_code: String,
    /// Repair type code.
    pub repair_type_code: String,
}

impl BydModel for MaintenanceRecord {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, _context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let text = |fields: &mut Fields, field| fields.get(field, String::new(), coerce::string);
        let model = Self {
            raw: fields.raw(),
            vin: text(&mut fields, "vin".into()),
            order_no: text(&mut fields, aliased("order_no", &["ro_no"])),
            mileage: fields.opt(aliased("mileage", &["total_mileage"]), coerce::int),
            service_date: fields.get(
                aliased("service_date", &["last_balance_date"]),
                None,
                base::byd_timestamp,
            ),
            shop_name: text(&mut fields, "shop_name".into()),
            dealer_code: text(&mut fields, "dealer_code".into()),
            repair_type_code: text(&mut fields, "repair_type_code".into()),
        };
        fields.finish()?;
        Ok(model)
    }
}

/// Exact UTC instant in microseconds since the Unix epoch, for comparing aware
/// datetimes the way Python compares them.
fn utc_micros(datetime: PyDateTime) -> i128 {
    i128::from(datetime.local.and_utc().timestamp_micros()) - i128::from(datetime.offset_us)
}

/// `datetime.min.replace(tzinfo=UTC)` (`0001-01-01T00:00:00Z`) in microseconds
/// since the Unix epoch.
const OLDEST_UTC_MICROS: i128 = -62_135_596_800_000_000;

/// Service history list returned by `App.AppMaintenance.getInfoFromOthers`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MaintenanceHistory {
    /// Original API response dict (with personal contact/location keys scrubbed).
    pub raw: Map<String, Value>,
    /// Vehicle identification number.
    pub vin: String,
    /// Workshop visits.
    pub records: Vec<MaintenanceRecord>,
}

impl MaintenanceHistory {
    /// Most recent record by service date, `None` when there are none.
    ///
    /// Matches Python's `max(records, key=lambda r: r.service_date or oldest)`:
    /// records without a `service_date` compare as `0001-01-01T00:00:00Z`, and
    /// ties keep the first record.
    pub fn latest(&self) -> Option<&MaintenanceRecord> {
        let mut iter = self.records.iter();
        let mut best = iter.next()?;
        let mut best_key = best.service_date.map_or(OLDEST_UTC_MICROS, utc_micros);
        for record in iter {
            let key = record.service_date.map_or(OLDEST_UTC_MICROS, utc_micros);
            if key > best_key {
                best = record;
                best_key = key;
            }
        }
        Some(best)
    }
}

impl BydModel for MaintenanceHistory {
    fn raw_mut(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.raw)
    }

    fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
        static PLAN: Plan = Plan::new(&[]);
        let mut fields = Fields::planned(&PLAN, raw, &[])?;
        let model = Self {
            raw: fields.raw(),
            vin: fields.get("vin", String::new(), coerce::string),
            records: fields.get("records", Vec::new(), |value| {
                coerce::list(value, |item| MaintenanceRecord::parse(item, context))
            }),
        };
        fields.finish()?;
        Ok(model)
    }

    fn properties(&self) -> Map<String, Value> {
        base::properties([("latest", self.latest().map_or(Value::Null, BydModel::dump))])
    }
}

pub(crate) fn parity_call(name: &str, args: &Value) -> Option<Outcome> {
    Some(match name {
        "models.maintenance.MaintenanceRecord" => base::model_call::<MaintenanceRecord>(args),
        "models.maintenance.MaintenanceHistory" => base::model_call::<MaintenanceHistory>(args),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn record_aliases_and_history_latest() {
        let history = MaintenanceHistory::parse(
            &json!({
                "vin": "TESTVIN0000000001",
                "records": [
                    {
                        "vin": "TESTVIN0000000001",
                        "ro_no": "RO-OLD",
                        "total_mileage": 50210,
                        "last_balance_date": 1_760_000_000_000_i64,
                        "shopName": "BYD Old",
                        "dealerCode": "D1",
                        "repairTypeCode": "JSXM"
                    },
                    {
                        "vin": "TESTVIN0000000001",
                        "ro_no": "RO-NEW",
                        "total_mileage": 99678,
                        "last_balance_date": 1_791_771_512_000_i64,
                        "shop_name": "BYD New",
                        "dealer_code": "D2",
                        "repair_type_code": "BY"
                    },
                    {
                        "vin": "TESTVIN0000000001",
                        "ro_no": "RO-TIE",
                        "last_balance_date": 1_791_771_512_000_i64
                    },
                ]
            }),
            &Value::Null,
        )
        .unwrap();
        let latest = history.latest().unwrap();
        assert_eq!(latest.order_no, "RO-NEW");
        assert_eq!(latest.mileage, Some(99678));
        assert_eq!(latest.shop_name, "BYD New");
        assert_eq!(latest.dealer_code, "D2");
        assert_eq!(latest.repair_type_code, "BY");

        let empty = MaintenanceHistory::parse(&json!({}), &Value::Null).unwrap();
        assert!(empty.latest().is_none());
        assert_eq!(empty.properties()["latest"], Value::Null);
        assert!(parity_call("models.maintenance.Other", &json!({})).is_none());

        let min_utc = chrono::NaiveDate::from_ymd_opt(1, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .unwrap()
            .and_utc()
            .timestamp_micros();
        assert_eq!(OLDEST_UTC_MICROS, i128::from(min_utc));
    }
}
