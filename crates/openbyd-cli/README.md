## The `openbyd` command-line tool

`openbyd` lets you check a BYD account, see what the BYD overseas app API returns for your car, or debug a problem, without writing code. It is an unofficial tool and is not affiliated with BYD. It is the Rust port of the script `scripts/dump_all.py` from pyBYD, plus login, watch and remote-command support.

### Install

```sh
cargo install openbyd-cli      # installs the `openbyd` binary
openbyd version                # the tool's version and the pyBYD commit it matches
```

It needs no OpenSSL, cmake or NASM: TLS is rustls with the `ring` provider.

### Settings

`openbyd` reads the same `BYD_*` environment variables as pyBYD: `BYD_USERNAME`, `BYD_PASSWORD`, `BYD_BASE_URL`, `BYD_COUNTRY_CODE`, `BYD_LANGUAGE`, `BYD_TIME_ZONE`, `BYD_CONTROL_PIN` and the rest. Flags override them: `--username`, `--base-url`, `--country-code`, `--language`, `--time-zone`.

```sh
export BYD_USERNAME="you@example.com"
export BYD_BASE_URL="https://dilinkappoversea-eu.byd.auto"   # the default; use your region's
```

### Commands

Every command prints JSON on stdout (pretty on a terminal, compact when piped). Add `--format table` for a table where that makes sense. Logs and diagnostics go to stderr; `-v` and `-vv` add detail.

| Command | What it does |
|---|---|
| `openbyd login [--save-session FILE]` | Logs in; prints the user id and vehicle count. |
| `openbyd vehicles` | Lists vehicles: VIN, model name, alias, energy type. |
| `openbyd dump [--vin VIN] [--skip SECTION,...] [--raw]` | Calls every read-only endpoint for each vehicle and prints one JSON document. |
| `openbyd get SECTION --vin VIN [--raw]` | One section only. |
| `openbyd watch [--vin VIN] [--seconds N]` | Prints each MQTT event as a line of JSON until Ctrl-C or the time is up. |
| `openbyd command NAME --vin VIN --yes-send-to-car [...]` | Sends a remote command to the car. |
| `openbyd version` | The tool's version and the pyBYD commit. |

The sections are `realtime`, `gps`, `charging`, `hvac`, `energy`, `charging-schedule`, `latest-config`, `capabilities`, `push-state`, `bookings`, `notifications` and `ac-current-limit`.

```sh
# Log in and check the account
openbyd login
# {"user_id":"...","vehicle_count":1,"session_saved":false}

# What cars does it have?
openbyd vehicles --format table

# Everything the API returns for one car, parsed model and raw payload side by side
openbyd dump --vin LNBXXXXXXXXXXXXXX --raw > dump.json

# A section that fails does not stop the dump; it shows up as
#   "hvac": {"error": {"class": "BydApiError", "code": "1008", "message": "..."}}

# One section
openbyd get gps --vin LNBXXXXXXXXXXXXXX

# Live events
openbyd watch --vin LNBXXXXXXXXXXXXXX --seconds 60

# Climate for 15 minutes at 22 C (this moves a real car; see below)
openbyd command start-climate --vin LNBXXXXXXXXXXXXXX --temperature 22 --minutes 15 --yes-send-to-car
```

Without `--raw` a section is the parsed model. With `--raw` it is `{"parsed": ..., "raw": ...}`, so you can spot fields the model does not parse yet.

Remote commands: `lock`, `unlock`, `flash-lights`, `find-car`, `start-climate` (`--temperature` 15 to 31, default 21; `--minutes` 10, 15, 20, 25 or 30, default 20), `stop-climate`, `close-windows`, `open-trunk`, `close-trunk`, `start-charging`, `stop-charging`. `stop-charging` always fails without sending anything: the BYD cloud accepts it but the car keeps charging.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. |
| 1 | An API or transport error. A JSON error object `{"error": {"class", "code", "message"}}` is printed on stdout. |
| 2 | A usage error: bad or missing arguments, or no password or PIN available. |
| 3 | Authentication failed (wrong password, or the saved session has expired). |
| 130 | Interrupted with Ctrl-C. `watch` stops cleanly and exits 0. |

`dump` exits 0 when it produced a document, even if some sections reported errors. It stops with 3 if the credentials stop working and with 1 if the server cannot be reached at all.

### Safety

**Password.** It comes from `BYD_PASSWORD` or from a prompt that does not echo. There is deliberately no `--password` flag: a flag would be visible in the process list and in your shell history. When there is nobody to prompt (a script, a pipe), `openbyd` exits with code 2 rather than hang.

**Sessions.** A new login can end an existing session of the same account on some BYD clusters, including the one in the BYD phone app. To log in as seldom as possible, save the session once and reuse it:

```sh
openbyd login --save-session ~/.openbyd-session.json
openbyd dump --session ~/.openbyd-session.json
```

With `--session` the password is not needed and never sent. When the session has expired the command fails with code 3 and you log in again. The session file holds the session tokens, which let anyone act as you until they expire: it is created readable by its owner only where the platform allows (mode 0600 on Unix; on Windows it keeps the access rules of its folder, so keep it in your profile). Its contents are never printed.

**Secrets in output.** Tokens, passwords, PINs and the session file contents are never printed, not even with `-vv`. VINs and coordinates are real data you asked for and are printed. To paste a dump into a bug report, add `--redact`: it masks VINs (as `<vin-1>`, `<vin-2>`, ...), coordinates, e-mail addresses and plates.

**Car commands move a real car.** `openbyd command` refuses unless you pass `--yes-send-to-car`, and it prints what it is about to send before sending. It needs the control PIN: `--pin`, `BYD_CONTROL_PIN`, or a prompt that does not echo (prefer the last two; a flag is visible in the process list). It makes a single attempt: BYD can act on a command even when it reports a failure, so an automatic second send could lock, unlock or open something twice. Use `--retries N` to allow up to N sends in all.
