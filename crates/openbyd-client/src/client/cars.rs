//! The car layer on the client: pyBYD's `get_car`, the client as a
//! [`CarBackend`], and the cars' share of the MQTT pushes.
//!
//! [`BydClient::get_car`] builds one [`BydCar`] per VIN and keeps it, as
//! pyBYD's `_cars` does; MQTT `vehicleInfo`, `smartCharge` and
//! `energyConsumption` pushes for that VIN are merged into it.
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
use openbyd_protocol::models::control::{
    BatteryHeatParams, ChargeChangeResult, ClimateScheduleParams, ClimateStartParams,
    RemoteControlResult, SeatClimateParams,
};
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::gps::GpsInfo;
use openbyd_protocol::models::hvac::HvacStatus;
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
}
