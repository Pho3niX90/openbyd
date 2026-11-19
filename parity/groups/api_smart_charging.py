"""pybyd/_api/smart_charging.py: smart charging flows compared by trace.

``make_change_charge_fetch_fn`` and ``make_save_charging_schedule_fetch_fn``
return closures; their calls build the closure from the factory's
arguments and call it once per entry of ``calls`` (``{"endpoint",
"request_serial"}``), the way the client's trigger-and-poll routine does,
returning every ``[reply, serial]``. Results that are models are compared
as ``model_dump(mode="json")``.
"""

from __future__ import annotations

from pybyd._api import smart_charging as sc
from pybyd.models import EnergyType

from parity.api_support import (
    VIN,
    error_reply,
    flow_oracle,
    ok_reply,
    random_reply,
    random_scenario_base,
    raw_reply,
    scenario,
    transport_error,
)
from parity.framework import Group, random_text
from parity.groups.config import SAMPLE_SESSION
from parity.groups.pyjson import random_json, random_object

group = Group("api_smart_charging", crate="client")

HOMEPAGE = "/control/smartCharge/homePage"
TOGGLE = "/control/smartCharge/changeChargeStatue"
RESULT = "/control/smartCharge/changeResult"
SAVE = "/control/smartCharge/saveOrUpdate"
AC_LIMIT = "/control/smartCharge/getAcCurrentLimit"
CODES = ["1001", "1002", "1005", "1010", "1008", "6002", "6024", "8003", "9999", 1001]
OWNER = {VIN: False}
GUEST = {VIN: True}


def _model_flow(function):
    async def run(config, session, transport, *, vin, **kwargs):
        result = await function(config, session, transport, vin, **kwargs)
        return result.model_dump(mode="json")

    return run


def _pair_flow(function):
    async def run(config, session, transport, *, vin, **kwargs):
        return list(await function(config, session, transport, vin, **kwargs))

    return run


def _fetch_fn_flow(factory):
    async def run(config, session, transport, *, vin, calls, **factory_args):
        fetch = factory(**factory_args)
        results = []
        for call in calls:
            results.append(list(await fetch(call["endpoint"], config, session, transport, vin,
                                            call.get("request_serial"))))
        return results

    return run


flow_oracle(group, "api.smart_charging.fetch_charging_schedule", _model_flow(sc.fetch_charging_schedule))
flow_oracle(group, "api.smart_charging.toggle_smart_charging", _model_flow(sc.toggle_smart_charging))
flow_oracle(group, "api.smart_charging.fetch_ac_current_limit", _model_flow(sc.fetch_ac_current_limit))
flow_oracle(group, "api.smart_charging.trigger_charge_change", _pair_flow(sc.trigger_charge_change))
flow_oracle(group, "api.smart_charging.poll_charge_result", _pair_flow(sc.poll_charge_result))
flow_oracle(group, "api.smart_charging.trigger_save_charging_schedule", _pair_flow(sc.trigger_save_charging_schedule))
flow_oracle(group, "api.smart_charging.make_change_charge_fetch_fn", _fetch_fn_flow(sc.make_change_charge_fetch_fn))
flow_oracle(
    group,
    "api.smart_charging.make_save_charging_schedule_fetch_fn",
    _fetch_fn_flow(sc.make_save_charging_schedule_fetch_fn),
)


@group.oracle("api.smart_charging.is_charge_change_ready", errors=(AttributeError,))
def is_charge_change_ready(payload) -> bool:
    return sc.is_charge_change_ready(payload)


@group.oracle("api.smart_charging.constants")
def constants() -> dict:
    return {
        "_TOGGLE_ENDPOINT": sc._TOGGLE_ENDPOINT,
        "_RESULT_ENDPOINT": sc._RESULT_ENDPOINT,
        "_SAVE_ENDPOINT": sc._SAVE_ENDPOINT,
        "_GET_AC_LIMIT_ENDPOINT": sc._GET_AC_LIMIT_ENDPOINT,
        "VEHICLE_UNREACHABLE_CODES": sorted(sc.VEHICLE_UNREACHABLE_CODES),
    }


def ok(data) -> dict:
    return ok_reply(SAMPLE_SESSION, data)


SCHEDULE = {
    "soc": 80,
    "type": "1",
    "vehicleTimeZone": "Europe/Amsterdam",
    "smartChargeDto": {"startChargeTime": "22:00", "endChargeTime": "06:00", "chargeWay": "e", "status": 1},
    "smartJourneyDto": {"status": 0},
    "updateTime": 1700000000,
}
LIVE_EU = {"currentLimitList": ["6A", "8A", "10A", "16A", "MAX"], "currentEvCurrentLimit": "MAX"}
COMMON_REPLIES = [ok({}), ok([1]), ok("text"), ok(None), raw_reply({"code": "0"}), raw_reply([]), transport_error(502),
                  *(error_reply(code) for code in CODES)]
SERIALS = [None, "", "S1", 5, 0]
READY_SAMPLES = [{}, {"res": 1}, {"res": 2}, {"res": 3}, {"res": 0}, {"res": True}, {"res": False}, {"res": 2.0},
                 {"res": "2"}, {"res": None}, {"res": -1}, {"res": 2**64 - 1}, {"requestSerial": "S"},
                 None, [], "x", 0]


@group.recorded
def recorded():
    yield "api.smart_charging.constants", {}
    for sample in READY_SAMPLES:
        yield "api.smart_charging.is_charge_change_ready", {"payload": sample}

    for guests in [OWNER, GUEST]:
        for reply in [ok(SCHEDULE), ok({**SCHEDULE, "vin": "OTHERVIN"}), ok({"soc": "x"}), *COMMON_REPLIES]:
            yield "api.smart_charging.fetch_charging_schedule", scenario(guest_vins=guests, replies=[reply], vin=VIN)
        for enable in [True, False]:
            for reply in [ok({"result": "ok", "requestSerial": "S1"}), ok({"result": 5}), *COMMON_REPLIES]:
                yield "api.smart_charging.toggle_smart_charging", scenario(
                    guest_vins=guests, replies=[reply], vin=VIN, enable=enable
                )
        # start=False is BYD's known no-op stop (test_stop_charging).
        for start in [True, False]:
            for reply in [ok({"requestSerial": "S1"}), ok({"requestSerial": 7}), *COMMON_REPLIES]:
                yield "api.smart_charging.trigger_charge_change", scenario(
                    guest_vins=guests, replies=[reply], vin=VIN, start=start
                )
    for serial in SERIALS + ["S9"]:
        for reply in [ok({"res": 2, "message": "ok"}), ok({"res": 1, "requestSerial": "S2"}),
                      ok({"requestSerial": ""}), *COMMON_REPLIES]:
            yield "api.smart_charging.poll_charge_result", scenario(replies=[reply], vin=VIN, request_serial=serial)
    for times in [("22:00", "06:00", "e"), ("00:00", "full", "0,1,2,3,4"), ("", "", "s")]:
        for enabled in [True, False, None]:
            extra = {} if enabled is None else {"enabled": enabled}
            for reply in [ok({"requestSerial": "S1"}), *COMMON_REPLIES]:
                yield "api.smart_charging.trigger_save_charging_schedule", scenario(
                    replies=[reply], vin=VIN, start_charge_time=times[0], end_charge_time=times[1],
                    charge_way=times[2], **extra,
                )
    # test_ac_current_limit: the energy type is sent as str(int(...)).
    for energy_type in [None, int(EnergyType.HYBRID), 0, 1, -1, True, "EV", "2", ""]:
        extra = {} if energy_type is None else {"energy_type": energy_type}
        for reply in [ok(LIVE_EU), ok({"currentLimitList": [6, " 10A ", None, ""], "currentEvCurrentLimit": 16}),
                      ok({"currentLimitList": None, "currentEvCurrentLimit": "--"}), *COMMON_REPLIES]:
            yield "api.smart_charging.fetch_ac_current_limit", scenario(replies=[reply], vin=VIN, **extra)
    # The closures, driven as the trigger-and-poll routine drives them.
    sequences = [
        [{"endpoint": TOGGLE}, {"endpoint": RESULT, "request_serial": "S1"}, {"endpoint": RESULT, "request_serial": "S1"}],
        [{"endpoint": SAVE}, {"endpoint": RESULT, "request_serial": None}],
        [{"endpoint": RESULT, "request_serial": 5}, {"endpoint": "/other"}],
        [{"endpoint": RESULT}],
    ]
    replies = [ok({"requestSerial": "S1"}), ok({"res": 1}), ok({"res": 2, "requestSerial": "S2"})]
    for calls in sequences:
        for start in [True, False]:
            yield "api.smart_charging.make_change_charge_fetch_fn", scenario(
                replies=replies, vin=VIN, start=start, calls=calls
            )
        for enabled in [True, False]:
            yield "api.smart_charging.make_save_charging_schedule_fetch_fn", scenario(
                replies=replies, vin=VIN, start_charge_time="22:00", end_charge_time="full",
                charge_way="e", enabled=enabled, calls=calls,
            )
    for code in ["6002", "1001", "1008"]:
        yield "api.smart_charging.make_save_charging_schedule_fetch_fn", scenario(
            replies=[error_reply(code)], vin=VIN, start_charge_time="22:00", end_charge_time="06:00",
            charge_way="s", enabled=True, calls=sequences[1],
        )


def random_calls(rng) -> list[dict]:
    calls = []
    for _ in range(rng.randint(0, 4)):
        call = {"endpoint": rng.choice([TOGGLE, SAVE, RESULT, RESULT, random_text(rng, 8)])}
        if rng.random() < 0.7:
            call["request_serial"] = rng.choice(SERIALS + [random_text(rng, 6)])
        calls.append(call)
    return calls


def charge_data(rng):
    return rng.choice([
        {"res": rng.choice([0, 1, 2, 3, True, "2", None]), "requestSerial": rng.choice(SERIALS)},
        {"requestSerial": rng.choice(SERIALS)},
        SCHEDULE, LIVE_EU, random_object(rng), random_json(rng),
    ])


@group.fuzz
def fuzz(rng):
    while True:
        yield "api.smart_charging.is_charge_change_ready", {
            "payload": rng.choice(READY_SAMPLES + [random_object(rng), {"res": random_json(rng)}, random_json(rng)])
        }
        base = random_scenario_base(rng)
        session = base["session"]
        replies = [random_reply(rng, session, charge_data) for _ in range(rng.randint(0, 4))]
        vin = rng.choice([VIN, "", random_text(rng, 17)])
        common = {**base, "guest_vins": rng.choice([{}, OWNER, GUEST]), "replies": replies}
        choice = rng.randrange(8)
        if choice == 0:
            yield "api.smart_charging.fetch_charging_schedule", {**common, "args": {"vin": vin}}
        elif choice == 1:
            yield "api.smart_charging.toggle_smart_charging", {**common, "args": {"vin": vin, "enable": rng.random() < 0.5}}
        elif choice == 2:
            yield "api.smart_charging.trigger_charge_change", {**common, "args": {"vin": vin, "start": rng.random() < 0.5}}
        elif choice == 3:
            yield "api.smart_charging.poll_charge_result", {
                **common, "args": {"vin": vin, "request_serial": rng.choice(SERIALS + [random_text(rng, 6)])}
            }
        elif choice == 4:
            args = {"vin": vin, "start_charge_time": random_text(rng, 5), "end_charge_time": rng.choice(["full", "06:00"]),
                    "charge_way": rng.choice(["s", "e", "0,1"])}
            if rng.random() < 0.5:
                args["enabled"] = rng.random() < 0.5
            yield "api.smart_charging.trigger_save_charging_schedule", {**common, "args": args}
        elif choice == 5:
            args = {"vin": vin}
            if rng.random() < 0.7:
                args["energy_type"] = rng.choice([0, 1, 2, -1, True, False, "EV", random_text(rng, 4), 12345])
            yield "api.smart_charging.fetch_ac_current_limit", {**common, "args": args}
        elif choice == 6:
            yield "api.smart_charging.make_change_charge_fetch_fn", {
                **common, "args": {"vin": vin, "start": rng.random() < 0.5, "calls": random_calls(rng)}
            }
        else:
            yield "api.smart_charging.make_save_charging_schedule_fetch_fn", {
                **common,
                "args": {"vin": vin, "start_charge_time": "22:00", "end_charge_time": random_text(rng, 5),
                         "charge_way": "e", "enabled": rng.random() < 0.5, "calls": random_calls(rng)},
            }
