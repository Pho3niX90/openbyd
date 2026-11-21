//! Node.js bindings for openbyd: the add-on the npm package `openbyd` loads.
//!
//! It is [`openbyd_host`] and nothing more: functions and client methods by
//! name, with JSON text in and JSON text out. The typed TypeScript layer on
//! top (`bindings/node/src`) is generated from `bindings/spec.py`.
//!
//! - A failure is thrown as an `Error` whose message is [`ERROR_PREFIX`]
//!   followed by the error's JSON (`class`, `bases`, `message`, `code`, ...);
//!   the TypeScript layer turns it into the matching error class.
//! - Calls that wait on the network run on openbyd's own runtime and settle
//!   a promise; the add-on starts no runtime of its own.
//! - Events reach JavaScript on its own thread, through a thread-safe
//!   function that does not keep the process alive.

use std::sync::Arc;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use openbyd_host::{Client, Host, HostError, runtime};
use serde_json::Value;

/// What the message of an error thrown by this add-on starts with; the
/// error's JSON follows.
#[napi]
pub const ERROR_PREFIX: &str = "openbyd:";

fn thrown(error: &HostError) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("{ERROR_PREFIX}{}", error.to_json()),
    )
}

fn json(what: &str, text: Option<String>) -> std::result::Result<Value, HostError> {
    match text {
        None => Ok(Value::Null),
        Some(text) => serde_json::from_str(&text)
            .map_err(|error| HostError::value(format!("{what} is not JSON: {error}"))),
    }
}

fn answer(outcome: std::result::Result<Value, HostError>) -> Result<String> {
    match outcome {
        Ok(value) => Ok(value.to_string()),
        Err(error) => Err(thrown(&error)),
    }
}

/// Runs a function that needs no client (`openbyd_host::call`) and returns
/// its result as JSON text.
#[napi]
pub fn call(function: String, args: Option<String>) -> Result<String> {
    answer(json("args", args).and_then(|args| openbyd_host::call(&function, args)))
}

/// Runs a call of the fake server or the test broker
/// (`openbyd_host::testing::call`). Fails in an add-on built without the
/// feature `testing`.
#[napi]
pub fn testing_call(name: String, args: Option<String>) -> Result<String> {
    answer(json("args", args).and_then(|args| testing(&name, args)))
}

#[cfg(feature = "testing")]
fn testing(name: &str, args: Value) -> std::result::Result<Value, HostError> {
    openbyd_host::testing::call(name, args)
}

#[cfg(not(feature = "testing"))]
fn testing(_name: &str, _args: Value) -> std::result::Result<Value, HostError> {
    Err(HostError::value(
        "this add-on was built without the feature 'testing'",
    ))
}

/// Calls back into JavaScript with an event's name and payload. Weak: an
/// idle client does not keep the process alive.
type EventFunction =
    ThreadsafeFunction<FnArgs<(String, String)>, (), FnArgs<(String, String)>, Status, false, true>;

struct JsHost(EventFunction);

impl Host for JsHost {
    fn event(&self, name: &str, payload: &str) {
        // Queued for the JavaScript thread; never waits for it.
        self.0.call(
            (name.to_owned(), payload.to_owned()).into(),
            ThreadsafeFunctionCallMode::NonBlocking,
        );
    }
}

/// The client by method name. Wrapped by the package's `Client`.
#[napi]
pub struct NativeClient {
    client: Arc<Client>,
}

#[napi]
impl NativeClient {
    /// A closed client for `config` with `options` (both JSON text; see
    /// `openbyd_host::Client::new`), whose events go to `on_event(name, payload)`.
    #[napi(constructor)]
    pub fn new(
        config: String,
        options: Option<String>,
        on_event: Function<FnArgs<(String, String)>, ()>,
    ) -> Result<Self> {
        let on_event = on_event
            .build_threadsafe_function()
            .weak::<true>()
            .callee_handled::<false>()
            .build()?;
        let host: Arc<dyn Host> = Arc::new(JsHost(on_event));
        let built = json("config", Some(config))
            .and_then(|config| Ok((config, json("options", options)?)))
            .and_then(|(config, options)| Client::new(config, options, host));
        match built {
            Ok(client) => Ok(Self { client }),
            Err(error) => Err(thrown(&error)),
        }
    }

    /// Runs a method that answers at once (`openbyd_host::SYNC_METHODS`).
    #[napi]
    pub fn call_sync(&self, method: String, args: Option<String>) -> Result<String> {
        answer(json("args", args).and_then(|args| self.client.call_sync(&method, args)))
    }

    /// Runs a method that waits on the network (`openbyd_host::ASYNC_METHODS`).
    /// The promise gives the result as JSON text.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn call<'env>(
        &self,
        env: &'env Env,
        method: String,
        args: Option<String>,
    ) -> Result<Object<'env>> {
        let (deferred, promise) =
            env.create_deferred::<String, Box<dyn FnOnce(Env) -> Result<String> + Send>>()?;
        let client = Arc::clone(&self.client);
        runtime::spawn(async move {
            let outcome = match json("args", args) {
                Ok(args) => runtime::spawn(client.call(method, args))
                    .await
                    .unwrap_or_else(|_| Err(HostError::internal("openbyd panicked"))),
                Err(error) => Err(error),
            };
            match outcome {
                Ok(value) => deferred.resolve(Box::new(move |_| Ok(value.to_string()))),
                Err(error) => deferred.reject(thrown(&error)),
            }
        });
        Ok(promise)
    }
}
