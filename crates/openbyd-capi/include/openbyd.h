/*
 * openbyd as a C library.
 *
 * Functions and client methods are named by strings and take and return
 * JSON text, so this header stays small. The JVM and .NET packages put a
 * typed layer on top; from C or C++ use it directly.
 *
 * Conventions:
 *  - Every string is UTF-8 and NUL-terminated. A null `args` means no arguments.
 *  - A function that returns `char *` returns JSON you free with
 *    openbyd_string_free(), and writes 1 to `*ok` when the JSON is the
 *    result, or 0 when it is an error object with the fields `class` (the
 *    Python exception class pyBYD raises), `bases`, `message`, `code`,
 *    `endpoint`, `status_code`, `error_type` and `loc`.
 *  - Strings passed to a callback are valid only until it returns.
 *  - Callbacks run on the library's threads. They must return quickly and
 *    must not call openbyd_client_free().
 *  - Everything may be called from any thread.
 */
#ifndef OPENBYD_H
#define OPENBYD_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* A client. */
typedef struct openbyd_client openbyd_client;

/* Receives a client's events: the event's name and its payload, a JSON object. */
typedef void (*openbyd_event_fn)(void *user_data, const char *name, const char *payload);

/* Receives the outcome of openbyd_client_call(): the number the call was
 * given, 1 or 0 for result or error, and the JSON. */
typedef void (*openbyd_done_fn)(void *user_data, uint64_t call, int32_t ok, const char *json);

/* Runs a function that needs no client: "version", "encode_envelope",
 * "decode_envelope", "parity_call", "parse_model", "configure", "runtime",
 * "has_testing". */
char *openbyd_call(const char *function, const char *args, int32_t *ok);

/* Runs a call of the fake server or the MQTT test broker. Fails in a
 * library built without the feature "testing". */
char *openbyd_testing_call(const char *name, const char *args, int32_t *ok);

/* Makes a client for `config` (an object of BydConfig's fields) with
 * `options` (or null), closed until its "start" method is called. Returns
 * null on failure, with the error's JSON in `*error` when `error` is not
 * null. `on_event` and `on_done` may be null; both receive `user_data`. */
openbyd_client *openbyd_client_new(const char *config, const char *options,
                                   openbyd_event_fn on_event, openbyd_done_fn on_done,
                                   void *user_data, char **error);

/* Runs a method that answers at once, such as "start", "close",
 * "export_session" or "car.state". */
char *openbyd_client_call_sync(const openbyd_client *client, const char *method,
                               const char *args, int32_t *ok);

/* Starts a method that waits on the network, such as "login" or
 * "get_vehicle_realtime", and returns at once. Its outcome goes to the
 * client's `on_done` with `call`, a number of your choosing. */
void openbyd_client_call(const openbyd_client *client, uint64_t call, const char *method,
                         const char *args);

/* Closes the client and frees it. No callback runs after this returns;
 * calls still in flight end without reporting. Null does nothing. */
void openbyd_client_free(openbyd_client *client);

/* Frees a string this library returned. Null does nothing. */
void openbyd_string_free(char *text);

#ifdef __cplusplus
}
#endif

#endif /* OPENBYD_H */
