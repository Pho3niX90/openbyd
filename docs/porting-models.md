# Porting a pyBYD model

Every pyBYD reply model is a pydantic v2 class. The contract: for any input,
the Rust model serialises to exactly what
`Model.model_validate(raw, context=...).model_dump(mode="json")` gives, and
fails where pydantic raises `ValidationError`. The worked examples are
`crates/openbyd-protocol/src/models/gps.rs` (a before-validator) and
`models/charging.rs` (key aliases, sentinel rules, properties). Read those
two files first; most models are the same shape with more fields.

## Steps

1. **Read the Python class** at the pinned commit (`../pybyd-ref/src/pybyd/models/<file>.py`):
   fields in order, `Field(validation_alias=...)`, `_KEY_ALIASES`,
   `_SENTINEL_RULES`, `@model_validator`s, `@property`s, enums.
2. **Write the struct.** Same field names in the same order, `raw` first
   (it is inherited from `BydBaseModel`). Carry each docstring over as a doc
   comment. Derive `Serialize`; field order is dump order.
3. **Write `BydModel::parse`** in pydantic's order:
   ```rust
   fn parse(raw: &Value, context: &Value) -> Result<Self, ValidationError> {
       let values = own_before_validator(raw);                    // only if the class has one
       let mut fields = Fields::cleaned(&values, KEY_ALIASES)?;    // BydBaseModel's before-validator; non-dict input fails here
       let mut model = Self {
           raw: fields.raw(),
           soc: fields.opt("soc", coerce::int),
           vin: fields.get("vin", String::new(), coerce::string),
           gps_timestamp: fields.get(aliased("gps_timestamp", &["gpsTimeStamp"]), None, base::byd_timestamp),
           // ...every field, in declaration order
       };
       fields.finish()?;
       base::clear_if(&mut model.full_hour, base::is_negative); // each _SENTINEL_RULES entry
       // then the class's own after-validators
       Ok(model)
   }
   ```
   `Fields::cleaned(values, aliases)` reads the same as
   `Fields::new(&base::clean_byd_values(values, aliases))` but borrows the
   input instead of copying it (twice). Without a before-validator pass
   `raw` itself. A before-validator should return the input unchanged when it
   has nothing to do (a `Cow`, or `raw` itself) rather than a clone; one that
   only adds or replaces keys and stashes its input as `raw`, like
   `VehicleRealtimeData`'s, returns just those keys and uses
   `Fields::cleaned_with_changes(raw, &changes, KEY_ALIASES)`.
   Then give the struct its `raw` field back for the owned parse:
   ```rust
   fn raw_mut(&mut self) -> Option<&mut serde_json::Map<String, Value>> {
       Some(&mut self.raw)
   }
   ```
   `BydModel::parse_owned` (used by the bindings and the client) calls
   `parse`; when `raw` is the whole reply, `fields.raw()` returns an empty
   map and `parse_owned` moves the reply into `raw_mut()` once `parse`
   returns, instead of copying it. So **`parse` must not read `model.raw`**
   (an after-validator that looks at `raw`, like `Vehicle`'s
   `_fill_missing_pics`): such a model overrides `parse_owned` to call
   `Self::parse(&raw, context)`. The parity call goes through `parse_owned`
   and `into_dump`, so the recorded vectors catch a model that breaks this.
4. **Properties:** one `pub fn` per `@property` with the same name, and list
   them all in `BydModel::properties` with `base::properties([...])`.
5. **Parity:** add the arm `"models.<file>.<Class>" => base::model_call::<Class>(args)`
   to the module's `parity_call`, then create `parity/groups/models_<file>.py`:
   ```python
   group = Group("models_<file>")
   register_model(group, "models.<file>.<Class>", Class, samples=[{...}, {...}])
   ```
   Samples are realistic payloads with synthetic data (`TESTVIN0000000001`,
   `52.0, 4.0`); the upstream tests in `../pybyd-ref/tests/` are a good source.
   Everything else (oracle, a few hundred recorded cases, endless fuzz) is
   generated from the class. Pass `contexts=[...]` if a validator reads `info.context`.
6. Run `python parity/generate.py models_<file>`, `cargo test -p openbyd-protocol`,
   `python parity/dev_install.py` and `OPENBYD_FUZZ=20000 pytest python/tests/test_parity.py`.
7. **Speed:** if the class has corpus payloads (`parity/corpus/<Class>.json`),
   add it to `largest_of!` in `crates/openbyd-protocol/benches/protocol.rs`
   and check it against pyBYD with `benches/compare_pybyd.py <Class>`. Parsing
   runs for every reply, so avoid copying the input or allocating per field
   in a validator (see `crates/openbyd-protocol/benches/README.md`).

## Shared helpers

Rust (`models/base.rs`; `models::vehicle` and `models::realtime` re-export
their old names):

- **Enums.** `byd_enum!` declares a `BydEnum` (an `IntEnum` with `UNKNOWN = -1`
  and a Python name per member); write `impl PyEnum` and `impl IntEnum` by hand
  for one that differs (`StearingWheelHeat`, `SeatHeatVentState`). Read a field
  with `fields.opt("x", enum_field)` (or `fields.get("x", E::Unknown, enum_field)`).
  Answer the enum's parity call with `"models.<file>.<Enum>" => enum_call::<Enum>(args)`.
  Python's `EnumClass(value)` is `PyEnum::call`; `PyEnum::methods` adds a
  method's result (`to_command_level`) to the answer.
- **Python semantics.** `py_truthy(&Value)` is `bool(value)`, `py_equals_int(&Value, n)`
  is `value == n` (so `1.0` and `true` equal `1`).
- **`byd_enum_member`** is `BydEnum(value)` for any member list (the `models.base.BydEnum` call).
- **Seat state.** `SeatClimateState` converts from `&HvacStatus` and `&VehicleRealtimeData`;
  `SeatClimateParams::from_current_models(hvac, realtime)` takes the parsed models.

Python (`parity/models_support.py`):

- **`register_enum(group, call, cls, methods=())`** registers the oracle, recorded
  values and fuzz of one `BydEnum` call (add the names of methods to call, such as
  `("to_command_level",)`). `enum_values(cls)` is its list of awkward inputs.
- **`mutate_fields(rng, cases, pools=None, nested=None)`** is the usual fuzz step:
  a realistic sample with one to three fields changed.
- **`skip_unrepresentable_integers(group, bound=2**63)`** is the integer-limit option.
  pyBYD's integers are unbounded and openbyd's are `i64`, so a conversion such as
  `int(1e300)` gives an answer openbyd can only call `Unsupported`. Call it once at
  the end of a group module and every case whose pyBYD answer holds an integer
  outside `(-bound, bound)` is left out of the recorded and fuzz sources (errors and
  `raw` are not looked at). It filters on the answer; if a case must be dropped
  because of its input even when pyBYD raises, filter in the group, as
  `models_control` does for its event models.

## Field types

| Python annotation | Rust field | Reader call | `#[serde(...)]` |
|---|---|---|---|
| `int \| None = None` | `Option<i64>` | `fields.opt("x", coerce::int)` | |
| `int = 0` | `i64` | `fields.get("x", 0, coerce::int)` | |
| `float \| None` | `Option<f64>` | `fields.opt("x", coerce::float)` | `serialize_with = "base::ser_opt_float"` |
| `float` | `f64` | `fields.get("x", 0.0, coerce::float)` | `serialize_with = "base::ser_float"` |
| `str \| None` / `str = ""` | `Option<String>` / `String` | `opt(.., coerce::string)` / `get(.., String::new(), coerce::string)` | |
| `bool \| None` | `Option<bool>` | `fields.opt("x", coerce::boolean)` | |
| `BydTimestamp` | `Option<PyDateTime>` | `fields.get("x", None, base::byd_timestamp)` | `serialize_with = "base::ser_opt_datetime"` |
| `SomeBydEnum \| None` | `Option<SomeBydEnum>` | `fields.opt("x", coerce::int_enum)` | `serialize_with = "base::ser_opt_int_enum"` |
| `list[str]` | `Vec<String>` | `fields.get("x", Vec::new(), \|v\| coerce::list(v, coerce::string))` | |
| `list[float]` | `Vec<f64>` | as above with `coerce::float` | `serialize_with = "base::ser_float_list"` |
| `dict[str, Any]` | `Map<String, Value>` | `fields.get("x", Map::new(), coerce::any_dict)` | |
| `dict[str, int]` | `IndexMap<String, i64>` | `fields.get("x", IndexMap::new(), \|v\| coerce::dict(v, coerce::int))` | |
| `int \| str` | `IntOrStr` | `coerce::int_or_str` | |
| `int = Field(ge=0, le=3)` | `i64` | `\|v\| coerce::int_bounded(v, Some(0), Some(3))` | |
| nested `Model \| None` | `Option<Model>` | `fields.opt("x", \|v\| Model::parse(v, context))` | |
| a field with no default | `T` | `fields.required("x", conv)` | |

Keys: a plain name looks up `to_camel(name)` and then `name`.
`Field(validation_alias="k")` is `aliased("name", &["k"])`; `AliasChoices("a", "b")`
is `aliased("name", &["a", "b"])`. A plain `BaseModel` (not `BydBaseModel`) uses
`Fields::plain`, where the only key is the field name.

An enum is a Rust enum with explicit discriminants plus `impl coerce::IntEnum`
(see the doc example on the trait). `MISSING` is `Some(UNKNOWN)` for a `BydEnum`
and `None` for a plain `IntEnum`. A Python enum alias (two names, one value) is
one Rust variant. `StrEnum` has `coerce::StrEnum` / `coerce::str_enum`.
A custom `BeforeValidator` (`_BydFloat`, `_HhmmTime`, ...) is a Rust function
`fn(&Value) -> Result<T, ValidationError>` next to the model; port the Python
function line by line and give it its own parity call if it is module-level.

## What pydantic does with each input (measured, pydantic 2.14 / core 2.50)

| input | `int` | `float` | `str` | `bool` | `BydEnum` | `BydTimestamp` |
|---|---|---|---|---|---|---|
| `1.0` / `-0.0` | 1 / 0 | 1.0 / -0.0 | error | true / false | member 1 / 0 | 1 s / None |
| `1.5` | error | 1.5 | error | error | UNKNOWN | 1 s (truncated) |
| float ≥ 2^63 or ≤ -2^63 | error | as is | error | error | UNKNOWN | see limits |
| `true` | 1 | 1.0 | error | true | member 1 | 1 s |
| int outside i64 | big int (openbyd: `Unsupported`) | float | error | error | **error** | see limits |
| `" 12 "` (any Unicode White_Space) | 12 | 12.0 | as is | error | member 12 | 12 s |
| `"+5"`, `"00012"`, `"-0"` | 5, 12, 0 | 5.0, 12.0, -0.0 | as is | error | lookup | `"+5"` → None |
| `"1_000"` | 1000 | 1000.0 | as is | error | lookup | None |
| `"0__1"`, `"0_-5"`, `"0-05"` | 1, -5, error | error | as is | error | lookup / UNKNOWN | None |
| `"1__0"`, `"_1"`, `"1_"` | error | error | as is | error | UNKNOWN | None |
| `"1.0"`, `"1.00"` / `"1."`, `"1.5"` | 1 / error | 1.0 / 1.0, 1.5 | as is | error | member 1 / UNKNOWN | None |
| `"1e3"` | error | 1000.0 | as is | error | UNKNOWN | None |
| `"nan"`, `"-Infinity"`, `"NAN"` | error | NaN, -inf | as is | error | UNKNOWN | None |
| `"true"`, `"Off"`, `"y"`, `"1"` | error (`"1"` → 1) | error | as is | yes (exact words, case-insensitive, no trimming) | UNKNOWN | None |
| `"١٢"` (non-ASCII digits) | error | error | as is | error | UNKNOWN | **12 s** (`int()` reads them) |
| `null` | error | error | error | error | UNKNOWN | None |
| `[]`, `{}` | error | error | error | error | UNKNOWN | None |

Containers: `list[T]` takes only a JSON array (not a string); `dict[str, T]`
only an object. A nested model field takes only an object.

Model behaviour (`BydBaseModel`):

- **Cleaning first.** `_clean_dict` drops `null` and every string whose
  `str.strip()` is `""`, `"--"`, `"NaN"` or `"nan"` *before* any field sees it,
  so a sentinel becomes the field's default, never an error. `"NAN"` and
  `" -- x"` are not sentinels.
- **Key aliases** (`_KEY_ALIASES`) rename old → new only when the new key is absent, in order.
- **Both alias and field name present:** the alias wins, whatever the order.
  A key that is neither (for example `gpsTimestamp` when the alias is
  `gpsTimeStamp`) is ignored (`extra="ignore"`).
- **`raw`** is the input dict *after* the model's own before-validators (GPS's
  merged `data`), unless the input has its own `raw` key: then that value is
  used (a sentinel `raw` falls back to `{}`, a non-dict `raw` is an error).
- **Non-dict input** (null, list, string, number) is a `ValidationError`.
- **Validator order:** subclass before-validators run before the base's (the
  last defined runs first); after-validators run base first, then in
  definition order.
- **All fields are validated** before pydantic raises, so one error does not
  hide another; but an exception pydantic does not catch (an `OverflowError`
  from `parse_byd_timestamp`) stops validation and escapes as itself.
  `Fields` and `coerce::Failures` reproduce this.
- **Datetimes in JSON:** `YYYY-MM-DDTHH:MM:SS[.ffffff]` plus `Z` for UTC, or the
  offset rounded to whole seconds and printed `±HH:MM` (seconds dropped, so
  `-00:00:30` prints `-00:00`, and `+23:59:59.6` prints `+24:00`).

## Representation limits (both sides follow them)

- **Non-finite floats** are written `"NaN"`, `"Infinity"`, `"-Infinity"` in
  every parity answer. JSON inputs are finite; NaN reaches a model only through
  strings like `"NAN"`.
- **Integers are `i64`.** pydantic accepts arbitrarily large integers; openbyd
  answers those with the error kind `Unsupported`. Generated payloads keep JSON
  integers within `[-2^63, 2^64-1]` (what `serde_json::Value` holds) and values
  that become a model `int` within `±2^53`, so property arithmetic stays in range
  (`ChargingStatus::time_to_full_minutes` returns `None` on overflow).
- **Timestamps before 3001.** `datetime.fromtimestamp` raises `OSError` on
  Windows from 3001-01-19 but works to year 9999 on Linux. openbyd ports the
  Linux behaviour (`ValueError` past 9999, `OSError` when `gmtime` overflows,
  `OverflowError` past `time_t`), and the generators avoid epochs past 3001 so
  vectors recorded on either platform agree.
- `serde_json` is built with `float_roundtrip`; without it some float inputs
  parse one ULP off and int fields disagree near 2^63.

## Pitfalls hit while building this

- pydantic's `str → int` is not Python's `int()`: `"0__1"` is 1 but `"1__0"` is
  an error, a `-` may follow leading zeros (`"0_-5"`), and what follows must be
  a JSON integer (`"0-05"` fails). `str → float` uses Rust's float parser after
  trimming, and strips underscores only from the *untrimmed* text (`" 1_0 "` fails).
- `bool` from a string does not trim (`" true"` fails) and `2` is an error.
- An `IntEnum` reads its input like `int`, but a failed parse goes to
  `_missing_` (UNKNOWN), except an integer outside i64, which is an error.
- `parse_byd_timestamp` uses `str.isdigit()` (true for `²`) to choose between
  `int()` and `fromisoformat`, and `int()` reads any Unicode decimal digit.
- CPython's C `fromisoformat` differs from `_pydatetime`: any single character
  (even multi-byte, even a digit) separates date and time; text between the time
  and a `+HH:MM` offset is ignored; `"10:00:00:12"` is 12 centiseconds; an offset
  of zero whole seconds is UTC even with microseconds. Port from the C behaviour
  (`base::datetime_fromisoformat`) and test against the real thing.
- `model_dump(mode="json")` keeps a `float` field's NaN as NaN, but turns NaN
  inside `dict[str, Any]` into `null`.
- serde_json's `Map::remove` reorders keys; use `shift_remove` where Python's
  `dict.pop` would keep the order.

## Debugging a mismatch

1. Take the `args` from the failure and replay both sides:
   ```python
   from parity.framework import load_groups, openbyd_answer
   group = load_groups()["models_charging"]
   group.answer(call, args), openbyd_answer(call, args)   # openbyd's answer has a "message"
   ```
   The `message` names the field and pydantic error type openbyd chose
   (`ValidationError (int_parsing) at full_hour`).
2. Shrink the payload to the one field that differs, then test the conversion
   alone against pydantic: `TypeAdapter(int).validate_python(value)` versus
   `parity.call("coerce.int", ...)`. If the difference is in a `coerce` function,
   add the input to `VALUES` in `parity/groups/coerce.py` so it stays covered.
3. If pydantic raises something other than `ValidationError` (`OverflowError`,
   `OSError`), the framework stops: either the generator left the shared domain
   (fix the generator, see `in_domain` in `parity/models_support.py`) or the
   Rust side must return that exception kind (`ValidationError::raised`).
4. Never change an oracle to make a case pass; port the odd behaviour and say
   so in a comment.
