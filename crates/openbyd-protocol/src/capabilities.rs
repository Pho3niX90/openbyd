//! The feature namespaces of the car layer (`pybyd/_capabilities/`).
//!
//! pyBYD's `BydCar` exposes each group of remote commands as a small object:
//! `car.lock`, `car.hvac`, `car.seat`, `car.steering`, `car.battery`,
//! `car.finder`, `car.windows` and `car.trunk`. Each knows whether the
//! vehicle supports it and, for every command, which client method to call
//! with which parameters and which state the command should show until the
//! car confirms it (its projections).
//!
//! This module holds that logic without any I/O. A command method such as
//! [`lock::LockCapability::lock`] checks availability and returns a
//! [`CommandPlan`]: the [`ProjectionSpec`]s to register and the
//! [`PlannedCommand`] to send. The car (`openbyd_client::car`) registers
//! the projections, turns the plan into a [`CommandCall`] against the
//! snapshot it then holds ([`CommandPlan::resolve`]; the seat commands read
//! every seat's current level there) and sends it.
//!
//! [`ProjectionSpec`]: crate::state::ProjectionSpec

pub mod battery_heat;
#[doc(hidden)]
pub mod calls;
mod command;
pub mod finder;
pub mod hvac;
pub mod lock;
pub mod seat;
pub mod steering;
pub mod trunk;
pub mod windows;

pub use command::{
    CommandCall, CommandPlan, PlannedCommand, projected_seat_state, unsupported, validation_error,
};

pub(crate) use calls::parity_call;
