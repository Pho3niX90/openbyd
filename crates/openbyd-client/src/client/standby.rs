//! The preconditioning plans of pyBYD's `BydClient`: `get_standby_plans`,
//! `save_standby_plan`, `create_standby_plan`, `set_default_standby_plan`,
//! `delete_standby_plan` and `run_standby_plan`.
//!
//! These create, change, delete and run climate plans on real cars. Every
//! request, re-read and wait follows pyBYD; the client parity cases in
//! `parity/groups/client_standby.py` hold them to it.

use openbyd_protocol::error::{Error, ErrorKind};
use openbyd_protocol::models::base::BydModel;
use openbyd_protocol::models::standby::{
    COMMAND_CLIMATE, PLAN_TYPE_USER, StandbyPlan, StandbyPlanCommand, StandbyPlanList,
    StandbyPlanRunResult, StandbySchedule,
};
use openbyd_protocol::parity::{Args, ParityError};
use serde_json::{Value, json};

use super::{BydClient, ReadFetch, TriggeredRead};
use crate::api::control::model_error;
use crate::api::standby::{
    SEND_ENDPOINT, SEND_RESULT_ENDPOINT, delete_standby_plan, fetch_standby_plans,
    make_send_fetch_fn, save_standby_plan, set_default_standby_plan,
};
use crate::api::vehicle::parse_model;
use crate::env::ClientEnv;

/// How long [`BydClient::run_standby_plan`] polls for: pyBYD's
/// `poll_attempts` and `poll_interval` keywords.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunPollSettings {
    /// How many times `send-result` is asked (10).
    pub poll_attempts: i64,
    /// Seconds between two polls (1.5).
    pub poll_interval: f64,
}

impl Default for RunPollSettings {
    fn default() -> Self {
        Self {
            poll_attempts: 10,
            poll_interval: 1.5,
        }
    }
}

impl<E: ClientEnv> BydClient<E> {
    /// Fetches the preconditioning plans app 3.6.0 schedules climate with
    /// (pyBYD's `get_standby_plans`).
    pub async fn get_standby_plans(&self, vin: &str) -> Result<StandbyPlanList, Error> {
        authed!(self, |cx| fetch_standby_plans(cx, vin))
    }

    /// Saves a plan's name, commands and schedule, then returns the fresh
    /// plans (pyBYD's `save_standby_plan`).
    ///
    /// Without a `schedule` the plan keeps its current one
    /// ([`StandbyPlan::schedule`], whose failure is returned before
    /// anything is sent); [`StandbySchedule::off`] unschedules it. Change
    /// `plan.commands` (for example the `OPENAIR` `controlParamsMap`) to
    /// change what the plan does. Saving with `default_plan` 1 makes it the
    /// default; the cloud clears the flag on the others.
    pub async fn save_standby_plan(
        &self,
        vin: &str,
        plan: &StandbyPlan,
        schedule: Option<&StandbySchedule>,
    ) -> Result<StandbyPlanList, Error> {
        let current;
        let schedule = match schedule {
            Some(schedule) => schedule,
            None => {
                current = plan.schedule().map_err(model_error)?;
                &current
            }
        };
        authed!(self, |cx| save_standby_plan(cx, vin, plan, schedule))?;
        self.get_standby_plans(vin).await
    }

    /// Creates a user plan and returns it (pyBYD's `create_standby_plan`).
    ///
    /// `commands` are what the plan does, for example copied from the
    /// preset plan. They must include the A/C (`OPENAIR`, else a
    /// `ValueError` before anything is sent); seats and steering wheel only
    /// go on top of it. `save` does not return the new id, so the plan is
    /// picked out of the next `list`: the plans are read before and after,
    /// and unless exactly one new plan appeared this fails with `BydError`
    /// (the plan may still have been created).
    pub async fn create_standby_plan(
        &self,
        vin: &str,
        name: &str,
        commands: &[StandbyPlanCommand],
        schedule: Option<&StandbySchedule>,
    ) -> Result<StandbyPlan, Error> {
        if !commands
            .iter()
            .any(|command| command.command_type == COMMAND_CLIMATE)
        {
            return Err(Error::new(
                ErrorKind::ValueError,
                "a plan needs an OPENAIR command",
            ));
        }
        let before: Vec<String> = self
            .get_standby_plans(vin)
            .await?
            .plans
            .into_iter()
            .map(|plan| plan.id)
            .collect();
        // `StandbyPlan(plan_name=name, plan_type=2, default_plan=0,
        // commands=[...])`: the name goes through the model, so one BYD
        // would read as "no value" (`""`, `"--"`, `"nan"`) is saved empty.
        let mut plan: StandbyPlan = parse_model(
            &json!({"plan_name": name, "plan_type": PLAN_TYPE_USER, "default_plan": 0}),
        )?;
        plan.commands = commands
            .iter()
            .map(|command| StandbyPlanCommand {
                command_id: None,
                ..command.clone()
            })
            .collect();
        let off = StandbySchedule::off();
        let schedule = schedule.unwrap_or(&off);
        authed!(self, |cx| save_standby_plan(cx, vin, &plan, schedule))?;
        let after = self.get_standby_plans(vin).await?;
        let mut created = after
            .plans
            .into_iter()
            .filter(|plan| !before.contains(&plan.id));
        match (created.next(), created.next()) {
            (Some(plan), None) => Ok(plan),
            _ => Err(Error::new(
                ErrorKind::BydError,
                format!("could not find the created plan {name:?} in the plan list"),
            )),
        }
    }

    /// Makes a plan the default, then returns the fresh plans (pyBYD's
    /// `set_default_standby_plan`).
    pub async fn set_default_standby_plan(
        &self,
        vin: &str,
        plan_id: &str,
    ) -> Result<StandbyPlanList, Error> {
        authed!(self, |cx| set_default_standby_plan(cx, vin, plan_id))?;
        self.get_standby_plans(vin).await
    }

    /// Deletes a user plan, then returns the fresh plans (pyBYD's
    /// `delete_standby_plan`).
    ///
    /// The preset plan cannot be deleted in the app; unschedule it with
    /// [`save_standby_plan`](Self::save_standby_plan) instead.
    pub async fn delete_standby_plan(
        &self,
        vin: &str,
        plan_id: &str,
    ) -> Result<StandbyPlanList, Error> {
        authed!(self, |cx| delete_standby_plan(cx, vin, plan_id))?;
        self.get_standby_plans(vin).await
    }

    /// Runs a plan now, keeping its schedule (pyBYD's `run_standby_plan`).
    ///
    /// `send-result` goes from `res` 1 to 2 after about six seconds; MQTT
    /// is not waited for. A result still at 1 when polling gives up is
    /// returned as it is, since the app itself stops polling there; any
    /// other `res` than 1 or 2 fails with `BydRemoteControlError` (its code
    /// is the `res`).
    pub async fn run_standby_plan(
        &self,
        vin: &str,
        plan_id: &str,
        settings: RunPollSettings,
    ) -> Result<StandbyPlanRunResult, Error> {
        let fetch = make_send_fetch_fn(plan_id);
        with_reauth!(
            self,
            async {
                let raw = self
                    .trigger_and_poll(TriggeredRead {
                        vin,
                        trigger_endpoint: SEND_ENDPOINT,
                        poll_endpoint: SEND_RESULT_ENDPOINT,
                        fetch: ReadFetch::StandbySend(&fetch),
                        label: "StandbyPlanRun",
                        mqtt_event_type: None,
                        mqtt_timeout: Some(0.0),
                        poll_attempts: settings.poll_attempts,
                        poll_interval: settings.poll_interval,
                        signal_retries: 2,
                    })
                    .await?;
                let mut result: StandbyPlanRunResult = parse_model(&raw)?;
                match result.res {
                    Some(res) if res != 1 && res != 2 => Err(Error::api(
                        ErrorKind::BydRemoteControlError,
                        format!(
                            "standby plan run rejected by cloud (res={res}, message={:?})",
                            result.message
                        ),
                        res.to_string(),
                        SEND_RESULT_ENDPOINT,
                    )),
                    _ => {
                        result.vin = vin.to_owned();
                        Ok(result)
                    }
                }
            }
            .await
        )
    }
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

/// The `schedule` argument: null, or `StandbySchedule`'s keyword arguments.
fn schedule_argument(
    args: &Args<'_>,
) -> Result<Option<Result<StandbySchedule, Error>>, ParityError> {
    match args.value("schedule") {
        Value::Null => Ok(None),
        Value::Object(kwargs) => Ok(Some(
            StandbySchedule::from_kwargs(kwargs).map_err(model_error),
        )),
        _ => Err(ParityError::bad_arguments(
            "schedule must be an object or null",
        )),
    }
}

/// Runs one of the plan methods for the parity call `client.<method>`;
/// `Ok(Err(..))` is the method's own failure. Models come as JSON and are
/// built first, as the pyBYD caller builds them: a plan or command as the
/// raw dict `model_validate` reads, a schedule as keyword arguments.
pub(super) async fn run_method<E: ClientEnv>(
    client: &BydClient<E>,
    method: &str,
    args: &Args<'_>,
) -> Result<Result<Value, Error>, ParityError> {
    let vin = args.str("vin")?;
    macro_rules! built {
        ($value:expr) => {
            match $value {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            }
        };
    }
    Ok(match method {
        "get_standby_plans" => client
            .get_standby_plans(vin)
            .await
            .map(|plans| plans.dump()),
        "save_standby_plan" => {
            let plan: StandbyPlan = built!(parse_model(args.value("plan")));
            let schedule = match schedule_argument(args)? {
                Some(schedule) => Some(built!(schedule)),
                None => None,
            };
            client
                .save_standby_plan(vin, &plan, schedule.as_ref())
                .await
                .map(|plans| plans.dump())
        }
        "create_standby_plan" => {
            let raw = args
                .value("commands")
                .as_array()
                .ok_or_else(|| ParityError::bad_arguments("commands must be a list"))?;
            let mut commands = Vec::with_capacity(raw.len());
            for command in raw {
                commands.push(built!(parse_model::<StandbyPlanCommand>(command)));
            }
            let schedule = match schedule_argument(args)? {
                Some(schedule) => Some(built!(schedule)),
                None => None,
            };
            client
                .create_standby_plan(vin, args.str("name")?, &commands, schedule.as_ref())
                .await
                .map(|plan| plan.dump())
        }
        "set_default_standby_plan" => client
            .set_default_standby_plan(vin, args.str("plan_id")?)
            .await
            .map(|plans| plans.dump()),
        "delete_standby_plan" => client
            .delete_standby_plan(vin, args.str("plan_id")?)
            .await
            .map(|plans| plans.dump()),
        _ => {
            let defaults = RunPollSettings::default();
            let settings = RunPollSettings {
                poll_attempts: match args.value("poll_attempts") {
                    Value::Null => defaults.poll_attempts,
                    _ => args.i64("poll_attempts")?,
                },
                poll_interval: match args.value("poll_interval") {
                    Value::Null => defaults.poll_interval,
                    value => value.as_f64().ok_or_else(|| {
                        ParityError::bad_arguments("poll_interval must be a number")
                    })?,
                },
            };
            client
                .run_standby_plan(vin, args.str("plan_id")?, settings)
                .await
                .map(|result| result.dump())
        }
    })
}
