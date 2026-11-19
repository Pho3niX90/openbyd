//! The feature objects of a car (`car.lock`, `car.hvac`, ...).
//!
//! Each wraps the pure capability of `openbyd_protocol::capabilities`, which
//! decides what to send and project, and runs its plans on the car.

use std::sync::Arc;

use openbyd_protocol::capabilities::{self as pure, hvac::HvacReading};
use openbyd_protocol::error::Error;
use openbyd_protocol::models::control::ClimateScheduleParams;
use openbyd_protocol::models::latest_config::VehicleCapabilities;

pub use openbyd_protocol::capabilities::hvac::ClimateStart;
pub use openbyd_protocol::capabilities::seat::{SeatLevel, SeatMode, SeatPosition};

use super::{CarBackend, CarInner, is_byd_error};

/// `car.lock`: lock and unlock the doors.
pub struct LockCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::lock::LockCapability,
}

impl<B: CarBackend> LockCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, available: Option<bool>) -> Self {
        Self {
            car: Arc::clone(car),
            pure: pure::lock::LockCapability::new(car.vin.clone(), available),
        }
    }

    /// Whether the vehicle supports locking.
    pub fn available(&self) -> bool {
        self.pure.available()
    }

    /// Lock all doors.
    pub async fn lock(&self) -> Result<(), Error> {
        self.car.execute(self.pure.lock()?).await
    }

    /// Unlock all doors.
    pub async fn unlock(&self) -> Result<(), Error> {
        self.car.execute(self.pure.unlock()?).await
    }
}

/// `car.hvac`: start, stop and schedule climate control.
pub struct HvacCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::hvac::HvacCapability,
}

impl<B: CarBackend> HvacCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, available: Option<bool>) -> Self {
        Self {
            car: Arc::clone(car),
            pure: pure::hvac::HvacCapability::new(car.vin.clone(), available),
        }
    }

    fn reading(&self) -> Option<HvacReading> {
        self.car
            .snapshot()
            .hvac
            .as_ref()
            .map(HvacReading::from_projected)
    }

    /// Whether the vehicle supports remote climate control.
    pub fn available(&self) -> bool {
        self.pure.available()
    }

    /// °C offset of the 1-17 scale, from the last HVAC status if known.
    pub fn temp_offset(&self) -> f64 {
        pure::hvac::HvacCapability::temp_offset(self.reading().as_ref())
    }

    /// The settable temperature range in °C.
    pub fn temp_range(&self) -> (f64, f64) {
        pure::hvac::HvacCapability::temp_range(self.reading().as_ref())
    }

    /// Start climate control, or change the settings of a running session.
    ///
    /// Without a known HVAC status the status is fetched first (and merged),
    /// so the temperature scale fits the car's range; if that fetch fails
    /// with a `BydError` the default 15-31 °C scale is used. See
    /// [`pure::hvac::HvacCapability::start`] for how a running session's
    /// settings are kept.
    pub async fn start(&self, start: ClimateStart) -> Result<(), Error> {
        let time_span = self.pure.check_start(&start)?;
        let mut reading = self.reading();
        if reading.is_none() {
            reading = match self.car.update_hvac().await {
                Ok(status) => Some(HvacReading::from_status(&status)),
                Err(error) if is_byd_error(&error) => None,
                Err(error) => return Err(error),
            };
        }
        let plan = self.pure.start(&start, time_span, reading.as_ref())?;
        self.car.execute(plan).await
    }

    /// Stop climate control, including the seat and steering wheel heating.
    pub async fn stop(&self) -> Result<(), Error> {
        self.car.execute(self.pure.stop()?).await
    }

    /// Create, change or remove a climate schedule.
    pub async fn schedule(&self, params: ClimateScheduleParams) -> Result<(), Error> {
        self.car.execute(self.pure.schedule(params)?).await
    }
}

/// `car.seat`: seat heating and ventilation.
pub struct SeatCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::seat::SeatCapability,
}

impl<B: CarBackend> SeatCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, flags: &VehicleCapabilities) -> Self {
        let availability = pure::seat::SeatAvailability {
            driver_heat: flags.driver_seat_heat,
            driver_ventilation: flags.driver_seat_ventilation,
            passenger_heat: flags.passenger_seat_heat,
            passenger_ventilation: flags.passenger_seat_ventilation,
            rear_left_heat: flags.rear_left_seat_heat,
            rear_left_ventilation: flags.rear_left_seat_ventilation,
            rear_right_heat: flags.rear_right_seat_heat,
            rear_right_ventilation: flags.rear_right_seat_ventilation,
        };
        Self {
            car: Arc::clone(car),
            pure: pure::seat::SeatCapability::new(car.vin.clone(), availability),
        }
    }

    /// Whether the vehicle supports `mode` on the seat at `position`.
    pub fn is_mode_available(&self, position: SeatPosition, mode: SeatMode) -> bool {
        self.pure.is_mode_available(position, mode)
    }

    /// Set the heating level of one seat; the others keep their current level.
    pub async fn heat(&self, position: SeatPosition, level: SeatLevel) -> Result<(), Error> {
        self.car.execute(self.pure.heat(position, level)?).await
    }

    /// Set the ventilation level of one seat; the others keep their current level.
    pub async fn ventilation(&self, position: SeatPosition, level: SeatLevel) -> Result<(), Error> {
        self.car
            .execute(self.pure.ventilation(position, level)?)
            .await
    }
}

/// `car.steering`: steering wheel heating.
pub struct SteeringCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::steering::SteeringCapability,
}

impl<B: CarBackend> SteeringCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, available: Option<bool>) -> Self {
        Self {
            car: Arc::clone(car),
            pure: pure::steering::SteeringCapability::new(car.vin.clone(), available),
        }
    }

    /// Whether the vehicle supports steering wheel heating.
    pub fn available(&self) -> bool {
        self.pure.available()
    }

    /// Steering wheel heating on or off.
    pub async fn heat(&self, on: bool) -> Result<(), Error> {
        self.car.execute(self.pure.heat(on)?).await
    }
}

/// `car.battery`: battery heating.
pub struct BatteryHeatCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::battery_heat::BatteryHeatCapability,
}

impl<B: CarBackend> BatteryHeatCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, available: Option<bool>) -> Self {
        Self {
            car: Arc::clone(car),
            pure: pure::battery_heat::BatteryHeatCapability::new(car.vin.clone(), available),
        }
    }

    /// Whether the vehicle supports battery heating.
    pub fn available(&self) -> bool {
        self.pure.available()
    }

    /// Battery heating on or off.
    pub async fn heat(&self, on: bool) -> Result<(), Error> {
        self.car.execute(self.pure.heat(on)?).await
    }
}

/// `car.finder`: find my car and flash the lights.
pub struct FinderCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::finder::FinderCapability,
}

impl<B: CarBackend> FinderCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, find: Option<bool>, flash: Option<bool>) -> Self {
        Self {
            car: Arc::clone(car),
            pure: pure::finder::FinderCapability::new(car.vin.clone(), find, flash),
        }
    }

    /// Whether the vehicle supports find-my-car.
    pub fn find_available(&self) -> bool {
        self.pure.find_available()
    }

    /// Whether the vehicle supports flashing the lights.
    pub fn flash_available(&self) -> bool {
        self.pure.flash_available()
    }

    /// Find-my-car: horn and lights.
    pub async fn find(&self) -> Result<(), Error> {
        self.car.execute(self.pure.find()?).await
    }

    /// Flash the lights.
    pub async fn flash_lights(&self) -> Result<(), Error> {
        self.car.execute(self.pure.flash_lights()?).await
    }
}

/// `car.windows`: close the windows or crack them open.
pub struct WindowsCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::windows::WindowsCapability,
}

impl<B: CarBackend> WindowsCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, close: Option<bool>, open: Option<bool>) -> Self {
        Self {
            car: Arc::clone(car),
            pure: pure::windows::WindowsCapability::new(car.vin.clone(), close, open, true),
        }
    }

    /// Whether the vehicle can close its windows remotely.
    pub fn close_available(&self) -> bool {
        self.pure.close_available()
    }

    /// Whether the vehicle can crack its windows open remotely (the
    /// capability flag and the command gate, which reads the vehicle's learn info).
    pub fn open_available(&self) -> bool {
        self.pure.open_available()
    }

    /// Close all windows.
    pub async fn close(&self) -> Result<(), Error> {
        self.car.execute(self.pure.close()?).await
    }

    /// Crack all windows to about 10 %.
    pub async fn open(&self) -> Result<(), Error> {
        self.car.execute(self.pure.open()?).await
    }
}

/// `car.trunk`: open and close the trunk.
pub struct TrunkCapability<B> {
    car: Arc<CarInner<B>>,
    pure: pure::trunk::TrunkCapability,
}

impl<B: CarBackend> TrunkCapability<B> {
    pub(crate) fn new(car: &Arc<CarInner<B>>, open: Option<bool>, close: Option<bool>) -> Self {
        Self {
            car: Arc::clone(car),
            pure: pure::trunk::TrunkCapability::new(car.vin.clone(), open, close),
        }
    }

    /// Whether the vehicle can release the trunk latch.
    pub fn open_available(&self) -> bool {
        self.pure.open_available()
    }

    /// Whether the vehicle can lower the trunk electrically.
    pub fn close_available(&self) -> bool {
        self.pure.close_available()
    }

    /// Release the trunk latch.
    pub async fn open(&self) -> Result<(), Error> {
        self.car.execute(self.pure.open()?).await
    }

    /// Lower the trunk.
    pub async fn close(&self) -> Result<(), Error> {
        self.car.execute(self.pure.close()?).await
    }
}
