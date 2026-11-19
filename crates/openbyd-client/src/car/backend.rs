//! The client methods the car layer calls.

use std::future::Future;

use openbyd_protocol::error::Error;
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

/// What [`super::BydCar`] needs from the client: the `BydClient` methods
/// pyBYD's `BydCar` and its capabilities call, with the same names and
/// arguments (keyword arguments the car never passes, such as
/// `command_pwd` or the polling settings, are left to the client's defaults).
///
/// The futures are `Send` and the methods take `&self`, so one backend can
/// serve several cars and tasks. Remote commands return the client's
/// result, which the car ignores: a command that did not work is an error.
pub trait CarBackend: Send + Sync + 'static {
    /// `client.lock(vin)`.
    fn lock(&self, vin: &str) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.unlock(vin)`.
    fn unlock(&self, vin: &str) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.start_climate(vin, params=params)`.
    fn start_climate(
        &self,
        vin: &str,
        params: &ClimateStartParams,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.stop_climate(vin)`.
    fn stop_climate(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.schedule_climate(vin, params=params)`.
    fn schedule_climate(
        &self,
        vin: &str,
        params: &ClimateScheduleParams,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.set_seat_climate(vin, params=params)`, also used for steering wheel heating.
    fn set_seat_climate(
        &self,
        vin: &str,
        params: &SeatClimateParams,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.set_battery_heat(vin, params=params)`.
    fn set_battery_heat(
        &self,
        vin: &str,
        params: &BatteryHeatParams,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.find_car(vin)`.
    fn find_car(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.flash_lights(vin)`.
    fn flash_lights(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.close_windows(vin)`.
    fn close_windows(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.open_windows(vin)`.
    fn open_windows(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.open_trunk(vin)`.
    fn open_trunk(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.close_trunk(vin)`.
    fn close_trunk(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<RemoteControlResult, Error>> + Send;

    /// `client.get_vehicle_realtime(vin)`.
    fn get_vehicle_realtime(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<VehicleRealtimeData, Error>> + Send;

    /// `client.get_hvac_status(vin)`.
    fn get_hvac_status(&self, vin: &str) -> impl Future<Output = Result<HvacStatus, Error>> + Send;

    /// `client.get_gps_info(vin)`.
    fn get_gps_info(&self, vin: &str) -> impl Future<Output = Result<GpsInfo, Error>> + Send;

    /// `client.get_charging_homepage(vin)`: the live charging state and the
    /// schedule from one `homePage` reply.
    fn get_charging_homepage(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<(ChargingStatus, SmartChargingSchedule), Error>> + Send;

    /// `client.start_charging(vin)`.
    fn start_charging(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<ChargeChangeResult, Error>> + Send;

    /// `client.stop_charging(vin)`.
    fn stop_charging(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<ChargeChangeResult, Error>> + Send;

    /// `client.get_energy_consumption(vin)`.
    fn get_energy_consumption(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<EnergyConsumption, Error>> + Send;

    /// `client.get_ac_current_limit(vin)`.
    fn get_ac_current_limit(
        &self,
        vin: &str,
    ) -> impl Future<Output = Result<AcCurrentLimit, Error>> + Send;

    /// `client.save_charging_schedule(vin, start_charge_time=..., end_charge_time=..., charge_way=..., enabled=...)`.
    fn save_charging_schedule(
        &self,
        vin: &str,
        start_charge_time: &str,
        end_charge_time: &str,
        charge_way: &str,
        enabled: bool,
    ) -> impl Future<Output = Result<ChargeChangeResult, Error>> + Send;
}
