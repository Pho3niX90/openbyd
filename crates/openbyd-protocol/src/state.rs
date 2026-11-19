//! Merging of HTTP and MQTT data into one vehicle state (`pybyd/_state_engine.py`, `_validators.py`).
//!
//! [`VehicleStateEngine`] keeps one vehicle's state as a [`VehicleSnapshot`]:
//! each `update_*` method merges one section (realtime, HVAC, GPS, charging,
//! energy, charging schedule) after running it through the quality filters
//! in [`validators`], and commands lay their expected outcome over the
//! accepted data as projections until a payload confirms them, they expire
//! or the command is rolled back.
//!
//! Nothing here reads the clock: the methods take the monotonic time.

mod calls;
pub mod engine;
pub mod validators;

pub use engine::{
    DEFAULT_PROJECTION_TTL, FieldProjection, Projected, ProjectionSpec, SectionModel,
    StateChangedCallback, VehicleSnapshot, VehicleStateEngine,
};
pub use validators::{
    RealtimeOverride, RealtimeUpdates, apply_gps_filters, apply_hvac_filters,
    apply_realtime_filters, guard_gps_coordinates, has_valid_coordinates,
};

pub(crate) use calls::parity_call;

#[cfg(test)]
mod tests;
