//! One object per vehicle: merged state, features and commands (`pybyd/car.py`).
//!
//! [`BydCar`] holds a vehicle's state in a [`VehicleStateEngine`] and exposes
//! each group of commands as a field, as pyBYD does:
//!
//! ```ignore
//! car.lock.lock().await?;
//! car.hvac.start(ClimateStart { temperature: 21.0, duration: 20, ..Default::default() }).await?;
//! car.seat.heat(SeatPosition::Driver, SeatLevel::High).await?;
//! car.steering.heat(true).await?;
//! let state = car.state();
//! ```
//!
//! A command registers the state it expects (its projections) before it is
//! sent and rolls them back when it fails. Reads (`update_*`) fetch a section
//! and merge it; MQTT pushes (`handle_mqtt_*`) are merged on a task of the
//! running Tokio runtime, after any command in flight.
//!
//! `BydCar` does not depend on the client: it is generic over a
//! [`CarBackend`], the client methods the car layer calls.

mod backend;
mod features;
#[doc(hidden)]
pub mod parity;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex as SyncMutex, PoisonError, RwLock};
use std::time::Instant;

use openbyd_protocol::capabilities::{CommandCall, CommandPlan};
use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::charging::ChargingStatus;
use openbyd_protocol::models::command_gating::{GuestScope, evaluate_command_gate};
use openbyd_protocol::models::control::{ChargeChangeResult, RemoteCommand};
use openbyd_protocol::models::energy::EnergyConsumption;
use openbyd_protocol::models::gps::GpsInfo;
use openbyd_protocol::models::hvac::HvacStatus;
use openbyd_protocol::models::latest_config::VehicleCapabilities;
use openbyd_protocol::models::ota::OtaUpgradeInfo;
use openbyd_protocol::models::realtime::VehicleRealtimeData;
use openbyd_protocol::models::smart_charging::AcCurrentLimit;
use openbyd_protocol::models::vehicle::Vehicle;
use openbyd_protocol::state::{
    DEFAULT_PROJECTION_TTL, StateChangedCallback, VehicleSnapshot, VehicleStateEngine,
};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

pub use backend::CarBackend;
pub use features::{
    BatteryHeatCapability, ClimateStart, FinderCapability, HvacCapability, LockCapability,
    SeatCapability, SeatLevel, SeatMode, SeatPosition, SteeringCapability, TrunkCapability,
    WindowsCapability,
};

/// The monotonic clock projections are timed with (pyBYD's `time.monotonic()`).
pub trait Clock: Send + Sync {
    /// Seconds since an arbitrary fixed point.
    fn monotonic(&self) -> f64;
}

/// The system's monotonic clock, counted from when the value was made and
/// starting at [`MONOTONIC_ORIGIN_S`](crate::http::MONOTONIC_ORIGIN_S), never near 0.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock(Instant);

impl SystemClock {
    /// A clock starting now.
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        crate::http::MONOTONIC_ORIGIN_S + self.0.elapsed().as_secs_f64()
    }
}

/// The optional arguments of [`BydCar::new`].
pub struct CarOptions {
    /// The vehicle's capability flags. `None` assumes the common features
    /// (pyBYD's `implicit_default`).
    pub capabilities: Option<VehicleCapabilities>,
    /// Called with the VIN and the new snapshot after every state change.
    pub on_state_changed: Option<StateChangedCallback>,
    /// How long a command's projections stay active, in seconds.
    pub projection_ttl: f64,
    /// The clock projections are timed with.
    pub clock: Arc<dyn Clock>,
}

impl Default for CarOptions {
    fn default() -> Self {
        Self {
            capabilities: None,
            on_state_changed: None,
            projection_ttl: DEFAULT_PROJECTION_TTL,
            clock: Arc::new(SystemClock::new()),
        }
    }
}

/// The capability flags pyBYD assumes when it is given none.
pub fn implicit_capabilities(vin: &str) -> VehicleCapabilities {
    let raw = json!({
        "vin": vin,
        "source": "implicit_default",
        "lock": true,
        "unlock": true,
        "climate": true,
        "car_on": true,
        "battery_heat": true,
        "steering_wheel_heat": true,
        "driver_seat_heat": true,
        "driver_seat_ventilation": true,
        "passenger_seat_heat": true,
        "passenger_seat_ventilation": true,
        "find_car": true,
        "flash_lights": true,
        "close_windows": true,
        "location": true,
    });
    VehicleCapabilities::parse(&raw, &Value::Null).expect("the implicit capabilities validate")
}

/// What the car and its feature objects share.
pub(crate) struct CarInner<B> {
    backend: B,
    vin: String,
    /// The engine, locked for every merge and for the whole of a command (pyBYD's `engine.lock`).
    engine: Mutex<VehicleStateEngine>,
    /// The latest snapshot, readable without waiting for a command to finish.
    snapshot: Arc<RwLock<VehicleSnapshot>>,
    /// The MQTT pushes not merged yet, oldest first.
    pushes: SyncMutex<VecDeque<PendingMerge>>,
    clock: Arc<dyn Clock>,
}

/// A merge waiting for the engine lock.
type PendingMerge = Box<dyn FnOnce(&mut VehicleStateEngine, f64) + Send>;

impl<B: CarBackend> CarInner<B> {
    fn now(&self) -> f64 {
        self.clock.monotonic()
    }

    pub(crate) fn snapshot(&self) -> VehicleSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Runs a command with its projections (pyBYD's `_execute_command`).
    ///
    /// Holds the engine lock throughout, so commands and merges never
    /// interleave. The projections are registered first; the call is built
    /// from the snapshot that then shows (a seat command reads the other
    /// seats' levels there); on any error the projections are rolled back
    /// and the error returned. On success they stay until a payload confirms
    /// them or they expire. A command dropped before it finishes keeps its
    /// projections until they expire, as a cancelled pyBYD command does.
    pub(crate) async fn execute(&self, plan: CommandPlan) -> Result<(), Error> {
        let mut engine = self.engine.lock().await;
        let command_id = if plan.specs.is_empty() {
            None
        } else {
            Some(engine.register_projections(&plan.specs, self.now()))
        };
        let call = plan.resolve(engine.snapshot());
        let outcome = self.send(&call).await;
        if outcome.is_err()
            && let Some(command_id) = command_id
        {
            engine.rollback_projections(&command_id, self.now());
        }
        outcome
    }

    async fn send(&self, call: &CommandCall) -> Result<(), Error> {
        let (backend, vin) = (&self.backend, self.vin.as_str());
        match call {
            CommandCall::Lock => backend.lock(vin).await.map(drop),
            CommandCall::Unlock => backend.unlock(vin).await.map(drop),
            CommandCall::StartClimate(params) => backend.start_climate(vin, params).await.map(drop),
            CommandCall::StopClimate => backend.stop_climate(vin).await.map(drop),
            CommandCall::ScheduleClimate(params) => {
                backend.schedule_climate(vin, params).await.map(drop)
            }
            CommandCall::SetSeatClimate(params) => {
                backend.set_seat_climate(vin, params).await.map(drop)
            }
            CommandCall::SetBatteryHeat(params) => {
                backend.set_battery_heat(vin, params).await.map(drop)
            }
            CommandCall::FindCar => backend.find_car(vin).await.map(drop),
            CommandCall::FlashLights => backend.flash_lights(vin).await.map(drop),
            CommandCall::CloseWindows => backend.close_windows(vin).await.map(drop),
            CommandCall::OpenWindows => backend.open_windows(vin).await.map(drop),
            CommandCall::OpenTrunk => backend.open_trunk(vin).await.map(drop),
            CommandCall::CloseTrunk => backend.close_trunk(vin).await.map(drop),
        }
    }

    /// Merges under the engine lock, reading the clock once it is held.
    pub(crate) async fn merge(&self, merge: impl FnOnce(&mut VehicleStateEngine, f64)) {
        let mut engine = self.engine.lock().await;
        let now = self.now();
        merge(&mut engine, now);
    }

    pub(crate) async fn update_hvac(&self) -> Result<HvacStatus, Error> {
        let data = self.backend.get_hvac_status(&self.vin).await?;
        let merged = data.clone();
        self.merge(move |engine, now| engine.update_hvac(merged, now))
            .await;
        Ok(data)
    }

    /// Merges `data` on a task of the running Tokio runtime; without one it is dropped.
    ///
    /// Pushes are merged in the order they were handed over, as pyBYD's
    /// event loop runs their tasks: an older partial push must not land on
    /// top of a newer full one. A runtime with several workers may start
    /// the tasks in any order, so each task drains the queue oldest first
    /// under the engine lock instead of carrying its own push. Draining
    /// also picks up any push whose task was cancelled when an earlier
    /// runtime shut down.
    fn spawn_merge(
        self: &Arc<Self>,
        merge: impl FnOnce(&mut VehicleStateEngine, f64) + Send + 'static,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.pushes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(Box::new(merge));
        let inner = Arc::clone(self);
        runtime.spawn(async move { inner.drain_pushes().await });
    }

    /// Merges every waiting push in arrival order under the engine lock.
    ///
    /// Releases the queue lock before running each merge, since a state
    /// callback may hand in another push.
    async fn drain_pushes(&self) {
        let mut engine = self.engine.lock().await;
        loop {
            let next = self
                .pushes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            let Some(merge) = next else {
                break;
            };
            let now = self.now();
            merge(&mut engine, now);
        }
    }
}

/// One vehicle: its merged state and its features.
///
/// Methods take `&self` and the car is `Send + Sync` (for a `Send + Sync`
/// backend), so it can be shared between tasks behind an `Arc`.
pub struct BydCar<B: CarBackend> {
    inner: Arc<CarInner<B>>,
    capabilities: VehicleCapabilities,
    /// Lock and unlock.
    pub lock: LockCapability<B>,
    /// Climate control.
    pub hvac: HvacCapability<B>,
    /// Seat heating and ventilation.
    pub seat: SeatCapability<B>,
    /// Steering wheel heating.
    pub steering: SteeringCapability<B>,
    /// Battery heating.
    pub battery: BatteryHeatCapability<B>,
    /// Find my car and flash the lights.
    pub finder: FinderCapability<B>,
    /// Close the windows or crack them open.
    pub windows: WindowsCapability<B>,
    /// Open and close the trunk.
    pub trunk: TrunkCapability<B>,
}

impl<B: CarBackend> BydCar<B> {
    /// The car for `vin`, talking to the BYD cloud through `backend`.
    ///
    /// Fails only when the vehicle's `vehicleFunLearnInfo` holds a value the
    /// window gate cannot compare (a `TypeError`, as in pyBYD).
    pub fn new(
        backend: B,
        vin: impl Into<String>,
        vehicle: Vehicle,
        options: CarOptions,
    ) -> Result<Self, Error> {
        let vin = vin.into();
        // A shared guest's grant masks what the car offers (pyBYD #103).
        let guest_permissions = vehicle
            .is_shared()
            .then(|| vehicle.granted_permission_codes());
        let capabilities = options
            .capabilities
            .unwrap_or_else(|| implicit_capabilities(&vin))
            .with_guest_permissions(vehicle.is_shared(), guest_permissions.as_ref());
        // Opening the windows also needs openWindowLearnInfo, which the
        // latest config does not see, so the command gate is asked (#20);
        // only when the flag is set, as pyBYD's `and` does.
        let open_windows = capabilities.open_windows.unwrap_or(false) && {
            let learn_info: Map<String, Value> = vehicle
                .vehicle_fun_learn_info
                .iter()
                .map(|(key, value)| (key.clone(), json!(value)))
                .collect();
            evaluate_command_gate(
                RemoteCommand::OpenWindows,
                &capabilities,
                None,
                (!learn_info.is_empty()).then_some(&learn_info),
                GuestScope::new(vehicle.is_shared(), guest_permissions.as_ref()),
            )
            .map_err(openbyd_protocol::capabilities::validation_error)?
            .supported
        };

        let mut engine = VehicleStateEngine::new(vin.clone(), vehicle, options.projection_ttl);
        let snapshot = Arc::new(RwLock::new(engine.snapshot().clone()));
        let shared = Arc::clone(&snapshot);
        let mut user = options.on_state_changed;
        engine.set_on_state_changed(Some(Box::new(move |vin, state| {
            *shared.write().unwrap_or_else(PoisonError::into_inner) = state.clone();
            if let Some(callback) = user.as_mut() {
                callback(vin, state);
            }
        })));
        let inner = Arc::new(CarInner {
            backend,
            vin: vin.clone(),
            engine: Mutex::new(engine),
            snapshot,
            pushes: SyncMutex::new(VecDeque::new()),
            clock: options.clock,
        });
        Ok(Self {
            lock: LockCapability::new(&inner, capabilities.lock),
            hvac: HvacCapability::new(&inner, capabilities.climate),
            seat: SeatCapability::new(&inner, &capabilities),
            steering: SteeringCapability::new(&inner, capabilities.steering_wheel_heat),
            battery: BatteryHeatCapability::new(&inner, capabilities.battery_heat),
            finder: FinderCapability::new(&inner, capabilities.find_car, capabilities.flash_lights),
            windows: WindowsCapability::new(&inner, capabilities.close_windows, Some(open_windows)),
            trunk: TrunkCapability::new(&inner, capabilities.open_trunk, capabilities.close_trunk),
            capabilities,
            inner,
        })
    }

    /// The vehicle identification number.
    pub fn vin(&self) -> &str {
        &self.inner.vin
    }

    /// The current state with active projections laid over it.
    pub fn state(&self) -> VehicleSnapshot {
        self.inner.snapshot()
    }

    /// The vehicle's capability flags.
    pub fn capabilities(&self) -> &VehicleCapabilities {
        &self.capabilities
    }

    /// Fetches realtime data and merges it. Returns the data as fetched.
    pub async fn update_realtime(&self) -> Result<VehicleRealtimeData, Error> {
        let data = self
            .inner
            .backend
            .get_vehicle_realtime(&self.inner.vin)
            .await?;
        let merged = data.clone();
        self.inner
            .merge(move |engine, now| engine.update_realtime(merged, now))
            .await;
        Ok(data)
    }

    /// Fetches the HVAC status and merges it. Returns the status as fetched.
    pub async fn update_hvac(&self) -> Result<HvacStatus, Error> {
        self.inner.update_hvac().await
    }

    /// Fetches the location and merges it. Returns the fix as fetched.
    pub async fn update_gps(&self) -> Result<GpsInfo, Error> {
        let data = self.inner.backend.get_gps_info(&self.inner.vin).await?;
        let merged = data.clone();
        self.inner
            .merge(move |engine, now| engine.update_gps(merged, now))
            .await;
        Ok(data)
    }

    /// Fetches the live charging state and the schedule in one call and
    /// merges both. Returns the charging state as fetched.
    ///
    /// MQTT `smartCharge` pushes carry only the live state, so only this
    /// refresh touches the schedule section.
    pub async fn update_charging(&self) -> Result<ChargingStatus, Error> {
        let (status, schedule) = self
            .inner
            .backend
            .get_charging_homepage(&self.inner.vin)
            .await?;
        let merged = status.clone();
        self.inner
            .merge(move |engine, now| engine.update_charging(merged, now))
            .await;
        self.inner
            .merge(move |engine, now| engine.update_charging_schedule(schedule, now))
            .await;
        Ok(status)
    }

    /// Starts charging now and waits for the toggle to settle.
    pub async fn start_charging(&self) -> Result<ChargeChangeResult, Error> {
        self.inner.backend.start_charging(&self.inner.vin).await
    }

    /// Asks the client to stop charging, which pyBYD's client always refuses
    /// (`BydEndpointNotSupportedError`): the cloud's stop request reports
    /// success without stopping. A double unlock with the key fob stops a charge.
    pub async fn stop_charging(&self) -> Result<ChargeChangeResult, Error> {
        self.inner.backend.stop_charging(&self.inner.vin).await
    }

    /// Fetches energy consumption and merges it. Returns the data as fetched.
    pub async fn update_energy(&self) -> Result<EnergyConsumption, Error> {
        let data = self
            .inner
            .backend
            .get_energy_consumption(&self.inner.vin)
            .await?;
        let merged = data.clone();
        self.inner
            .merge(move |engine, now| engine.update_energy(merged, now))
            .await;
        Ok(data)
    }

    /// Fetches the supported and active AC charging current limits.
    pub async fn get_ac_current_limit(&self) -> Result<AcCurrentLimit, Error> {
        self.inner
            .backend
            .get_ac_current_limit(&self.inner.vin)
            .await
    }

    /// Fetches the OTA firmware version and night auto-upgrade settings.
    pub async fn get_ota_version(&self) -> Result<OtaUpgradeInfo, Error> {
        self.inner.backend.get_ota_version(&self.inner.vin).await
    }

    /// Sets the night auto-upgrade (pyBYD's default time is `"2:00"`).
    pub async fn set_ota_auto_upgrade(
        &self,
        enabled: bool,
        auto_upgrade_time: &str,
        command_pwd: Option<&str>,
    ) -> Result<(), Error> {
        self.inner
            .backend
            .set_ota_auto_upgrade(&self.inner.vin, enabled, auto_upgrade_time, command_pwd)
            .await
            .map(drop)
    }

    /// Saves a smart-charging schedule, waits for the cloud to confirm it,
    /// then refreshes the charging state and schedule ([`Self::update_charging`]).
    pub async fn save_charging_schedule(
        &self,
        start_charge_time: &str,
        end_charge_time: &str,
        charge_way: &str,
        enabled: bool,
    ) -> Result<ChargeChangeResult, Error> {
        let result = self
            .inner
            .backend
            .save_charging_schedule(
                &self.inner.vin,
                start_charge_time,
                end_charge_time,
                charge_way,
                enabled,
            )
            .await?;
        self.update_charging().await?;
        Ok(result)
    }

    /// Merges an MQTT `vehicleInfo` push, through the same filters and guard
    /// window as a fetch.
    ///
    /// The merge runs on a new task of the current Tokio runtime, after any
    /// command in flight; called outside a runtime, the push is dropped.
    pub fn handle_mqtt_realtime(&self, data: VehicleRealtimeData) {
        self.inner
            .spawn_merge(move |engine, now| engine.update_realtime(data, now));
    }

    /// Merges an MQTT `vehicleStatusPush`: a partial update, laid over the
    /// sections already known instead of replacing them
    /// ([`VehicleStateEngine::patch_status_push`]). `data` is the realtime
    /// part; `hvac` and `charging` are there when the push had their section.
    ///
    /// Like [`Self::handle_mqtt_realtime`], the merge runs on a new task of
    /// the current Tokio runtime, and the push is dropped outside one.
    pub fn handle_mqtt_status_push(
        &self,
        data: VehicleRealtimeData,
        hvac: Option<HvacStatus>,
        charging: Option<ChargingStatus>,
    ) {
        self.inner.spawn_merge(move |engine, now| {
            engine.patch_status_push(Some(data), hvac, charging, now);
        });
    }

    /// Merges an MQTT `smartCharge` push (live charging state only), like
    /// [`Self::handle_mqtt_realtime`].
    pub fn handle_mqtt_charging(&self, data: ChargingStatus) {
        self.inner
            .spawn_merge(move |engine, now| engine.update_charging(data, now));
    }

    /// Merges an MQTT `energyConsumption` push, like [`Self::handle_mqtt_realtime`].
    pub fn handle_mqtt_energy(&self, data: EnergyConsumption) {
        self.inner
            .spawn_merge(move |engine, now| engine.update_energy(data, now));
    }

    /// Releases the car's resources. Nothing to release yet; kept as the
    /// lifecycle hook pyBYD has.
    pub fn close(&self) {}
}

/// Whether an error is a `BydError` (pyBYD's `except BydError`).
pub(crate) fn is_byd_error(error: &Error) -> bool {
    error.is_a(ErrorKind::BydError)
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "testing"))]
mod e2e;
