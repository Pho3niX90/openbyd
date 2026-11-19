//! Timings of the protocol crate's hot paths: model parsing and dumping,
//! value conversions, JSON text, envelopes, AES, MQTT decoding and the state
//! engine. See `README.md` in this directory.
//!
//! `cargo bench -p openbyd-protocol --bench protocol [-- <filter>]`

use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use openbyd_protocol::aes::{aes_decrypt_utf8, aes_encrypt_hex};
use openbyd_protocol::api::common::build_inner_base;
use openbyd_protocol::coerce;
use openbyd_protocol::config::BydConfig;
use openbyd_protocol::envelope::build_token_outer_envelope;
use openbyd_protocol::models::base::{self, BydModel};
use openbyd_protocol::models::booking::BookingList;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::gps::GpsInfo;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::latest_config::{LatestConfigFunction, VehicleLatestConfig};
use openbyd_protocol::models::message_center::{VehicleMsgItemInfo, VehicleMsgListResponse};
use openbyd_protocol::models::push_notification::PushNotificationState;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::smart_charging::AcCurrentLimit;
use openbyd_protocol::models::vehicle::Vehicle;
use openbyd_protocol::mqtt::decode_mqtt_payload;
use openbyd_protocol::pyjson;
use openbyd_protocol::session::Session;
use openbyd_protocol::state::VehicleStateEngine;
use serde_json::{Value, json};

/// How long each item is measured for, split into this many samples.
const BUDGET: Duration = Duration::from_millis(600);
const SAMPLES: usize = 15;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/corpus")
}

/// The payloads of one corpus file, in file order.
fn corpus(model: &str) -> Vec<(String, Value)> {
    let path = corpus_dir().join(format!("{model}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"));
    let entries: Vec<Value> = serde_json::from_str(&text).expect("the corpus is JSON");
    entries
        .into_iter()
        .map(|entry| {
            (
                entry["note"].as_str().unwrap_or("").to_owned(),
                entry["payload"].clone(),
            )
        })
        .collect()
}

/// The largest payload of a corpus file (by its JSON length).
fn largest(model: &str) -> Value {
    corpus(model)
        .into_iter()
        .map(|(_, payload)| payload)
        .max_by_key(|payload| payload.to_string().len())
        .expect("every corpus file has a payload")
}

fn full_realtime() -> Value {
    corpus("VehicleRealtimeData")
        .into_iter()
        .find(|(note, _)| note == "every field present")
        .expect("the realtime corpus has the full payload")
        .1
}

struct Bench {
    filter: Option<String>,
}

impl Bench {
    /// Times `routine` and prints the median time per call.
    fn run<T>(&self, name: &str, mut routine: impl FnMut() -> T) {
        if self
            .filter
            .as_ref()
            .is_some_and(|filter| !name.contains(filter.as_str()))
        {
            return;
        }
        // Calibrate: how many calls fill one sample.
        let mut batch = 1u64;
        loop {
            let start = Instant::now();
            for _ in 0..batch {
                black_box(routine());
            }
            if start.elapsed() >= BUDGET / (SAMPLES as u32 * 4) {
                break;
            }
            batch *= 2;
        }
        let per_sample = BUDGET / SAMPLES as u32;
        let calls = {
            let start = Instant::now();
            for _ in 0..batch {
                black_box(routine());
            }
            let once = start.elapsed().as_secs_f64() / batch as f64;
            ((per_sample.as_secs_f64() / once) as u64).max(1)
        };
        let mut samples: Vec<f64> = (0..SAMPLES)
            .map(|_| {
                let start = Instant::now();
                for _ in 0..calls {
                    black_box(routine());
                }
                start.elapsed().as_secs_f64() / calls as f64
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        let median = samples[SAMPLES / 2];
        let (low, high) = (samples[0], samples[SAMPLES - 1]);
        println!(
            "{name:<44} {:>12}   (min {}, max {})",
            format_time(median),
            format_time(low),
            format_time(high)
        );
    }
}

fn format_time(seconds: f64) -> String {
    if seconds >= 1e-3 {
        format!("{:.2} ms", seconds * 1e3)
    } else if seconds >= 1e-6 {
        format!("{:.2} µs", seconds * 1e6)
    } else {
        format!("{:.1} ns", seconds * 1e9)
    }
}

fn parse_dump<M: BydModel>(raw: &Value) -> Value {
    M::parse(raw, &Value::Null)
        .expect("corpus payloads parse")
        .dump()
}

fn models(bench: &Bench) {
    let realtime = full_realtime();
    let hybrid = json!({"energy_type": {"EnergyType": 2}});
    bench.run("realtime/full parse+dump (headline)", || {
        parse_dump::<VehicleRealtimeData>(black_box(&realtime))
    });
    bench.run("realtime/full parse", || {
        VehicleRealtimeData::parse(black_box(&realtime), &Value::Null).unwrap()
    });
    bench.run("realtime/full parse_owned+into_dump", || {
        VehicleRealtimeData::parse_owned(black_box(realtime.clone()), &Value::Null)
            .unwrap()
            .into_dump()
    });
    bench.run("realtime/full clone (the input of the row above)", || {
        black_box(&realtime).clone()
    });
    let parsed = VehicleRealtimeData::parse(&realtime, &Value::Null).unwrap();
    bench.run("realtime/full dump", || black_box(&parsed).dump());
    bench.run("realtime/full properties", || {
        black_box(&parsed).properties()
    });
    for (index, (_, payload)) in corpus("VehicleRealtimeData").iter().enumerate() {
        bench.run(&format!("realtime/corpus[{index}] parse+dump"), || {
            VehicleRealtimeData::parse(black_box(payload), &hybrid)
                .unwrap()
                .dump()
        });
    }

    macro_rules! largest_of {
        ($($model:ident),+ $(,)?) => {$(
            let payload = largest(stringify!($model));
            bench.run(concat!("model/", stringify!($model), " parse+dump"), || {
                parse_dump::<$model>(black_box(&payload))
            });
        )+};
    }
    largest_of!(
        AcCurrentLimit,
        BookingList,
        ChargingStatus,
        EnergyConsumption,
        GpsInfo,
        HvacStatus,
        LatestConfigFunction,
        PushNotificationState,
        Vehicle,
        VehicleLatestConfig,
        VehicleMsgItemInfo,
        VehicleMsgListResponse,
    );
}

fn helpers(bench: &Bench) {
    let realtime = full_realtime();
    bench.run("base/clean_byd_values (realtime)", || {
        base::clean_byd_values(black_box(&realtime), &[("abs", "absWarning")])
    });
    bench.run("base/to_camel", || {
        base::to_camel(black_box("left_front_tire_pressure"))
    });
    let values = [
        json!(12),
        json!(" 12 "),
        json!(2.5),
        json!("1_000"),
        json!(true),
        json!("x"),
    ];
    bench.run("coerce/int x6", || {
        values
            .iter()
            .map(|v| coerce::int(black_box(v)).is_ok())
            .filter(|ok| *ok)
            .count()
    });
    bench.run("coerce/float x6", || {
        values
            .iter()
            .map(|v| coerce::float(black_box(v)).is_ok())
            .filter(|ok| *ok)
            .count()
    });
    bench.run("coerce/string x6", || {
        values
            .iter()
            .map(|v| coerce::string(black_box(v)).is_ok())
            .filter(|ok| *ok)
            .count()
    });
    bench.run("coerce/int_enum x6", || {
        values
            .iter()
            .map(|v| {
                coerce::int_enum::<openbyd_protocol::models::realtime::LockState>(black_box(v))
                    .is_ok()
            })
            .filter(|ok| *ok)
            .count()
    });
    bench.run("pyjson/dumps_compact (realtime)", || {
        pyjson::dumps_compact(black_box(&realtime))
    });
    let text = pyjson::dumps_compact(&realtime);
    bench.run("pyjson/loads (realtime)", || {
        pyjson::loads(black_box(&text)).unwrap()
    });
    bench.run("pyjson/py_str float", || {
        pyjson::py_str(black_box(&json!(12.5)))
    });
}

fn transport(bench: &Bench) {
    let config = BydConfig::new("user@example.com", "secret");
    let session = Session::new("1234567", "XXXXXXXXSIGN", "XXXXXXXXENCRY", 0.0, 43_200.0);
    let random = [7u8; 16];
    let inner = build_inner_base(
        &config,
        1_700_000_000_000,
        &random,
        Some("TESTVIN0000000001"),
        None,
    );
    bench.run("envelope/build_token_outer_envelope", || {
        build_token_outer_envelope(
            &config,
            &session,
            black_box(&inner),
            1_700_000_000_000,
            None,
            1_700_000_000_100,
        )
    });
    let key = session.content_key().to_owned();
    let inner_text = pyjson::dumps_compact(&Value::Object(inner.clone()));
    bench.run("aes/encrypt inner request", || {
        aes_encrypt_hex(black_box(&inner_text), &key).unwrap()
    });
    let reply_text = pyjson::dumps_compact(&full_realtime());
    let sealed = aes_encrypt_hex(&reply_text, &key).unwrap();
    bench.run("aes/decrypt realtime reply", || {
        aes_decrypt_utf8(black_box(&sealed), &key).unwrap()
    });
    let push = json!({"event": "vehicleInfo", "vin": "TESTVIN0000000001", "data": {"respondData": full_realtime()}});
    let push_sealed = aes_encrypt_hex(&pyjson::dumps_compact(&push), &key).unwrap();
    bench.run("mqtt/decode_mqtt_payload (realtime push)", || {
        decode_mqtt_payload(black_box(push_sealed.as_bytes()), &key).unwrap()
    });
}

fn state(bench: &Bench) {
    let vehicle = Vehicle::parse(&largest("Vehicle"), &Value::Null).unwrap();
    let realtime = VehicleRealtimeData::parse(&full_realtime(), &Value::Null).unwrap();
    let mut engine = VehicleStateEngine::new("TESTVIN0000000001", vehicle, 30.0);
    let mut now = 0.0;
    bench.run("state/update_realtime (full)", || {
        now += 1.0;
        engine.update_realtime(black_box(realtime.clone()), now);
    });
}

fn main() {
    // `cargo bench` passes `--bench`; anything else not starting with `-` is a name filter.
    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let bench = Bench { filter };
    models(&bench);
    helpers(&bench);
    transport(&bench);
    state(&bench);
}
