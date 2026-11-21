//! openbyd as a C library, for languages that load one: the JVM package
//! (through JNA) and the .NET package (through P/Invoke). `include/openbyd.h`
//! declares it.
//!
//! It is [`openbyd_host`] and nothing more: functions and client methods by
//! name, with JSON text in and JSON text out. The typed layer each language
//! puts on top is generated (`bindings/generate.py`).
//!
//! # Conventions
//!
//! - Every string is UTF-8 and NUL-terminated. A null `args` means no arguments.
//! - A function that returns `char *` returns JSON the caller frees with
//!   [`openbyd_string_free`], and writes 1 to `*ok` when the JSON is the
//!   result or 0 when it is an error object (`class`, `bases`, `message`,
//!   `code`, `endpoint`, `status_code`, `error_type`, `loc`).
//! - Strings passed to a callback belong to the library and are valid only
//!   until the callback returns.
//! - Callbacks run on the library's own threads, never on the caller's.
//!   They must return quickly and must not call [`openbyd_client_free`].
//! - Everything may be called from any thread.

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, RwLock};

use openbyd_host::{Client, Host, HostError, runtime};
use serde_json::Value;

/// Receives a client's events: the event's name and its payload, a JSON object.
pub type EventFn =
    unsafe extern "C" fn(user_data: *mut c_void, name: *const c_char, payload: *const c_char);

/// Receives the outcome of [`openbyd_client_call`]: the number the call was
/// given, 1 or 0 for result or error, and the JSON.
pub type DoneFn =
    unsafe extern "C" fn(user_data: *mut c_void, call: u64, ok: i32, json: *const c_char);

/// The caller's callbacks. Closed by [`openbyd_client_free`], after which
/// neither is called again.
struct Callbacks {
    on_event: Option<EventFn>,
    on_done: Option<DoneFn>,
    user_data: *mut c_void,
    /// Read while a callback runs, written to close: closing waits for
    /// callbacks in progress.
    open: RwLock<bool>,
}

// The caller promises its callbacks and `user_data` may be used from any thread.
unsafe impl Send for Callbacks {}
unsafe impl Sync for Callbacks {}

impl Callbacks {
    fn while_open(&self, callback: impl FnOnce()) {
        let open = self
            .open
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *open {
            callback();
        }
    }

    fn close(&self) {
        *self
            .open
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
    }

    fn done(&self, call: u64, outcome: Result<Value, HostError>) {
        let Some(on_done) = self.on_done else { return };
        let (ok, json) = encode(outcome);
        // SAFETY: the caller's function, called as the header declares it.
        self.while_open(|| unsafe { on_done(self.user_data, call, ok, json.as_ptr()) });
    }
}

impl Host for Callbacks {
    fn event(&self, name: &str, payload: &str) {
        let Some(on_event) = self.on_event else {
            return;
        };
        let (Ok(name), Ok(payload)) = (CString::new(name), CString::new(payload)) else {
            return;
        };
        // SAFETY: the caller's function, called as the header declares it.
        self.while_open(|| unsafe { on_event(self.user_data, name.as_ptr(), payload.as_ptr()) });
    }
}

/// What [`openbyd_client_new`] returns.
pub struct ClientHandle {
    client: Arc<Client>,
    callbacks: Arc<Callbacks>,
}

/// JSON text as a C string. JSON text has no NUL byte: `serde_json` writes one as `\u0000`.
fn c_string(text: String) -> CString {
    CString::new(text).unwrap_or_else(|_| c"null".to_owned())
}

fn encode(outcome: Result<Value, HostError>) -> (i32, CString) {
    match outcome {
        Ok(value) => (1, c_string(value.to_string())),
        Err(error) => (0, c_string(error.to_json().to_string())),
    }
}

/// Reads a required string argument.
///
/// # Safety
/// `text` is null or a NUL-terminated string.
unsafe fn text<'a>(what: &str, text: *const c_char) -> Result<&'a str, HostError> {
    if text.is_null() {
        return Err(HostError::type_error(format!("{what} is null")));
    }
    // SAFETY: not null, and NUL-terminated by the caller's promise.
    unsafe { CStr::from_ptr(text) }
        .to_str()
        .map_err(|_| HostError::value(format!("{what} is not UTF-8")))
}

/// Reads JSON arguments; null means none.
///
/// # Safety
/// `json` is null or a NUL-terminated string.
unsafe fn json(what: &str, json: *const c_char) -> Result<Value, HostError> {
    if json.is_null() {
        return Ok(Value::Null);
    }
    // SAFETY: the caller's promise.
    let json = unsafe { text(what, json) }?;
    serde_json::from_str(json)
        .map_err(|error| HostError::value(format!("{what} is not JSON: {error}")))
}

/// Runs `body`, turning a panic into an error, and hands back its JSON.
///
/// # Safety
/// `ok` is null or points to an `int32_t`.
unsafe fn answer(ok: *mut i32, body: impl FnOnce() -> Result<Value, HostError>) -> *mut c_char {
    let outcome = catch_unwind(AssertUnwindSafe(body))
        .unwrap_or_else(|_| Err(HostError::internal("openbyd panicked")));
    let (flag, json) = encode(outcome);
    if !ok.is_null() {
        // SAFETY: not null, and an `int32_t` by the caller's promise.
        unsafe { *ok = flag };
    }
    json.into_raw()
}

/// Runs a function that needs no client (`openbyd_host::call`): `version`,
/// `encode_envelope`, `decode_envelope`, `parity_call`, `parse_model`,
/// `configure`, `runtime`, `has_testing`.
///
/// # Safety
/// `function` is a string, `args` a string or null, `ok` an `int32_t *` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openbyd_call(
    function: *const c_char,
    args: *const c_char,
    ok: *mut i32,
) -> *mut c_char {
    // SAFETY: the caller's promises.
    unsafe {
        answer(ok, || {
            openbyd_host::call(text("function", function)?, json("args", args)?)
        })
    }
}

/// Runs a call of the fake server or the test broker
/// (`openbyd_host::testing::call`). In a build without the feature `testing`
/// every call fails.
///
/// # Safety
/// `name` is a string, `args` a string or null, `ok` an `int32_t *` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openbyd_testing_call(
    name: *const c_char,
    args: *const c_char,
    ok: *mut i32,
) -> *mut c_char {
    // SAFETY: the caller's promises.
    unsafe {
        answer(ok, || {
            let (name, args) = (text("name", name)?, json("args", args)?);
            testing_call(name, args)
        })
    }
}

#[cfg(feature = "testing")]
fn testing_call(name: &str, args: Value) -> Result<Value, HostError> {
    openbyd_host::testing::call(name, args)
}

#[cfg(not(feature = "testing"))]
fn testing_call(_name: &str, _args: Value) -> Result<Value, HostError> {
    Err(HostError::value(
        "this library was built without the feature 'testing'",
    ))
}

/// Makes a client for `config` (an object of `BydConfig`'s fields) with
/// `options` (see `openbyd_host::Client::new`), closed until its `start`
/// method is called. Returns null on failure, with the error's JSON in
/// `*error` when `error` is not null.
///
/// `on_event` receives the client's events and `on_done` the outcome of
/// every [`openbyd_client_call`]; either may be null. Both are given
/// `user_data` unchanged.
///
/// # Safety
/// `config` is a string, `options` a string or null, `error` a `char **`
/// or null, and the callbacks stay callable until [`openbyd_client_free`] returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openbyd_client_new(
    config: *const c_char,
    options: *const c_char,
    on_event: Option<EventFn>,
    on_done: Option<DoneFn>,
    user_data: *mut c_void,
    error: *mut *mut c_char,
) -> *mut ClientHandle {
    let built = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller's promises.
        let (config, options) = unsafe { (json("config", config)?, json("options", options)?) };
        let callbacks = Arc::new(Callbacks {
            on_event,
            on_done,
            user_data,
            open: RwLock::new(true),
        });
        let host: Arc<dyn Host> = callbacks.clone();
        let client = Client::new(config, options, host)?;
        Ok(ClientHandle { client, callbacks })
    }))
    .unwrap_or_else(|_| Err(HostError::internal("openbyd panicked")));
    match built {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(failure) => {
            if !error.is_null() {
                let (_, json) = encode(Err(failure));
                // SAFETY: not null, and a `char **` by the caller's promise.
                unsafe { *error = json.into_raw() };
            }
            std::ptr::null_mut()
        }
    }
}

/// Runs a method that answers at once (`openbyd_host::SYNC_METHODS`), such
/// as `start`, `close`, `export_session` or `car.state`.
///
/// # Safety
/// `client` came from [`openbyd_client_new`] and is not freed, `method` is
/// a string, `args` a string or null, `ok` an `int32_t *` or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openbyd_client_call_sync(
    client: *const ClientHandle,
    method: *const c_char,
    args: *const c_char,
    ok: *mut i32,
) -> *mut c_char {
    // SAFETY: the caller's promises.
    unsafe {
        answer(ok, || {
            let handle = client
                .as_ref()
                .ok_or_else(|| HostError::type_error("client is null"))?;
            handle
                .client
                .call_sync(text("method", method)?, json("args", args)?)
        })
    }
}

/// Starts a method that waits on the network (`openbyd_host::ASYNC_METHODS`)
/// and returns at once. Its outcome goes to the client's `on_done` with
/// `call`, a number of the caller's choosing, on one of the library's threads.
///
/// # Safety
/// `client` came from [`openbyd_client_new`] and is not freed, `method` is
/// a string, `args` a string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openbyd_client_call(
    client: *const ClientHandle,
    call: u64,
    method: *const c_char,
    args: *const c_char,
) {
    // SAFETY: the caller's promises.
    let Some(handle) = (unsafe { client.as_ref() }) else {
        return;
    };
    let callbacks = Arc::clone(&handle.callbacks);
    // SAFETY: the caller's promises.
    let parsed = unsafe {
        text("method", method).and_then(|method| Ok((method.to_owned(), json("args", args)?)))
    };
    let (method, args) = match parsed {
        Ok(parsed) => parsed,
        Err(error) => {
            // From a library thread, as every other outcome.
            runtime::spawn(async move { callbacks.done(call, Err(error)) });
            return;
        }
    };
    let task = runtime::spawn(Arc::clone(&handle.client).call(method, args));
    runtime::spawn(async move {
        let outcome = task
            .await
            .unwrap_or_else(|_| Err(HostError::internal("openbyd panicked")));
        callbacks.done(call, outcome);
    });
}

/// Closes the client and frees it. No callback runs after this returns;
/// calls still in flight end without reporting. Null does nothing.
///
/// # Safety
/// `client` came from [`openbyd_client_new`] and is not used afterwards.
/// Not to be called from a callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openbyd_client_free(client: *mut ClientHandle) {
    if client.is_null() {
        return;
    }
    // SAFETY: from `Box::into_raw` in `openbyd_client_new`, and given up by the caller.
    let handle = unsafe { Box::from_raw(client) };
    let _ = catch_unwind(AssertUnwindSafe(|| {
        handle.callbacks.close();
        let _ = handle.client.call_sync("close", Value::Null);
        drop(handle);
    }));
}

/// Frees a string this library returned. Null does nothing.
///
/// # Safety
/// `text` came from this library and is not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openbyd_string_free(text: *mut c_char) {
    if !text.is_null() {
        // SAFETY: from `CString::into_raw`, and given up by the caller.
        drop(unsafe { CString::from_raw(text) });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use serde_json::json;

    use super::*;

    fn c(text: &str) -> CString {
        CString::new(text).unwrap()
    }

    /// Takes a returned string as JSON and frees it.
    fn take(json: *mut c_char) -> Value {
        assert!(!json.is_null());
        let value =
            serde_json::from_str(unsafe { CStr::from_ptr(json) }.to_str().unwrap()).unwrap();
        unsafe { openbyd_string_free(json) };
        value
    }

    fn function(name: &str, args: Option<&str>) -> (i32, Value) {
        let (name, args) = (c(name), args.map(c));
        let mut ok = -1;
        let args = args.as_ref().map_or(std::ptr::null(), |args| args.as_ptr());
        let json = unsafe { openbyd_call(name.as_ptr(), args, &mut ok) };
        (ok, take(json))
    }

    #[test]
    fn functions_answer_with_json_and_a_flag() {
        let (ok, version) = function("version", None);
        assert_eq!((ok, version), (1, json!(openbyd_host::VERSION)));
        let (ok, envelope) = function("encode_envelope", Some(r#"{"text": "hi"}"#));
        assert_eq!(ok, 1);
        let (ok, plain) = function(
            "decode_envelope",
            Some(&json!({ "envelope": envelope }).to_string()),
        );
        assert_eq!((ok, plain), (1, json!({"hex": "6869"})));

        let (ok, error) = function("nope", None);
        assert_eq!((ok, error["class"].as_str()), (0, Some("ValueError")));
        let (ok, error) = function("version", Some("{not json"));
        assert_eq!((ok, error["class"].as_str()), (0, Some("ValueError")));
        // A null flag is allowed, and so is a null function name (an error).
        let json =
            unsafe { openbyd_call(std::ptr::null(), std::ptr::null(), std::ptr::null_mut()) };
        assert_eq!(take(json)["class"], "TypeError");
        unsafe { openbyd_string_free(std::ptr::null_mut()) };
    }

    #[test]
    fn invalid_utf8_is_refused() {
        let bad = [0xff_u8, 0];
        let mut ok = -1;
        let json = unsafe { openbyd_call(bad.as_ptr().cast(), std::ptr::null(), &mut ok) };
        assert_eq!(ok, 0);
        assert_eq!(take(json)["class"], "ValueError");
    }

    type Outcome = (u64, i32, Value);

    struct Seen {
        done: Mutex<mpsc::Sender<Outcome>>,
        events: Mutex<Vec<(String, Value)>>,
    }

    unsafe extern "C" fn on_done(user_data: *mut c_void, call: u64, ok: i32, json: *const c_char) {
        let seen = unsafe { &*user_data.cast::<Seen>() };
        let value =
            serde_json::from_str(unsafe { CStr::from_ptr(json) }.to_str().unwrap()).unwrap();
        seen.done.lock().unwrap().send((call, ok, value)).unwrap();
    }

    unsafe extern "C" fn on_event(
        user_data: *mut c_void,
        name: *const c_char,
        payload: *const c_char,
    ) {
        let seen = unsafe { &*user_data.cast::<Seen>() };
        let name = unsafe { CStr::from_ptr(name) }.to_str().unwrap().to_owned();
        let payload =
            serde_json::from_str(unsafe { CStr::from_ptr(payload) }.to_str().unwrap()).unwrap();
        seen.events.lock().unwrap().push((name, payload));
    }

    fn new_client(
        config: &str,
        options: Option<&str>,
        seen: &Seen,
    ) -> Result<*mut ClientHandle, Value> {
        let (config, options) = (c(config), options.map(c));
        let mut error = std::ptr::null_mut();
        let client = unsafe {
            openbyd_client_new(
                config.as_ptr(),
                options
                    .as_ref()
                    .map_or(std::ptr::null(), |options| options.as_ptr()),
                Some(on_event),
                Some(on_done),
                std::ptr::from_ref(seen).cast_mut().cast(),
                &mut error,
            )
        };
        if client.is_null() {
            Err(take(error))
        } else {
            Ok(client)
        }
    }

    fn sync(client: *const ClientHandle, method: &str, args: Option<&str>) -> (i32, Value) {
        let (method, args) = (c(method), args.map(c));
        let mut ok = -1;
        let args = args.as_ref().map_or(std::ptr::null(), |args| args.as_ptr());
        let json = unsafe { openbyd_client_call_sync(client, method.as_ptr(), args, &mut ok) };
        (ok, take(json))
    }

    #[test]
    fn a_client_is_made_called_and_freed() {
        let (sender, receiver) = mpsc::channel();
        let seen = Seen {
            done: Mutex::new(sender),
            events: Mutex::new(Vec::new()),
        };
        let bad = new_client(r#"{"password": "x"}"#, None, &seen).unwrap_err();
        assert_eq!(bad["class"], "ValidationError");
        // Without somewhere to put the error, failure is still a null client.
        let config = c("[]");
        let none = unsafe {
            openbyd_client_new(
                config.as_ptr(),
                std::ptr::null(),
                None,
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert!(none.is_null());

        // Port 9 (discard) refuses connections: the call fails over the network path.
        let config = r#"{"username": "user@example.com", "password": "secret", "base_url": "http://127.0.0.1:9"}"#;
        let client = new_client(config, Some(r#"{"events": ["request"]}"#), &seen).unwrap();
        assert_eq!(sync(client, "commands_enabled", None), (1, json!(false)));
        assert_eq!(sync(client, "start", None), (1, Value::Null));
        let (ok, error) = sync(client, "nope", None);
        assert_eq!((ok, error["class"].as_str()), (0, Some("AttributeError")));
        let (ok, error) = sync(std::ptr::null(), "start", None);
        assert_eq!((ok, error["class"].as_str()), (0, Some("TypeError")));

        let (method, args) = (c("login"), c("{}"));
        unsafe { openbyd_client_call(client, 7, method.as_ptr(), args.as_ptr()) };
        let (call, ok, error) = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        assert_eq!(
            (call, ok, error["class"].as_str()),
            (7, 0, Some("BydTransportError"))
        );
        let events = seen.events.lock().unwrap().clone();
        assert_eq!(events[0].0, "request");
        assert_eq!(events[0].1["record"]["outcome"], "transport");

        // Arguments that cannot be read are reported the same way.
        let not_json = c("{");
        unsafe { openbyd_client_call(client, 8, method.as_ptr(), not_json.as_ptr()) };
        let (call, ok, error) = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap();
        assert_eq!(
            (call, ok, error["class"].as_str()),
            (8, 0, Some("ValueError"))
        );
        unsafe { openbyd_client_call(std::ptr::null(), 9, method.as_ptr(), args.as_ptr()) };

        // After free, a call in flight ends without reporting.
        unsafe { openbyd_client_call(client, 10, method.as_ptr(), args.as_ptr()) };
        unsafe { openbyd_client_free(client) };
        unsafe { openbyd_client_free(std::ptr::null_mut()) };
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(receiver.try_recv().is_err(), "no callback after free");
    }

    #[test]
    fn the_testing_call_matches_the_build() {
        let name = c("server.requests");
        let mut ok = -1;
        let json = unsafe { openbyd_testing_call(name.as_ptr(), std::ptr::null(), &mut ok) };
        let error = take(json);
        assert_eq!(ok, 0);
        if cfg!(feature = "testing") {
            assert_eq!(error["class"], "TypeError", "{error}");
        } else {
            assert!(error["message"].as_str().unwrap().contains("testing"));
        }
    }
}
