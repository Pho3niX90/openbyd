//! The car layer on the client: pyBYD's `get_car`, the client as a
//! [`CarBackend`], and the cars' share of the MQTT pushes.
//!
//! [`BydClient::get_car`] builds one [`BydCar`] per VIN and keeps it, as
//! pyBYD's `_cars` does; MQTT `vehicleInfo`, `vehicleStatusPush`,
//! `smartCharge` and `energyConsumption` pushes for that VIN are merged into it.
//!
//! A car the client keeps reaches the client through a [`ClientHandle`],
//! which holds it weakly: the client owns its cars, the cars do not own the
//! client, so dropping the last client handle frees both. A car whose
//! client is gone fails every call as a closed client does. A car built by
//! hand (`BydCar::new(client.clone(), ...)`) holds the client itself, which
//! also implements [`CarBackend`].

use std::sync::{Arc, Weak};

use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::control::CommandAck;
use openbyd_protocol::models::control::{
    BatteryHeatParams, ChargeChangeResult, ClimateScheduleParams, ClimateStartParams,
    RemoteControlResult, SeatClimateParams,
};
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::gps::GpsInfo;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::ota::OtaUpgradeInfo;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::smart_charging::{AcCurrentLimit, SmartChargingSchedule};
use openbyd_protocol::models::vehicle::Vehicle;
use openbyd_protocol::state::{DEFAULT_PROJECTION_TTL, StateChangedCallback};

use super::{BydClient, ChargePollSettings, Inner, PollSettings};
use crate::car::{BydCar, CarBackend, CarOptions, Clock};
use crate::env::{ClientEnv, Env};

/// A car the client keeps (what [`BydClient::get_car`] returns).
pub type ClientCar<E> = Arc<BydCar<ClientHandle<E>>>;

/// The optional arguments of [`BydClient::get_car`] (pyBYD's keyword
/// arguments, with their defaults).
pub struct GetCarOptions {
    /// The vehicle's metadata; `None` fetches the vehicle list to find it.
    pub vehicle: Option<Vehicle>,
    /// Called with the VIN and the new snapshot after every state change.
    pub on_state_changed: Option<StateChangedCallback>,
    /// How long a command's projections stay active, in seconds (default 30).
    pub projection_ttl: f64,
}

impl Default for GetCarOptions {
    fn default() -> Self {
        Self {
            vehicle: None,
            on_state_changed: None,
            projection_ttl: DEFAULT_PROJECTION_TTL,
        }
    }
}

/// A weak handle on a client, the [`CarBackend`] of the cars it keeps.
pub struct ClientHandle<E: ClientEnv>(Weak<Inner<E>>);

impl<E: ClientEnv> Clone for ClientHandle<E> {
    fn clone(&self) -> Self {
        Self(Weak::clone(&self.0))
    }
}

impl<E: ClientEnv> ClientHandle<E> {
    /// The client, while any handle on it is alive.
    pub fn client(&self) -> Option<BydClient<E>> {
        self.0.upgrade().map(|inner| BydClient { inner })
    }

    /// The client, or the error a closed client raises.
    fn live(&self) -> Result<BydClient<E>, Error> {
        self.client().ok_or_else(super::not_initialized)
    }
}

/// The client's monotonic clock, for the projections of its cars; a car
/// whose client is gone sees every projection expired.
struct ClientClock<E: ClientEnv>(Weak<Inner<E>>);

impl<E: ClientEnv> Clock for ClientClock<E> {
    fn monotonic(&self) -> f64 {
        self.0
            .upgrade()
            .map_or(f64::INFINITY, |inner| inner.env.monotonic())
    }
}

impl<E: ClientEnv> BydClient<E> {
    /// The car of `vin`, built once and kept (pyBYD's `get_car`).
    ///
    /// A kept car is returned as it is, whatever the options. Otherwise the
    /// vehicle list is fetched unless `options.vehicle` is given (a VIN the
    /// account does not have fails with `BydError`), then the capabilities
    /// ([`get_vehicle_capabilities`](Self::get_vehicle_capabilities)), and the
    /// new car replaces any car kept for the VIN meanwhile.
    pub async fn get_car(&self, vin: &str, options: GetCarOptions) -> Result<ClientCar<E>, Error> {
        if let Some(existing) = self.car(vin) {
            return Ok(existing);
        }
        let vehicle = match options.vehicle {
            Some(vehicle) => vehicle,
            None => self
                .get_vehicles()
                .await?
                .into_iter()
                .find(|vehicle| vehicle.vin == vin)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::BydError,
                        format!("Vehicle {vin} not found in account"),
                    )
                })?,
        };
        let capabilities = self.get_vehicle_capabilities(vin, false).await;
        let car = BydCar::new(
            ClientHandle(Arc::downgrade(&self.inner)),
            vin,
            vehicle,
            CarOptions {
                capabilities: Some(capabilities),
                on_state_changed: options.on_state_changed,
                projection_ttl: options.projection_ttl,
                clock: Arc::new(ClientClock(Arc::downgrade(&self.inner))),
            },
        )?;
        let car = Arc::new(car);
        self.cars().insert(vin.to_owned(), Arc::clone(&car));
        Ok(car)
    }

    /// The car kept for `vin`, if [`get_car`](Self::get_car) built one.
    pub fn car(&self, vin: &str) -> Option<ClientCar<E>> {
        self.cars().get(vin).cloned()
    }

    /// Closes and forgets every kept car (the first step of pyBYD's `async_close`).
    pub(super) fn close_cars(&self) {
        let cars: Vec<_> = self.cars().drain().map(|(_, car)| car).collect();
        for car in cars {
            car.close();
        }
    }

    /// Hands a parsed `vehicleInfo` push to the car kept for `vin`.
    pub(super) fn route_realtime(&self, vin: &str, data: &VehicleRealtimeData) {
        if let Some(car) = self.car(vin) {
            car.handle_mqtt_realtime(data.clone());
        }
    }

    /// Hands the partial models of a `vehicleStatusPush` to the car kept for `vin`.
    pub(super) fn route_status_push(
        &self,
        vin: &str,
        data: &VehicleRealtimeData,
        hvac: Option<&HvacStatus>,
        charging: Option<&ChargingStatus>,
    ) {
        if let Some(car) = self.car(vin) {
            car.handle_mqtt_status_push(data.clone(), hvac.cloned(), charging.cloned());
        }
    }

    /// Hands a parsed `smartCharge` push to the car kept for `vin`.
    pub(super) fn route_charging(&self, vin: &str, data: &ChargingStatus) {
        if let Some(car) = self.car(vin) {
            car.handle_mqtt_charging(data.clone());
        }
    }

    /// Hands a parsed `energyConsumption` push to the car kept for `vin`.
    pub(super) fn route_energy(&self, vin: &str, data: &EnergyConsumption) {
        if let Some(car) = self.car(vin) {
            car.handle_mqtt_energy(data.clone());
        }
    }
}

/// The client as the car layer calls it: each method with pyBYD's
/// defaults for what the car does not pass (no `command_pwd`, so the
/// configured PIN; the default polling).
impl<E: ClientEnv> CarBackend for BydClient<E> {
    async fn lock(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::lock(self, vin, None).await
    }

    async fn unlock(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::unlock(self, vin, None).await
    }

    async fn start_climate(
        &self,
        vin: &str,
        params: &ClimateStartParams,
    ) -> Result<RemoteControlResult, Error> {
        BydClient::start_climate(self, vin, params, None).await
    }

    async fn stop_climate(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::stop_climate(self, vin, None).await
    }

    async fn schedule_climate(
        &self,
        vin: &str,
        params: &ClimateScheduleParams,
    ) -> Result<RemoteControlResult, Error> {
        BydClient::schedule_climate(self, vin, params, None).await
    }

    async fn set_seat_climate(
        &self,
        vin: &str,
        params: &SeatClimateParams,
    ) -> Result<RemoteControlResult, Error> {
        BydClient::set_seat_climate(self, vin, params, None).await
    }

    async fn set_battery_heat(
        &self,
        vin: &str,
        params: &BatteryHeatParams,
    ) -> Result<RemoteControlResult, Error> {
        BydClient::set_battery_heat(self, vin, params, None).await
    }

    async fn find_car(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::find_car(self, vin, None).await
    }

    async fn flash_lights(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::flash_lights(self, vin, None).await
    }

    async fn close_windows(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::close_windows(self, vin, None).await
    }

    async fn open_windows(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::open_windows(self, vin, None).await
    }

    async fn open_trunk(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::open_trunk(self, vin, None).await
    }

    async fn close_trunk(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        BydClient::close_trunk(self, vin, None).await
    }

    async fn get_vehicle_realtime(&self, vin: &str) -> Result<VehicleRealtimeData, Error> {
        BydClient::get_vehicle_realtime(self, vin, PollSettings::default()).await
    }

    async fn get_hvac_status(&self, vin: &str) -> Result<HvacStatus, Error> {
        BydClient::get_hvac_status(self, vin).await
    }

    async fn get_gps_info(&self, vin: &str) -> Result<GpsInfo, Error> {
        BydClient::get_gps_info(self, vin, PollSettings::default()).await
    }

    async fn get_charging_homepage(
        &self,
        vin: &str,
    ) -> Result<(ChargingStatus, SmartChargingSchedule), Error> {
        BydClient::get_charging_homepage(self, vin).await
    }

    async fn start_charging(&self, vin: &str) -> Result<ChargeChangeResult, Error> {
        BydClient::start_charging(self, vin, ChargePollSettings::default()).await
    }

    async fn stop_charging(&self, vin: &str) -> Result<ChargeChangeResult, Error> {
        BydClient::stop_charging(self, vin, ChargePollSettings::default()).await
    }

    async fn get_energy_consumption(&self, vin: &str) -> Result<EnergyConsumption, Error> {
        BydClient::get_energy_consumption(self, vin).await
    }

    async fn get_ac_current_limit(&self, vin: &str) -> Result<AcCurrentLimit, Error> {
        BydClient::get_ac_current_limit(self, vin, None).await
    }

    async fn save_charging_schedule(
        &self,
        vin: &str,
        start_charge_time: &str,
        end_charge_time: &str,
        charge_way: &str,
        enabled: bool,
    ) -> Result<ChargeChangeResult, Error> {
        BydClient::save_charging_schedule(
            self,
            vin,
            start_charge_time,
            end_charge_time,
            charge_way,
            enabled,
            ChargePollSettings::default(),
        )
        .await
    }

    async fn get_ota_version(&self, vin: &str) -> Result<OtaUpgradeInfo, Error> {
        BydClient::get_ota_version(self, vin).await
    }

    async fn set_ota_auto_upgrade(
        &self,
        vin: &str,
        enabled: bool,
        auto_upgrade_time: &str,
        command_pwd: Option<&str>,
    ) -> Result<CommandAck, Error> {
        BydClient::set_ota_auto_upgrade(self, vin, enabled, auto_upgrade_time, command_pwd).await
    }
}

/// Every method goes to the client while it lives, and fails as a closed
/// client does once it is gone.
impl<E: ClientEnv> CarBackend for ClientHandle<E> {
    async fn lock(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::lock(&self.live()?, vin).await
    }

    async fn unlock(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::unlock(&self.live()?, vin).await
    }

    async fn start_climate(
        &self,
        vin: &str,
        params: &ClimateStartParams,
    ) -> Result<RemoteControlResult, Error> {
        CarBackend::start_climate(&self.live()?, vin, params).await
    }

    async fn stop_climate(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::stop_climate(&self.live()?, vin).await
    }

    async fn schedule_climate(
        &self,
        vin: &str,
        params: &ClimateScheduleParams,
    ) -> Result<RemoteControlResult, Error> {
        CarBackend::schedule_climate(&self.live()?, vin, params).await
    }

    async fn set_seat_climate(
        &self,
        vin: &str,
        params: &SeatClimateParams,
    ) -> Result<RemoteControlResult, Error> {
        CarBackend::set_seat_climate(&self.live()?, vin, params).await
    }

    async fn set_battery_heat(
        &self,
        vin: &str,
        params: &BatteryHeatParams,
    ) -> Result<RemoteControlResult, Error> {
        CarBackend::set_battery_heat(&self.live()?, vin, params).await
    }

    async fn find_car(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::find_car(&self.live()?, vin).await
    }

    async fn flash_lights(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::flash_lights(&self.live()?, vin).await
    }

    async fn close_windows(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::close_windows(&self.live()?, vin).await
    }

    async fn open_windows(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::open_windows(&self.live()?, vin).await
    }

    async fn open_trunk(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::open_trunk(&self.live()?, vin).await
    }

    async fn close_trunk(&self, vin: &str) -> Result<RemoteControlResult, Error> {
        CarBackend::close_trunk(&self.live()?, vin).await
    }

    async fn get_vehicle_realtime(&self, vin: &str) -> Result<VehicleRealtimeData, Error> {
        CarBackend::get_vehicle_realtime(&self.live()?, vin).await
    }

    async fn get_hvac_status(&self, vin: &str) -> Result<HvacStatus, Error> {
        CarBackend::get_hvac_status(&self.live()?, vin).await
    }

    async fn get_gps_info(&self, vin: &str) -> Result<GpsInfo, Error> {
        CarBackend::get_gps_info(&self.live()?, vin).await
    }

    async fn get_charging_homepage(
        &self,
        vin: &str,
    ) -> Result<(ChargingStatus, SmartChargingSchedule), Error> {
        CarBackend::get_charging_homepage(&self.live()?, vin).await
    }

    async fn start_charging(&self, vin: &str) -> Result<ChargeChangeResult, Error> {
        CarBackend::start_charging(&self.live()?, vin).await
    }

    async fn stop_charging(&self, vin: &str) -> Result<ChargeChangeResult, Error> {
        CarBackend::stop_charging(&self.live()?, vin).await
    }

    async fn get_energy_consumption(&self, vin: &str) -> Result<EnergyConsumption, Error> {
        CarBackend::get_energy_consumption(&self.live()?, vin).await
    }

    async fn get_ac_current_limit(&self, vin: &str) -> Result<AcCurrentLimit, Error> {
        CarBackend::get_ac_current_limit(&self.live()?, vin).await
    }

    async fn save_charging_schedule(
        &self,
        vin: &str,
        start_charge_time: &str,
        end_charge_time: &str,
        charge_way: &str,
        enabled: bool,
    ) -> Result<ChargeChangeResult, Error> {
        CarBackend::save_charging_schedule(
            &self.live()?,
            vin,
            start_charge_time,
            end_charge_time,
            charge_way,
            enabled,
        )
        .await
    }

    async fn get_ota_version(&self, vin: &str) -> Result<OtaUpgradeInfo, Error> {
        CarBackend::get_ota_version(&self.live()?, vin).await
    }

    async fn set_ota_auto_upgrade(
        &self,
        vin: &str,
        enabled: bool,
        auto_upgrade_time: &str,
        command_pwd: Option<&str>,
    ) -> Result<CommandAck, Error> {
        CarBackend::set_ota_auto_upgrade(
            &self.live()?,
            vin,
            enabled,
            auto_upgrade_time,
            command_pwd,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use openbyd_protocol::config::BydConfig;
    use openbyd_protocol::models::control::{
        BatteryHeatParams, ClimateScheduleParams, ClimateStartParams, SeatClimateParams,
    };
    use serde_json::{Map, json};

    use openbyd_protocol::mqtt::MqttEvent;

    use super::*;
    use crate::client::{CarRouter, ClientOptions};
    use crate::scripted::{ScriptedEnv, block_on};

    /// Every backend method once; returns each call's error kind.
    fn every_call<B: CarBackend>(backend: &B) -> Vec<ErrorKind> {
        let vin = "TESTVIN0000000001";
        let empty = Map::new();
        let start = ClimateStartParams::from_kwargs(&empty).unwrap();
        let schedule = ClimateScheduleParams::from_kwargs(&empty).unwrap();
        let seat = SeatClimateParams::from_kwargs(&empty).unwrap();
        let mut on = Map::new();
        on.insert("on".into(), json!(true));
        let battery = BatteryHeatParams::from_kwargs(&on).unwrap();
        let kind = |result: Result<(), Error>| result.unwrap_err().kind();
        block_on(async {
            vec![
                kind(backend.lock(vin).await.map(drop)),
                kind(backend.unlock(vin).await.map(drop)),
                kind(backend.start_climate(vin, &start).await.map(drop)),
                kind(backend.stop_climate(vin).await.map(drop)),
                kind(backend.schedule_climate(vin, &schedule).await.map(drop)),
                kind(backend.set_seat_climate(vin, &seat).await.map(drop)),
                kind(backend.set_battery_heat(vin, &battery).await.map(drop)),
                kind(backend.find_car(vin).await.map(drop)),
                kind(backend.flash_lights(vin).await.map(drop)),
                kind(backend.close_windows(vin).await.map(drop)),
                kind(backend.open_windows(vin).await.map(drop)),
                kind(backend.open_trunk(vin).await.map(drop)),
                kind(backend.close_trunk(vin).await.map(drop)),
                kind(backend.get_vehicle_realtime(vin).await.map(drop)),
                kind(backend.get_hvac_status(vin).await.map(drop)),
                kind(backend.get_gps_info(vin).await.map(drop)),
                kind(backend.get_charging_homepage(vin).await.map(drop)),
                kind(backend.start_charging(vin).await.map(drop)),
                kind(backend.stop_charging(vin).await.map(drop)),
                kind(backend.get_energy_consumption(vin).await.map(drop)),
                kind(backend.get_ac_current_limit(vin).await.map(drop)),
                kind(
                    backend
                        .save_charging_schedule(vin, "22:00", "06:00", "e", true)
                        .await
                        .map(drop),
                ),
            ]
        })
        .expect("nothing waits")
    }

    fn closed_client() -> BydClient<Arc<ScriptedEnv>> {
        let mut config = BydConfig::new("user@example.com", "p");
        config.mqtt_enabled = false;
        let client = BydClient::new(
            config,
            Arc::new(ScriptedEnv::new(0, "s", vec![])),
            ClientOptions::default(),
        );
        client.async_close();
        client
    }

    #[test]
    fn the_client_and_its_handle_serve_every_backend_call() {
        let client = closed_client();
        let direct = every_call(&client);
        let handle = ClientHandle(Arc::downgrade(&client.inner));
        assert!(handle.clone().client().is_some());
        assert_eq!(every_call(&handle), direct);
        // Commands are refused before the transport (no verified PIN), the
        // charging stop always; everything else finds the client closed.
        assert!(direct.iter().all(|kind| kind.is_a(ErrorKind::BydError)));
        assert_eq!(direct[0], ErrorKind::BydControlPasswordError);
        assert_eq!(direct[14], ErrorKind::BydError);
        assert_eq!(direct[18], ErrorKind::BydEndpointNotSupportedError);
    }

    #[test]
    fn a_handle_whose_client_is_gone_fails_every_call() {
        let gone: ClientHandle<Arc<ScriptedEnv>> = ClientHandle(Weak::new());
        assert!(gone.client().is_none());
        assert!(
            every_call(&gone)
                .iter()
                .all(|kind| *kind == ErrorKind::BydError)
        );
        assert_eq!(
            ClientClock::<Arc<ScriptedEnv>>(Weak::new()).monotonic(),
            f64::INFINITY
        );
    }

    /// A router written before status pushes existed: it hears the other pushes only.
    #[derive(Default)]
    struct OldRouter(std::sync::atomic::AtomicUsize);

    impl CarRouter for OldRouter {
        fn handle_mqtt_realtime(&self, _vin: &str, _data: &VehicleRealtimeData) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn handle_mqtt_charging(&self, _vin: &str, _data: &ChargingStatus) {}

        fn handle_mqtt_energy(&self, _vin: &str, _data: &EnergyConsumption) {}
    }

    fn event(name: &str, respond_data: serde_json::Value) -> MqttEvent {
        let payload = json!({
            "event": name, "vin": "TESTVIN0000000001", "data": {"respondData": respond_data},
        });
        let serde_json::Value::Object(payload) = payload else {
            unreachable!()
        };
        MqttEvent {
            event: name.to_owned(),
            vin: Some("TESTVIN0000000001".to_owned()),
            topic: "t".to_owned(),
            payload,
        }
    }

    #[test]
    fn a_status_push_patches_the_kept_car() {
        let router = Arc::new(OldRouter::default());
        let mut config = BydConfig::new("user@example.com", "p");
        config.mqtt_enabled = false;
        let options = ClientOptions {
            car_router: Some(Arc::clone(&router) as Arc<dyn CarRouter>),
            ..ClientOptions::default()
        };
        let client = BydClient::new(config, Arc::new(ScriptedEnv::new(0, "s", vec![])), options);
        client.async_start();
        let vehicle = crate::api::vehicle::parse_model::<Vehicle>(
            &json!({"vin": "TESTVIN0000000001", "energyType": "0"}),
        )
        .unwrap();
        let car = block_on(client.get_car(
            "TESTVIN0000000001",
            GetCarOptions {
                vehicle: Some(vehicle),
                ..GetCarOptions::default()
            },
        ))
        .unwrap()
        .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let settle = || async {
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
            };
            // A status push before the car knows anything changes nothing.
            client.on_mqtt_event(&event(
                "vehicleStatusPush",
                json!({"can4A505Dto": {"elecPercent": 55}}),
            ));
            settle().await;
            assert!(car.state().realtime.is_none());

            client.on_mqtt_event(&event(
                "vehicleInfo",
                json!({"elecPercent": 70, "leftFrontDoor": 0, "totalMileage": 1234}),
            ));
            settle().await;
            assert_eq!(router.0.load(std::sync::atomic::Ordering::SeqCst), 1);

            client.on_mqtt_event(&event(
                "vehicleStatusPush",
                json!({
                    "doorStatusDto": {"leftFrontDoor": 1},
                    "can4A505Dto": {"elecPercent": 91},
                    "airStatusDto": {"cycleModel": 2},
                    "time": 1_700_000_000,
                }),
            ));
            // Neither a push without a known section nor one that does not validate reaches the car.
            client.on_mqtt_event(&event("vehicleStatusPush", json!({"time": 5})));
            client.on_mqtt_event(&event(
                "vehicleStatusPush",
                json!({"can4A505Dto": {"elecPercent": 12}, "chargeStatusDto": {"soc": "x"}}),
            ));
            client.on_mqtt_event(&event(
                "remoteControlAck",
                json!({"res": 1, "ackStatus": 1}),
            ));
            settle().await;
        });

        let state = car.state();
        let realtime = state.realtime.as_ref().unwrap().base();
        assert_eq!(realtime.elec_percent, Some(91.0));
        assert_eq!(realtime.total_mileage, Some(1234.0));
        assert_eq!(realtime.raw["leftFrontDoor"], json!(1));
        assert!(state.hvac.is_none(), "no HVAC section is known to patch");
        // The old router was not told about the status pushes.
        assert_eq!(router.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn get_car_keeps_one_car_per_vin_until_close() {
        let client = closed_client();
        client.async_start();
        let vehicle = crate::api::vehicle::parse_model::<Vehicle>(
            &json!({"vin": "TESTVIN0000000001", "energyType": "0"}),
        )
        .unwrap();
        let options = || GetCarOptions {
            vehicle: Some(vehicle.clone()),
            ..GetCarOptions::default()
        };
        // The capabilities fetch fails (no script): unknown capabilities, a car anyway.
        let car = block_on(client.get_car("TESTVIN0000000001", options()))
            .unwrap()
            .unwrap();
        assert_eq!(car.capabilities().source, "unknown:fetch_error");
        let again = block_on(client.get_car("TESTVIN0000000001", GetCarOptions::default()))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&car, &again));
        // A push for the car outside a runtime is dropped, as pyBYD drops one without a loop.
        let data: VehicleRealtimeData =
            crate::api::vehicle::parse_model(&json!({"elecPercent": 5})).unwrap();
        client.route_realtime("TESTVIN0000000001", &data);
        client.route_realtime("OTHER", &data);
        client.route_status_push("TESTVIN0000000001", &data, None, None);
        client.route_status_push("OTHER", &data, None, None);
        client.async_close();
        assert!(client.car("TESTVIN0000000001").is_none());
        let outcome =
            block_on(client.get_car("TESTVIN0000000002", GetCarOptions::default())).unwrap();
        assert_eq!(
            outcome.err().map(|error| error.kind()),
            Some(ErrorKind::BydError)
        );
    }
}
