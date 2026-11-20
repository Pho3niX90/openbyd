"""Check the car-against-cloud comparison on synthetic pairs files with known injected errors.

Every line the helpers write is validated with ``export_pairs.check_line`` so the synthetic
data cannot drift from the real format. Two additions to the format (``gps_last_dist_m`` /
``gps_last_age_s`` on cloud lines, and new ``detail`` tokens) are left out of that check
while ``check_line`` does not know them yet.
"""

from __future__ import annotations

import json
import math
import os
import random
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import Any

import pytest

from bench.mapping import compare
from bench.mapping import export_pairs as pairs
from bench.replay import export_prod as ep

ROOT = Path(__file__).resolve().parents[2]
NEW_CLOUD_KEYS = ("gps_last_dist_m", "gps_last_age_s")


def _validate(row: dict[str, Any]) -> None:
    checked = dict(row)
    if checked["type"] == "cloud":
        for key in NEW_CLOUD_KEYS:
            if key not in pairs.CLOUD_LINE:
                checked.pop(key, None)
        if "detail" in checked and not pairs.DETAIL.fullmatch(str(checked["detail"])):
            checked.pop("detail")
        checked.pop("future_key", None)  # deliberately unknown: the tool must ignore it
    pairs.check_line(json.dumps(checked, separators=(",", ":")) + "\n")


class Pairs:
    """Builds a pairs file: a meta line, car lines and cloud lines, all valid per the schema."""

    def __init__(self, first_seen_t: float | None = None) -> None:
        self.car: list[dict[str, Any]] = []
        self.cloud: list[dict[str, Any]] = []
        self.raw: list[dict[str, Any]] = []
        self.first_seen_t = first_seen_t

    def car_row(self, t: float, cadence: str, **fields: Any) -> None:
        self.car.append({"type": "car", "t": round(t, 3), "cadence": cadence, **fields})

    def cloud_row(self, t: float, source: str, kind: str, **fields: Any) -> None:
        self.cloud.append({"type": "cloud", "t": round(t, 3), "source": source, "kind": kind, **fields})

    def raw_row(self, t: float, endpoint: str, fields: dict[str, Any] | None = None, source: str = "poll",
                **extra: Any) -> None:
        row = {"type": "raw", "t": round(t, 3), "source": source, "endpoint": endpoint, **extra}
        if fields is not None:
            row["fields"] = fields
        self.raw.append(row)

    def meta(self) -> dict[str, Any]:
        rows = {key: 0 for key in ep.ROW_KINDS}
        for row in self.cloud:
            bucket = f"{row['source']}_{row['kind']}"
            rows[bucket if bucket in rows else f"{row['source']}_other"] += 1
        extra = {} if self.first_seen_t is None else {"first_seen_t": round(self.first_seen_t, 3)}
        raw_rows = {"poll": 0, "mqtt": 0, "other": 0}
        endpoints = dict.fromkeys(pairs.RAW_ENDPOINTS, 0)
        for row in self.raw:
            raw_rows[row["source"]] += 1
            endpoints[row["endpoint"]] += 1
        return {"type": "meta", "version": pairs.VERSION, "start": "2026-03-01T00:00:00Z", "car_days": 29,
                "cloud_days": 13,
                "vin": pairs.SYNTHETIC_VIN, "model": "seal", "energy": "ev", "device_source": "car_app", **extra,
                "stats": {"car_rows": len(self.car), "cloud_rows": rows, "dropped_car_keys": {},
                          "dropped_cloud_keys": {}, "invalid_values": {}, "dropped_details": 0,
                          "unparsable_last_updated": 0, "cloud_fixes_matched": 0, "cloud_fixes_unmatched": 0,
                          "raw_rows": raw_rows, "raw_endpoints": endpoints, "raw_fields_dropped": {},
                          "raw_values_invalid": {}, "raw_invalid": {}, "raw_dropped_codes": 0}}

    def lines(self) -> list[str]:
        rows = [self.meta()] + sorted(self.car, key=lambda r: r["t"]) + sorted(self.cloud, key=lambda r: r["t"])
        rows += sorted(self.raw, key=lambda r: r["t"])
        out = []
        for row in rows:
            _validate(row)
            out.append(json.dumps(row, separators=(",", ":")) + "\n")
        return out

    def run(self, tmp_path: Path) -> tuple[dict[str, Any], str]:
        source = tmp_path / "pairs.jsonl"
        source.write_text("".join(self.lines()), encoding="ascii")
        results = compare.compare(source, tmp_path / "out")
        return results, (tmp_path / "out" / "report.md").read_text(encoding="utf-8")

    # --- car segments -------------------------------------------------------------------------

    def drive(self, t0: float, t1: float, speed: Any, odo0: float = 12000.0, soc: Any = 80) -> None:
        """Moving rows every 2 s; ``speed(t)`` in km/h; odometer integrates speed (whole km)."""
        dist = 0.0
        t = t0
        while t < t1:
            v = round(speed(t), 1)
            s = soc(t) if callable(soc) else soc
            self.car_row(t, "moving", speed_kph=v, soc_pct=s, odometer_km=math.floor(odo0 + dist),
                         power_level="on", gear="D", twelve_volt=14.1, remaining_kwh=round(0.8 * s, 1),
                         lifetime_kwh=round(8000 + dist * 0.16, 1), has_fix=True, fix_speed_kph=v, fix_age_s=0.5,
                         fix_accuracy_m=4.0, charging_gun_raw=1, charger_state_raw=0, charging_type_raw=1,
                         tyre_kpa={"fl": 240, "fr": 238, "rl": 270, "rr": 272}, outside_temp_c=12)
            dist += v / 3600 * 2
            t += 2

    def stationary(self, t0: float, t1: float, every: float = 60, **fields: Any) -> None:
        base = {"power_level": "on", "gear": "P", "speed_kph": 0.0, "soc_pct": 80, "odometer_km": 12345,
                "charging_gun_raw": 1, "charger_state_raw": 0, "charging_type_raw": 1, "twelve_volt": 13.6}
        base.update(fields)
        t = t0
        while t < t1:
            self.car_row(t, "stationary", **{k: (v(t) if callable(v) else v) for k, v in base.items()})
            t += every

    def charging(self, t0: float, t1: float, every: float = 60, **fields: Any) -> None:
        base = {"power_level": "off", "gear": "P", "speed_kph": 0.0, "soc_pct": 60, "odometer_km": 12345,
                "charging_gun_raw": 2, "charger_state_raw": 1, "charging_type_raw": 2, "charging_power_kw": 5.5,
                "charge_remaining_min": 150, "twelve_volt": 13.9}
        base.update(fields)
        t = t0
        while t < t1:
            self.car_row(t, "charging", **{k: (v(t) if callable(v) else v) for k, v in base.items()})
            t += every

    def parked(self, t0: float, t1: float, every: float = 300, **fields: Any) -> None:
        base = {"power_level": "off", "gear": "P", "soc_pct": 60, "charging_gun_raw": 1, "charger_state_raw": 0,
                "charging_type_raw": 1, "twelve_volt": 12.6}
        base.update(fields)
        t = t0
        while t < t1:
            self.car_row(t, "event", **base)
            t += every


# --- the injected errors ---------------------------------------------------------------------------


def test_soc_lag_while_charging_is_recovered(tmp_path: Path) -> None:
    lag = 120

    def soc(t: float) -> int:
        return 50 + max(0, int((t - 1017) // 420))

    b = Pairs()
    b.charging(1000, 8200, soc_pct=soc)
    # The car records a SoC step at once: a row at every change (off the 60 s grid).
    for k in range(1, 18):
        b.car_row(1017 + 420 * k, "charging", power_level="off", gear="P", speed_kph=0.0, soc_pct=soc(1017 + 420 * k),
                  odometer_km=12345, charging_gun_raw=2, charger_state_raw=1, charging_type_raw=2,
                  charging_power_kw=5.5, charge_remaining_min=150)
    t = 1300.0
    while t < 8100:
        s = soc(t - lag)
        b.cloud_row(t, "mqtt", "smartCharge", soc=s,
                    charging={"is_charging": True, "is_connected": True, "connect_state": 1, "soc": s})
        t += 37
    results, report = b.run(tmp_path)
    soc_push = results["soc"]["mqtt:smartCharge soc"]
    best = soc_push["best_fit_lag_while_changing"]
    assert abs(best["best_lag_s"] - lag) <= 20
    assert best["agreement_at_best"] == 1.0 and best["determined"] is True
    assert best["agreement_at_lag0"] < 0.85
    per_sample = soc_push["lag_s_while_changing"]
    assert per_sample["max"] <= lag and per_sample["max"] >= lag - 40
    assert soc_push["lag_negative_share"] == 0
    assert per_sample["states"] == {"charging": per_sample["n"]}
    assert results["soc"]["mqtt:smartCharge charging.soc"]["best_fit_lag_while_changing"]["best_lag_s"] == \
        best["best_lag_s"]
    assert "Best-fit constant lag" in report


def test_speed_lag_is_recovered(tmp_path: Path) -> None:
    def speed(t: float) -> float:
        return 60 + 40 * math.sin(2 * math.pi * t / 180)

    b = Pairs()
    b.drive(2000, 5600, speed)
    t = 2400.0
    while t < 5500:
        b.cloud_row(t, "mqtt", "vehicleInfo", speed_kph=round(speed(t - 30), 1), vehicle_on=True, online=True,
                    asleep=False)
        t += 23
    results, _ = b.run(tmp_path)
    best = results["speed"]["mqtt:vehicleInfo"]["best_lag"]
    assert best["best_lag_s"] == 30
    assert best["mae_at_best_kph"] < 1.0
    assert best["mae_at_lag0_kph"] > 10
    gps = results["speed"]["car_speed_vs_gps_speed"]
    assert gps["ratio_car_over_gps"]["median"] == 1.0
    assert gps["fit_car_on_gps"]["slope"] == 1.0


def test_odometer_offset_is_recovered(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 4600)
    for k in range(25):
        b.cloud_row(1030 + 120 * k, "mqtt", "vehicleInfo", odometer_km=12345.4, vehicle_on=True)
    results, report = b.run(tmp_path)
    odo = results["odometer"]["mqtt:vehicleInfo"]
    assert odo["diff_cloud_minus_car_km"]["median"] == 0.4
    assert odo["diff_cloud_minus_car_km"]["n"] == 25
    assert odo["with_decimals"] == 25
    assert odo["diff_cloud_minus_car_km"]["states"] == {"on_stationary": 25}
    assert "contaminated" in report


def test_charge_power_scale_is_recovered(tmp_path: Path) -> None:
    b = Pairs()
    powers = [3.0, 4.2, 5.5, 6.1, 7.0]
    for k in range(60):
        p = powers[k % 5]
        t = 1000 + 60 * k
        b.car_row(t, "charging", power_level="off", soc_pct=40, charging_gun_raw=2, charger_state_raw=1,
                  charging_type_raw=2, charging_power_kw=p, charge_remaining_min=200 - k)
        b.cloud_row(t + 1, "mqtt", "vehicleInfo", charge_power_kw=round(0.9 * p, 3))
        b.cloud_row(t + 2, "poll", "ok", charging={"is_charging": True, "is_connected": True, "connect_state": None,
                                                     "minutes_to_full": 200 - k + 7})
    results, _ = b.run(tmp_path)
    cp = results["charge_power"]["mqtt:vehicleInfo"]["while_car_charging"]
    assert cp["fit_cloud_on_car"]["slope"] == 0.9
    assert abs(cp["fit_cloud_on_car"]["offset"]) < 0.01
    assert cp["ratio_cloud_over_car"]["median"] == 0.9
    assert cp["fit_cloud_on_car"]["n"] == 60
    assert cp["within_tolerance_share"] is not None
    assert cp["cloud_value_kinds"] == {"positive": 60}
    ttf = results["time_to_full"]["poll:ok charging.minutes_to_full"]["diff_cloud_minus_car_min"]
    assert ttf["median"] == 7 and ttf["n"] == 60


def test_gl_fallback_power_while_on_and_unplugged(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 4600)
    for k in range(12):
        b.cloud_row(1010 + 300 * k, "mqtt", "vehicleInfo", charge_power_kw=1.2, vehicle_on=True, speed_kph=0)
    results, _ = b.run(tmp_path)
    cp = results["charge_power"]["mqtt:vehicleInfo"]
    unplugged = cp["cloud_positive_while_car_unplugged"]
    assert unplugged["n"] == 12 and unplugged["median"] == 1.2
    assert unplugged["states"] == {"on_stationary": 12}
    assert cp["while_car_charging"]["fit_cloud_on_car"]["n"] == 0


def test_unplug_lags_on_polls_and_push(tmp_path: Path) -> None:
    tc = 5000.0
    b = Pairs()
    b.charging(1000, tc)
    b.parked(tc, 9000)  # first row at tc: unplugged
    charging_on = {"is_charging": True, "is_connected": True, "connect_state": None}
    charging_off = {"is_charging": False, "is_connected": False, "connect_state": None}
    for t in (3800, 4400):
        b.cloud_row(t, "poll", "ok", asleep=True, online=False, charging=charging_on)
    b.cloud_row(tc + 300, "poll", "ok", asleep=True, online=False, charging=charging_on)  # stale
    b.cloud_row(tc + 600, "poll", "ok", asleep=True, online=False, charging=charging_off)
    b.cloud_row(tc + 900, "poll", "ok", asleep=True, online=False, charging=charging_off)
    b.cloud_row(tc - 100, "mqtt", "smartCharge", charging={**charging_on, "connect_state": 1})
    b.cloud_row(tc + 20, "mqtt", "smartCharge", charging={**charging_off, "connect_state": 0})
    results, report = b.run(tmp_path)
    unplug = results["plug_charging"]["transitions"]["unplug"]
    assert unplug["car_transitions"] == 1
    assert unplug["poll"]["delay_s"]["median"] == 600
    assert unplug["poll"]["old_state_shown_after_transition"] == 1
    assert unplug["push"]["delay_s"]["median"] == 20
    assert unplug["push"]["old_state_shown_after_transition"] == 0
    stop = results["plug_charging"]["transitions"]["charge_stop"]
    assert stop["poll"]["delay_s"]["median"] == 600
    assert stop["push"]["delay_s"]["median"] == 20
    assert unplug["poll"]["delay_s"]["states"] == {"parked_off": 1}
    table = results["plug_charging"]["mqtt:smartCharge"]["no_flag"]["charging.connect_state x charging_gun_raw"]
    assert table["rows"] == {"0": {"1": 1}, "1": {"2": 1}}
    assert "unplug" in report


def test_vehicle_on_false_while_car_on(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 4600)
    for k in range(10):
        b.cloud_row(1020 + 300 * k, "mqtt", "vehicleInfo", vehicle_on=False, online=True, asleep=False)
    results, report = b.run(tmp_path)
    s = results["vehicle_on"]["mqtt:vehicleInfo"]
    for definition in ("power_on_only", "power_acc_on_ok", "cadence_powered"):
        assert s["callouts"][definition]["vehicle_on_false_while_car_on"] == 10
        assert s["callouts"][definition]["vehicle_on_true_while_car_off"] == 0
    assert s["vehicle_on_x_power_level_not_moving"]["rows"] == {"false": {"on": 10}}
    assert "Callouts with car on = power_on_only" in report and "vehicle_on_false_while_car_on 10" in report


def test_stale_car_timeline_is_unknown(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 2000)
    b.stationary(9200, 10000)  # a two-hour silence in between
    b.cloud_row(5000, "mqtt", "vehicleInfo", soc=80, vehicle_on=True, speed_kph=0)
    b.cloud_row(1200, "mqtt", "vehicleInfo", soc=80, vehicle_on=True, speed_kph=0)
    results, _ = b.run(tmp_path)
    soc = results["soc"]["mqtt:vehicleInfo soc"]
    assert soc["cloud_values"] == 2
    assert soc["no_fresh_car_value"] == 1
    assert soc["diff_cloud_minus_car"]["n"] == 1
    matrix = results["vehicle_on"]["mqtt:vehicleInfo"]["vehicle_on_x_power_level_not_moving"]
    assert matrix["rows"] == {"true": {"on": 1, "unknown": 1}}
    assert matrix["states"] == {"on_stationary": 1, "unknown": 1}
    assert results["inventory"]["car_fresh_share"] < 0.3


def test_restored_poll_soc_is_excluded_from_byd_values(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 6000)
    for k in range(3):
        b.cloud_row(1100 + 100 * k, "poll", "ok", soc=80, asleep=False, online=True, gps_fresh=True)
    for k in range(4):
        b.cloud_row(2100 + 100 * k, "poll", "ok", soc=55, asleep=True, online=False, gps_fresh=False)
    b.cloud_row(3100, "poll", "ok", soc=80, asleep=True, online=False,
                charging={"is_charging": False, "is_connected": False, "connect_state": None, "soc": 80})
    results, _ = b.run(tmp_path)
    byd = results["soc"]["poll:ok soc (BYD's: asleep false or charging.soc present)"]["diff_cloud_minus_car"]
    every = results["soc"]["poll:ok soc (all, may be restored)"]["diff_cloud_minus_car"]
    assert byd["n"] == 4 and byd["max"] == 0
    assert every["n"] == 8 and every["min"] == -25
    s = results["vehicle_on"]["poll:ok"]["callouts"]["power_on_only"]
    assert s["asleep_true_while_car_on"] == 5


def test_empty_sections_say_so(tmp_path: Path) -> None:
    b = Pairs()
    b.parked(1000, 3000)
    results, report = b.run(tmp_path)
    assert results["soc"]["mqtt:vehicleInfo soc"]["diff_cloud_minus_car"] == {
        "n": 0, "days": 0, "clusters": 0, "states": {}, "note": "no samples"}
    assert results["charge_power"]["poll:ok"]["while_car_charging"]["fit_cloud_on_car"]["n"] == 0
    assert results["plug_charging"]["transitions"]["unplug"]["car_transitions"] == 0
    assert results["speed"]["mqtt:vehicleInfo"]["best_lag"]["best_lag_s"] is None
    assert results["inventory"]["cloud_rows"]["total"] == 0
    assert "no samples" in report and "No paired samples." in report


def test_meta_only_file(tmp_path: Path) -> None:
    results, report = Pairs().run(tmp_path)
    assert results["inventory"]["car_rows"]["kept"] == 0
    assert results["car_reference"]["kwh_per_soc_pct_fit_remaining_on_soc"]["n"] == 0
    assert "## 13. Limits" in report


def test_duplicates_dropped(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 1600)
    b.car_row(1060, "stationary", power_level="on", gear="P", speed_kph=0.0, soc_pct=80, odometer_km=12345,
              charging_gun_raw=1, charger_state_raw=0, charging_type_raw=1, twelve_volt=13.6)  # exact duplicate
    b.car_row(1061.0, "stationary", power_level="on", gear="P", speed_kph=0.0, soc_pct=80, odometer_km=12345,
              charging_gun_raw=1, charger_state_raw=0, charging_type_raw=1, twelve_volt=13.6)  # resend
    results, _ = b.run(tmp_path)
    rows = results["inventory"]["car_rows"]
    assert rows["read"] == 12 and rows["kept"] == 10
    assert rows["exact_duplicates_dropped"] == 1 and rows["resends_dropped"] == 1


def test_format_additions_and_unknown_keys(tmp_path: Path) -> None:
    b = Pairs()
    b.drive(1000, 1400, lambda t: 50.0, odo0=12345.2)  # ends at odometer 12350
    b.parked(1400, 9000, soc_pct=79)
    # The fix is from after the drive ended: parked since, so it counts.
    b.cloud_row(5000, "poll", "ok", gps_present=True, gps_fresh=True, gps_last_dist_m=35, gps_last_age_s=3595.0,
                future_key=7)
    b.cloud_row(5100, "poll", "skip", detail="min_gap")
    b.cloud_row(5200, "poll", "error", detail="1008")
    b.cloud_row(5300, "poll", "skip", detail="not_selected")
    # A fix taken early in the drive: the odometer changed since, so it does not count as parked.
    b.cloud_row(1500, "poll", "ok", gps_present=True, gps_fresh=True, gps_last_dist_m=900, gps_last_age_s=450.0)
    results, report = b.run(tmp_path)
    buckets = results["inventory"]["cloud_rows"]["by_source_kind_detail"]
    assert buckets["poll:skip:min_gap"] == 1 and buckets["poll:error:1008"] == 1
    assert buckets["poll:skip:not_selected"] == 1
    pos = results["position"]["poll:ok gps_fresh true"]
    assert pos["parked_gps_last_dist_m"]["n"] == 1 and pos["parked_gps_last_dist_m"]["median"] == 35
    assert pos["counts"]["gps_last_not_parked_or_unprovable"] == 1
    assert "future_key" in results["inventory"]["cloud_keys"]["poll:ok"]


def test_energy_over_last_50_km(tmp_path: Path) -> None:
    b = Pairs()
    b.drive(1000, 1000 + 3 * 3600, lambda t: 60.0)  # 180 km at 0.16 kWh/km
    b.parked(1000 + 3 * 3600, 1000 + 4 * 3600)
    b.cloud_row(1000 + 3 * 3600 + 600, "poll", "ok", last50km_energy_kwh=8.8,
                energy_distribution={"drive": 80, "electric": 10, "hvac": 6, "other": 4})
    results, _ = b.run(tmp_path)
    e = results["energy"]["poll:ok"]["vs_lifetime_kwh"]
    assert e["ratio_cloud_over_car"]["n"] == 1
    # 8.8 / (50 km x 0.16 kWh/km) = 1.1; whole-km odometer readings make the car window up to 1 km longer.
    assert abs(e["ratio_cloud_over_car"]["median"] - 1.1) < 0.05
    assert results["energy"]["energy_distribution"]["poll:ok"]["sum_within_tolerance_of_100_share"] == 1.0


def test_hvac_and_car_reference(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 4600, remaining_kwh=lambda t: round(0.8 * (60 + (t - 1000) // 600), 1),
                 soc_pct=lambda t: 60 + int((t - 1000) // 600), doors={"fl": "closed", "fr": "closed"})
    b.cloud_row(1100, "poll", "ok", hvac_on=True)
    b.cloud_row(1200, "poll", "ok", hvac_on=None)
    results, _ = b.run(tmp_path)
    assert results["hvac_on"]["poll:ok"]["rows"] == {"null": {"on_stationary": 1}, "true": {"on_stationary": 1}}
    ref = results["car_reference"]
    assert ref["kwh_per_soc_pct_fit_remaining_on_soc"]["slope"] == 0.8
    assert "lock" in ref["car_keys_never_seen"] and "alarm" in ref["car_keys_never_seen"]
    assert ref["door_values"]["fl"] == {"closed": 60}
    limits = {(x["quantity"], x["side"]) for x in results["limits"]}
    assert ("twelve_volt", "car_only") in limits and ("hvac_on", "cloud_only") in limits


# --- privacy and the command line --------------------------------------------------------------------


def _full_scenario() -> Pairs:
    b = Pairs()
    b.drive(4321.987, 6000, lambda t: 70.0)
    b.charging(6000, 9000)
    b.cloud_row(8765.432, "mqtt", "vehicleInfo", soc=61.789, speed_kph=0, vehicle_on=False, charge_power_kw=5.123)
    b.cloud_row(5000.321, "mqtt", "vehicleInfo", soc=80, speed_kph=69.5, vehicle_on=True, odometer_km=12013.6)
    b.cloud_row(7777.777, "poll", "ok", soc=60, asleep=False, gps_present=True, gps_fresh=True, gps_dist_m=4321,
                gps_step_m=0, charging={"is_charging": True, "is_connected": True, "connect_state": None})
    return b


def test_outputs_carry_no_input_times_or_rows(tmp_path: Path) -> None:
    b = _full_scenario()
    lines = b.lines()
    b.run(tmp_path)
    outputs = [(tmp_path / "out" / name).read_text(encoding="utf-8") for name in ("results.json", "report.md")]
    for text in outputs:
        for needle in ("4321.987", "8765.432", "5000.321", "7777.777", "61.789", "5.123", "12013.6"):
            assert needle not in text, needle
        for line in lines:
            assert line.strip() not in text
        for row in b.car[:50] + b.cloud:
            assert json.dumps(row, separators=(",", ":")) not in text
            assert json.dumps(row) not in text


ALLOWED_NUMBER_LISTS = {"speed_lags_s", "value_lags_s", "tied_lags_s"}


def _number_lists(node: Any, key: str = "") -> list[str]:
    """Keys of every list of bare numbers in a JSON tree, other than the allowed constant ranges."""
    found = []
    if isinstance(node, dict):
        for k, v in node.items():
            found += _number_lists(v, k)
    elif isinstance(node, list):
        if node and all(type(x) in (int, float) for x in node) and key not in ALLOWED_NUMBER_LISTS:
            found.append(key)
        for x in node:
            found += _number_lists(x, key)
    return found


def _keys(node: Any) -> set[str]:
    if isinstance(node, dict):
        return set(node) | {k for v in node.values() for k in _keys(v)}
    if isinstance(node, list):
        return {k for v in node for k in _keys(v)}
    return set()


def test_no_per_event_lists_are_written(tmp_path: Path) -> None:
    b = Pairs()
    b.charging(1000, 3000)
    b.parked(3000, 6000)  # one unplug: a single derived delay
    b.cloud_row(3300, "poll", "ok", asleep=False,
                charging={"is_charging": False, "is_connected": False, "connect_state": None})
    results, report = b.run(tmp_path)
    assert _number_lists(results) == []
    assert "values" not in _keys(results)
    assert "- values" not in report


def test_deterministic_and_command_line(tmp_path: Path) -> None:
    b = _full_scenario()
    source = tmp_path / "pairs.jsonl"
    source.write_text("".join(b.lines()), encoding="ascii")
    compare.main([str(source), "--out-dir", str(tmp_path / "a")])
    env = dict(os.environ)
    completed = subprocess.run([sys.executable, "-m", "bench.mapping.compare", str(source), "--out-dir",
                                str(tmp_path / "b")], cwd=ROOT, env=env, capture_output=True, text=True, check=True)
    assert "wrote results.json and report.md" in completed.stdout
    assert "4321.987" not in completed.stdout
    for name in ("results.json", "report.md"):
        assert (tmp_path / "a" / name).read_bytes() == (tmp_path / "b" / name).read_bytes()


def test_bad_lines_are_counted_not_fatal(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 1300)
    source = tmp_path / "pairs.jsonl"
    source.write_text("".join(b.lines()) + "not json\n[1]\n{\"type\":\"other\"}\n{\"type\":\"car\"}\n",
                      encoding="ascii")
    results = compare.compare(source, tmp_path / "out")
    assert results["inventory"]["skipped_lines"] == {"car_without_t": 1, "invalid_json": 1, "not_an_object": 1,
                                                     "unknown_type": 1}


@pytest.mark.parametrize("value, expected", [(True, "true"), (None, "null"), (compare.ABSENT, "absent"), (2, "2"),
                                             (2.5, "2.5"), (False, "false")])
def test_labels(value: Any, expected: str) -> None:
    assert compare.label(value) == expected


# --- phase 2: behaviours added for the real export ----------------------------------------------------


def _parked_gap(b: Pairs, odo_after: int = 12345, soc_after: int = 70, gun_after: int = 1) -> None:
    """On and stationary, then off (event rows), a two-hour silent gap, off again, then on again."""
    b.stationary(1000, 1300, soc_pct=70)
    b.parked(1300, 1700, soc_pct=70)
    b.parked(9000, 9400, soc_pct=soc_after, charging_gun_raw=gun_after)
    b.stationary(9400, 9700, soc_pct=soc_after, odometer_km=odo_after)


def test_parked_inferred_across_a_silent_gap(tmp_path: Path) -> None:
    b = Pairs()
    _parked_gap(b)
    b.cloud_row(5000, "poll", "ok", soc=70, asleep=True, online=False)
    results, report = b.run(tmp_path)
    diff = results["soc"]["poll:ok soc (all, may be restored)"]["diff_cloud_minus_car"]
    assert diff["n"] == 1 and diff["states"] == {"parked_inferred": 1}
    hours = results["inventory"]["car_hours_by_state"]
    assert hours["parked_inferred"] > 1.5
    assert "parked_inferred" in report


@pytest.mark.parametrize("change", [{"odo_after": 12350}, {"soc_after": 69}, {"gun_after": 2}])
def test_no_inference_when_the_car_changed(tmp_path: Path, change: dict[str, int]) -> None:
    b = Pairs()
    _parked_gap(b, **change)
    b.cloud_row(5000, "poll", "ok", soc=70, asleep=True, online=False)
    results, _ = b.run(tmp_path)
    soc = results["soc"]["poll:ok soc (all, may be restored)"]
    assert soc["diff_cloud_minus_car"]["n"] == 0 and soc["no_fresh_car_value"] == 1


def test_rounding_rule_agreement(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 2000)  # car SoC 80
    for k in range(5):
        b.cloud_row(1010 + 100 * k, "mqtt", "vehicleInfo", soc=80.6)
    results, _ = b.run(tmp_path)
    s = results["soc"]["mqtt:vehicleInfo soc"]
    assert s["with_decimals"] == 5
    assert s["rounding_rules"]["pairs"] == 5
    assert s["rounding_rules"]["rule_evaluations_agreeing"] == {"round_half_up": 0, "floor": 5, "exact": 0}
    assert s["better_rounding_rule"] == "floor"


def test_vehicle_info_charge_power_as_charge_signal(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 2000, charging_gun_raw=2, charger_state_raw=1, charging_type_raw=1)  # plugged, idle
    b.charging(2000, 4000)  # charge starts at 2000
    b.parked(4000, 6000, charging_gun_raw=2, charger_state_raw=1, charging_type_raw=1)  # stops at 4000
    b.cloud_row(1500, "mqtt", "vehicleInfo", charge_power_kw=0)
    b.cloud_row(2040, "mqtt", "vehicleInfo", charge_power_kw=5.4)
    b.cloud_row(4010, "mqtt", "vehicleInfo", charge_power_kw=None)  # says nothing
    b.cloud_row(4030, "mqtt", "vehicleInfo", charge_power_kw=0)
    results, report = b.run(tmp_path)
    tr = results["plug_charging"]["transitions"]
    assert tr["charge_start"]["push_vehicleInfo_power"]["delay_s"]["median"] == 40
    assert tr["charge_stop"]["push_vehicleInfo_power"]["delay_s"]["median"] == 30
    assert tr["cloud_rows"] == {"poll": 0, "push": 0, "push_vehicleInfo_power": 4}
    assert "push_vehicleInfo_power" not in tr["plug_in"]
    assert "push_vehicleInfo_power" in report


def test_callouts_per_definition_and_cadence(tmp_path: Path) -> None:
    b = Pairs()
    # Stationary rows whose power level is unreadable: only the cadence says the car is powered.
    for k in range(20):
        b.car_row(1000 + 60 * k, "stationary", speed_kph=0.0, soc_pct=80, odometer_km=12345)
    for k in range(6):
        b.cloud_row(1030 + 120 * k, "poll", "ok", vehicle_on=False, asleep=False, online=True, speed_kph=0)
    results, _ = b.run(tmp_path)
    calls = results["vehicle_on"]["poll:ok"]["callouts"]
    assert calls["power_on_only"]["vehicle_on_false_while_car_on"] == 0
    assert calls["power_acc_on_ok"]["vehicle_on_false_while_car_on"] == 0
    assert calls["cadence_powered"]["vehicle_on_false_while_car_on"] == 6


def test_power_gear_table(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 2000)  # power on
    b.parked(2000, 4000)  # power off
    b.cloud_row(1100, "poll", "ok", vehicle_on=True, asleep=False, online=True, speed_kph=0)
    b.cloud_row(1200, "poll", "ok", vehicle_on=None, asleep=True, online=False, speed_kph=None)  # excluded
    b.cloud_row(2500, "poll", "ok", vehicle_on=False, asleep=False, online=True, speed_kph=None)
    results, _ = b.run(tmp_path)
    table = results["vehicle_on"]["poll:ok"]["power_gear_x_power_level"]
    assert table["rows"] == {"powerGear 1 (OFF) or unknown + vehicleState 0/2": {"off": 1},
                             "powerGear 3 (ON)": {"on": 1}}
    assert results["vehicle_on"]["poll:ok"]["power_gear_x_car_cadence"]["rows"]["powerGear 3 (ON)"] == {
        "stationary": 1}


def test_pushes_after_car_changes_and_while_parked(tmp_path: Path) -> None:
    b = Pairs()
    b.parked(1000, 2000)
    b.stationary(2000, 3000)  # power on at 2000
    b.parked(3000, 5000)  # power off at 3000
    b.cloud_row(2025, "mqtt", "vehicleInfo", soc=80, vehicle_on=True)
    b.cloud_row(2026.5, "mqtt", "remoteControl")
    b.cloud_row(3500, "mqtt", "vehicleInfo", soc=60, vehicle_on=False, charge_power_kw=None)
    results, report = b.run(tmp_path)
    pu = results["pushes"]
    assert pu["first_cloud_row_after_car_event"]["power_on"]["mqtt:vehicleInfo"]["delay_s"]["median"] == 25
    assert pu["first_cloud_row_after_car_event"]["power_off"]["mqtt:vehicleInfo"]["delay_s"]["median"] == 500
    assert pu["vehicleInfo_while_car_off_not_moving_by_state"] == {"parked_off": 1}
    assert pu["vehicleInfo_while_car_off_keys_with_values"] == {"soc": 1, "vehicle_on": 1}
    assert pu["vehicleInfo_to_nearest_remoteControl_s"]["min"] == 1.5
    assert pu["vehicleInfo_with_remoteControl_at_or_before_share"] == 0.5  # the later push follows it
    assert "## 14. Pushes" in report


def test_poll_periods_and_details(tmp_path: Path) -> None:
    b = Pairs(first_seen_t=2000)
    for k in range(5):
        b.cloud_row(100 + 300 * k, "poll", "ok", soc=80, asleep=False)  # before registration
    for k in range(3):
        b.cloud_row(2100 + 200 * k, "poll", "ok", asleep=True, vehicle_on=None, vehicle_unreachable=True)
    b.stationary(3000, 4000)
    for k in range(5):
        b.cloud_row(3010 + 60 * k, "poll", "ok", soc=80, asleep=k == 0, vehicle_unreachable=False)
    b.cloud_row(3020, "poll", "error", detail="1008")
    b.cloud_row(3030, "poll", "skip", detail="busy")
    results, report = b.run(tmp_path)
    po = results["polls"]
    per = po["per_period"]
    assert po["registration_source"] == "meta.first_seen_t"
    assert (per["before_registration"]["poll_ok"], per["before_registration"]["awake_share"]) == (5, 1.0)
    assert (per["registered_no_rows"]["poll_ok"], per["registered_no_rows"]["awake"]) == (3, 0)
    assert (per["car_rows"]["poll_ok"], per["car_rows"]["awake"]) == (5, 4)
    assert per["before_registration"]["interval_s"]["median"] == 300
    assert per["car_rows"]["interval_s"]["median"] == 60
    assert po["asleep_unreachable_check"]["registered_no_rows"] == {
        "asleep_polls": 3, "vehicle_on_null": 3, "vehicle_unreachable_true": 3}
    bins = {x["bin"]: x for x in po["half_day_bins_from_registration"]}
    assert bins[-1]["poll_ok"] == 5 and bins[0]["poll_ok"] == 8
    assert bins[0]["asleep_share"] == round(4 / 8, 3) and bins[0]["vehicle_unreachable_share"] == round(3 / 8, 3)
    assert po["poll_rows_kind_detail_x_car_state"]["rows"]["error:1008"] == {"on_stationary": 1}
    assert po["poll_rows_kind_detail_x_period"]["rows"]["skip:busy"] == {"car_rows": 1}
    assert report.index("## 0. Regime") < report.index("## 1. Inventory")


def test_periods_fall_back_to_the_first_car_row(tmp_path: Path) -> None:
    b = Pairs()
    b.cloud_row(100, "poll", "ok", soc=80)
    b.stationary(3000, 3200)
    b.cloud_row(3010, "poll", "ok", soc=80)
    results, _ = b.run(tmp_path)
    po = results["polls"]
    assert po["registration_source"] == "first car row"
    assert po["per_period"]["before_registration"]["poll_ok"] == 1 and po["per_period"]["car_rows"]["poll_ok"] == 1


def test_online_latency_after_power_off(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 2000)
    b.parked(2000, 8000)  # power off at 2000
    b.cloud_row(2100, "poll", "ok", online=True, asleep=False)
    b.cloud_row(2700, "poll", "ok", online=False, asleep=True)
    results, _ = b.run(tmp_path)
    res = results["online_latency"]["poll:ok"]
    assert res["asleep_true_after_power_off"]["delay_s"]["median"] == 700
    assert res["online_false_after_power_off"]["delay_s"]["median"] == 700
    since = res["hours_since_car_power_off_when_cloud_says"]
    assert since["online_true"]["n"] == 1 and since["online_true"]["median"] == round(100 / 3600, 3)


def test_contamination_hint(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 9000)  # car SoC 80
    b.cloud_row(1100, "mqtt", "vehicleInfo", soc=70)
    b.cloud_row(1700, "poll", "ok", soc=80, asleep=True)  # push 10 min old differs: a hint
    b.cloud_row(8000, "poll", "ok", soc=80, asleep=True)  # push too old to compare
    results, _ = b.run(tmp_path)
    con = results["poll_contamination"]["soc"]
    assert con["hint_count"] == 1
    assert con["poll_vs_car_x_latest_push"]["rows"] == {"poll_equals_car": {"no_push_within_window": 1,
                                                                            "push_differs": 1}}


# --- fix round: frozen values, transition categories, awake split, jitter, clusters --------------------


def test_frozen_cloud_value_against_a_changing_car(tmp_path: Path) -> None:
    b = Pairs()
    b.charging(1000, 8200, soc_pct=lambda t: 50 + int((t - 1000) // 600))  # car SoC rises 50 -> 61
    frozen = {"is_charging": True, "is_connected": True, "connect_state": None, "soc": 50}
    for k in range(40):
        b.cloud_row(1030 + 180 * k, "poll", "ok", soc=50, asleep=True, charging=frozen)
    results, _ = b.run(tmp_path)
    diff = results["soc"]["poll:ok soc (all, may be restored)"]["diff_cloud_minus_car"]
    assert diff["n"] == 40 and diff["max"] == 0 and diff["min"] <= -10
    runs = results["plug_charging"]["poll:ok"]["asleep_runs"]
    assert runs == {**runs, "asleep_rows": 40, "runs": 1, "constant_runs": 1, "rows_in_constant_runs": 40}
    assert runs["both_flags_x_run_kind"]["rows"] == {"both_right": {"constant_run": 40}}


def test_transition_categories(tmp_path: Path) -> None:
    b = Pairs()
    b.charging(1000, 3000)
    b.parked(3000, 6000)  # unplug at 3000
    b.charging(6000, 8000)  # plug in at 6000
    b.parked(8000, 12000)  # unplug at 8000
    on = {"is_charging": True, "is_connected": True, "connect_state": None}
    off = {"is_charging": False, "is_connected": False, "connect_state": None}
    # Between 3000 and 6000 no poll at all. Between 8000 and the end only asleep polls, still saying plugged.
    for t in (8500, 9500):
        b.cloud_row(t, "poll", "ok", asleep=True, charging=on)
    b.cloud_row(6300, "poll", "ok", asleep=False, charging=on)  # plug-in shown by an awake poll
    b.cloud_row(10500, "poll", "ok", asleep=True, charging=off)
    results, report = b.run(tmp_path)
    tr = results["plug_charging"]["transitions"]
    unplug = tr["unplug"]["poll"]["categories"]
    assert unplug["no_rows"] == {"events": 1, "shown": 0, "not_shown": 1}
    assert unplug["only_asleep"] == {"events": 1, "shown": 1, "not_shown": 0}
    assert tr["unplug"]["poll"]["old_state_shown_after_transition"] == 1
    assert tr["plug_in"]["poll"]["categories"]["any_awake"] == {"events": 1, "shown": 1, "not_shown": 0}
    assert "no_rows: 1 events (0 shown, 1 not)" in report


def test_awake_and_asleep_flags_are_split(tmp_path: Path) -> None:
    b = Pairs()
    b.charging(1000, 4000)
    on = {"is_charging": True, "is_connected": True, "connect_state": None}
    for k in range(4):
        b.cloud_row(1100 + 600 * k, "poll", "ok", asleep=k % 2 == 0, charging=on)
    results, report = b.run(tmp_path)
    pc = results["plug_charging"]["poll:ok"]
    assert pc["awake"]["charging.is_charging x car_charging"]["rows"] == {"true": {"true": 2}}
    assert pc["asleep"]["charging.is_charging x car_charging"]["rows"] == {"true": {"true": 2}}
    assert "no_flag" not in pc
    assert "[awake]" in report and "[asleep]" in report


@pytest.mark.parametrize("jitter", [2.0, 30.0])
def test_car_row_jitter_keeps_a_zero_lag_verdict(tmp_path: Path, jitter: float) -> None:
    def soc(t: float) -> int:
        return 50 + max(0, int((t - 1017) // 420))

    b = Pairs()
    # The car records each step at once, but its rows arrive `jitter` seconds late.
    times = sorted({1000 + 60.0 * k for k in range(120)} | {1017 + 420.0 * k for k in range(1, 17)})
    for t in times:
        b.car_row(t + jitter, "charging", power_level="off", soc_pct=soc(t), charging_gun_raw=2,
                  charger_state_raw=1, charging_type_raw=2, charging_power_kw=5.5)
    t = 1300.0
    while t < 8000:
        b.cloud_row(t, "poll", "ok", soc=soc(t), asleep=False)
        t += 37
    results, _ = b.run(tmp_path)
    best = results["soc"]["poll:ok soc (asleep false: realtime awake)"]["best_fit_lag_while_changing"]
    assert best["best_lag_s"] == 0
    assert best["agreement_at_lag0"] >= 0.9


def test_clusters_and_drives_are_counted(tmp_path: Path) -> None:
    b = Pairs()
    b.drive(1000, 1600, lambda t: 50.0)
    b.drive(5000, 5600, lambda t: 50.0)  # a second drive more than 10 min later
    for t in (1100, 1130, 1160, 5100, 5130):
        b.cloud_row(t, "poll", "ok", speed_kph=50.0, asleep=False)
    results, report = b.run(tmp_path)
    best = results["speed"]["poll:ok"]["best_lag"]
    assert best["clusters"] == 2 and best["drives"] == 2
    diff = results["speed"]["poll:ok"]["diff_cloud_minus_car_speed_kph"]
    assert diff["clusters"] == 2 and diff["drives"] == 2
    gps = results["speed"]["car_speed_vs_gps_speed"]
    assert gps["driving_segments"] == 2
    assert gps["ratio_car_over_gps_by_gps_speed_band"]["20-60"]["median"] == 1.0
    assert "days / clusters / drives" in report


# --- fix round 3 ----------------------------------------------------------------------------------------


def test_across_gap_reversal_closes_the_window(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 2000)  # unplugged
    b.charging(2000, 2400)  # plug-in at 2000 (timed), rows until 2340
    b.parked(5000, 7000)  # after a silent gap the car is unplugged: an across-gap unplug
    b.cloud_row(5100, "poll", "ok", asleep=False,
                charging={"is_charging": True, "is_connected": True, "connect_state": None})
    results, report = b.run(tmp_path)
    plug_in = results["plug_charging"]["transitions"]["plug_in"]
    assert plug_in["car_transitions"] == 1
    assert plug_in["poll"]["shown"] == 0
    assert plug_in["poll"]["categories"]["no_rows"]["events"] == 1
    assert plug_in["poll"]["right_censored_no_row_before_window_closed"] == {"next_reversal_time_uncertain": 1}
    assert results["plug_charging"]["transitions"]["unplug"]["time_uncertain_across_a_gap_not_measured"] == 1


def test_power_window_closes_at_an_across_gap_change(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 2000)  # on
    b.parked(2000, 2600)  # power off at 2000 (timed)
    b.stationary(6000, 7000)  # on again after a silent gap: time-uncertain power-on
    b.cloud_row(6100, "poll", "ok", online=False, asleep=True)  # after the car is back on: not credited
    results, _ = b.run(tmp_path)
    res = results["online_latency"]["poll:ok"]["asleep_true_after_power_off"]
    assert res["delay_s"]["n"] == 0
    assert res["right_censored_no_row_before_window_closed"] == {"next_reversal_time_uncertain": 1}
    assert results["online_latency"]["time_uncertain_power_changes_across_a_gap"] == 1


def test_lag_fit_uses_a_common_cohort(tmp_path: Path) -> None:
    b = Pairs()
    b.charging(0, 3200, soc_pct=60)  # rows every 60 s up to 3180
    b.charging(3300, 4000, soc_pct=60, odometer_km=12346)  # a 120 s hole that cannot be inferred
    for t in (3000, 3100, 3200):
        b.cloud_row(t, "mqtt", "vehicleInfo", soc=60)
    b.cloud_row(3300, "mqtt", "vehicleInfo", soc=61)  # disagrees; has no car value at a 20 s lag
    results, _ = b.run(tmp_path)
    best = results["soc"]["mqtt:vehicleInfo soc"]["best_fit_lag_while_changing"]
    assert (best["n_samples"], best["cohort_n"], best["excluded_missing_car_value"]) == (4, 3, 1)
    assert best["best_lag_s"] == 0 and best["determined"] is False
    assert {c["matched"] for c in best["matched_by_lag"]} == {3}


def test_speed_lag_uses_a_common_cohort_and_unrounded_error(tmp_path: Path) -> None:
    def speed(t: float) -> float:
        return 60 + 40 * math.sin(2 * math.pi * t / 180)

    b = Pairs()
    b.drive(2000, 5600, speed)
    for k in range(40):
        t = 2400 + 23 * k
        b.cloud_row(t, "mqtt", "vehicleInfo", speed_kph=round(speed(t - 30), 1))
    b.cloud_row(2010, "mqtt", "vehicleInfo", speed_kph=10.0)  # no car speed 30+ s earlier: excluded
    results, _ = b.run(tmp_path)
    best = results["speed"]["mqtt:vehicleInfo"]["best_lag"]
    assert best["best_lag_s"] == 30 and best["cohort_n"] == 40 and best["excluded_missing_car_value"] == 1
    assert {c["n"] for c in best["curve"]} == {40}


def test_energy_skips_backward_steps_and_splits_endpoint_age(tmp_path: Path) -> None:
    b = Pairs()
    b.drive(1000, 1000 + 3600, lambda t: 60.0, odo0=12000)  # 60 km, ends at odometer 12059
    b.parked(4600, 20000)
    b.cloud_row(4900, "poll", "ok", last50km_energy_kwh=8.8)  # endpoint 5 min old
    b.cloud_row(12000, "poll", "ok", last50km_energy_kwh=8.8)  # same window, endpoint 2 h old
    b.drive(20000, 20000 + 1800, lambda t: 60.0, odo0=11990)  # backward step, 30 km only
    b.parked(21800, 23000)
    b.cloud_row(22000, "poll", "ok", last50km_energy_kwh=8.0)
    results, _ = b.run(tmp_path)
    e = results["energy"]["poll:ok"]
    assert results["energy"]["odometer_backward_steps_persistent"] == 1
    assert e["windows_crossing_a_backward_odometer_step"] == 1
    assert e["per_50km_window"]["all_endpoints"]["windows"] == 1
    assert e["per_50km_window"]["all_endpoints"]["per_window"][0]["polls"] == 2
    assert e["per_50km_window"]["endpoint_age_le_10_min"]["per_window"][0]["polls"] == 1


def test_freeze_and_stale_wording(tmp_path: Path) -> None:
    b = Pairs()
    b.charging(1000, 5000, soc_pct=lambda t: 50 + int((t - 1000) // 600))
    b.parked(5000, 9000)  # unplug at 5000
    frozen = {"is_charging": True, "is_connected": True, "connect_state": None, "soc": 50}
    for k in range(10):
        b.cloud_row(1100 + 300 * k, "poll", "ok", soc=50, asleep=True, charging=frozen)
    b.cloud_row(6800, "poll", "ok", soc=50, asleep=True, charging=frozen)  # 0.5 h after the unplug
    b.cloud_row(8600, "poll", "ok", soc=56, asleep=False, charging=frozen)  # 1 h after, awake
    results, report = b.run(tmp_path)
    fs = results["freeze_and_stale"]
    assert fs["soc_freeze_adjacent_asleep_polls_same_observed_plug_episode"]["rows"] == {"unchanged": {"pairs": 9}}
    assert fs["soc_freeze_filter_first_asleep_plugged_polls"]["rows"] == {"unchanged": {"pairs": 9}}
    stale = fs["connected_true_while_car_unplugged_hours_since_last_observed_unplug"]
    assert stale["asleep"]["n"] == 1 and stale["asleep"]["max"] == 0.5
    assert stale["awake"]["n"] == 1 and stale["awake"]["max"] == 1.0
    assert "continuity not established" in report


def test_push_overlap_counts_the_car_observation_window(tmp_path: Path) -> None:
    b = Pairs()
    b.cloud_row(500, "mqtt", "vehicleInfo", soc=80)  # same calendar day, before the first car row
    b.stationary(1000, 2000)
    b.cloud_row(1500, "mqtt", "vehicleInfo", soc=80)
    results, report = b.run(tmp_path)
    assert results["pushes"]["pushes_in_car_observation_window"]["mqtt:vehicleInfo"] == 1
    assert "Pushes inside the car observation window" in report


def test_energy_drops_out_of_order_odometer_readings(tmp_path: Path) -> None:
    b = Pairs()
    b.drive(1000, 1000 + 3600, lambda t: 60.0, odo0=12000)
    # A late or resent row 0.2 s after a newer one, 1 km lower; the next readings recover.
    b.car_row(2800.2, "moving", speed_kph=60.0, soc_pct=80, odometer_km=12028, power_level="on", gear="D",
              lifetime_kwh=8004.5, remaining_kwh=64.0)
    b.parked(4600, 8000)
    b.cloud_row(4900, "poll", "ok", last50km_energy_kwh=8.8)
    results, _ = b.run(tmp_path)
    assert results["energy"]["odometer_readings_out_of_order_dropped"] == 1
    assert results["energy"]["odometer_backward_steps_persistent"] == 0
    assert results["energy"]["poll:ok"]["per_50km_window"]["all_endpoints"]["windows"] == 1


# --- decode: raw BYD fields against the car ------------------------------------------------------------
# Raw lines must pass the exporter's line check, so the synthetic fields use real realtime paths with made-up
# meanings: lightStopStatus plays "any door open", trunkLidLower "the front-left door", powerGear the gear,
# gl 1000 x motor power, tirePressUnit a constant, epb and rate noise.

CLOSED = dict.fromkeys(("fl", "fr", "rl", "rr", "hood", "trunk"), "closed")
DOOR_ORDER = ("fl", "fr", "trunk", "rr") * 2  # one door per block, so "any door open" beats every single door


def _decode(b: Pairs) -> dict[str, Any]:
    """The decode section alone, without writing files (the full run is tested elsewhere)."""
    car = [{k: v for k, v in row.items() if k in compare.CAR_KEYS or k == "t"} for row in b.car]
    return compare.Analysis(b.meta(), car, [], Counter(), [dict(row) for row in b.raw]).decode()


def _hypotheses(de: dict[str, Any], field: str) -> dict[str, Any]:
    record = de["enum_fields"].get(field) or de["numeric_fields"][field]
    return record["hypotheses"]


def _decode_scenario(seed: int = 7) -> tuple[Pairs, list[str]]:
    """Eight blocks of irregular length, 15 min apart: on and parked (one door open a while), then a drive.

    Returns the builder and the rate noise values (which must never reach an output).
    """
    rng = random.Random(seed)
    b = Pairs()
    noise_values: list[str] = []

    def raw(t: float, doors: dict[str, str], gear: str, motor: float | None) -> None:
        value = round(rng.uniform(-900, 900), 3)
        noise_values.append(f"{value:.3f}")
        fields: dict[str, Any] = {
            "lightStopStatus": int("open" in doors.values()), "trunkLidLower": int(doors["fl"] == "open"),
            "powerGear": 3 if gear == "D" else 1, "tirePressUnit": 1, "epb": rng.randint(0, 4), "rate": value,
            "time_t": round(t - 0.5, 3),
        }
        if motor is not None:
            fields["gl"] = round(motor * 1000)
        b.raw_row(t, "realtime", fields)

    t0 = 1000.0
    for door in DOOR_ORDER:
        steps = rng.randint(30, 50)
        start = rng.randint(6, 12)
        stop = start + rng.randint(10, 18)
        for step in range(steps):
            t = t0 + step * 30
            doors = {**CLOSED, door: "open"} if start <= step < stop else dict(CLOSED)
            b.car_row(t, "stationary", power_level="on", gear="P", speed_kph=0.0, soc_pct=80, doors=doors,
                      charging_gun_raw=1, charger_state_raw=0, charging_type_raw=1)
            raw(t + 1, doors, "P", None)
        t0 += steps * 30
        for step in range(rng.randint(200, 400)):
            t = t0 + step * 2
            motor = round(rng.uniform(-30, 80), 1)
            b.car_row(t, "moving", power_level="on", gear="D", speed_kph=50.0, soc_pct=80, doors=dict(CLOSED),
                      motor_power_kw=motor, charging_gun_raw=1, charger_state_raw=0, charging_type_raw=1)
            if step % 2 == 0:
                raw(t + 0.5, CLOSED, "D", motor)
        t0 = t + 900
    # While the car is unknown (a silent gap), a stale reply, and a failure without a body.
    b.raw_row(t0 - 300, "realtime", {"tirePressUnit": 1})
    b.raw_row(t0 - 900 + 5, "realtime", {"tirePressUnit": 1, "lightStopStatus": 1, "time_t": t0 - 900 + 5 - 400})
    b.raw_row(t0 - 290, "energy", code="1008")
    return b, noise_values


def test_decode_finds_the_injected_relationships(tmp_path: Path) -> None:
    b, _ = _decode_scenario()
    results, report = b.run(tmp_path)
    de = results["decode"]
    with_fields = sum(1 for row in b.raw if "fields" in row)
    paired = with_fields - 2  # all but the line while the car is unknown and the stale one
    assert de["lines"] == {"raw_lines": with_fields + 1, "with_fields": with_fields, "paired_with_car": paired,
                           "car_unknown": 1, "inferred_parked": 0, "stale_sample": 1, "sample_time_invalid": 0,
                           "by_source_endpoint": {"poll:energy": 1, "poll:realtime": with_fields}}
    assert de["clusters"] == {"total": 8, "train": 4, "test": 4}
    assert de["codes_x_car_state"]["rows"] == {"energy:1008": {"unknown": 1}}
    # A field 1 exactly when a door is open: any door, not a single one.
    door = de["enum_fields"]["realtime:lightStopStatus"]
    assert door["best_reading"] == "doors_any_open"
    hyp = door["hypotheses"]["doors_any_open"]
    assert hyp["rows"] == {"0": {"closed": hyp["rows"]["0"]["closed"]}, "1": {"open": hyp["rows"]["1"]["open"]}}
    assert hyp["raw_values_supported"] == ["0", "1"] and hyp["clusters_by_raw_value"]["1"] == 8
    assert hyp["train_gate"] and hyp["repeated"] and hyp["test_clusters"] == 4
    assert hyp["tested"] and hyp["bh_pass"] and hyp["p_value"] < 0.01 and hyp["verdict"] == "predicts"
    assert hyp["held_out"]["balanced_accuracy"] == 1.0 and hyp["held_out"]["coverage"] == 1.0
    assert door["hypotheses"]["doors.fl"]["verdict"] != "predicts"
    # The gear code predicts the gear.
    gear = de["enum_fields"]["realtime:powerGear"]
    assert gear["best_reading"] == "gear" and gear["hypotheses"]["gear"]["verdict"] == "predicts"
    assert gear["hypotheses"]["gear"]["modal"] == {"1": {"car": "P", "share": 1.0}, "3": {"car": "D", "share": 1.0}}
    # One door's field seen open in two (train) blocks only: unsupported, never a finding.
    single = de["enum_fields"]["realtime:trunkLidLower"]
    fl = single["hypotheses"]["doors.fl"]
    assert fl["all_pairs"]["lambda"] == 1.0 and fl["verdict"] == "observation"  # perfect, but only twice
    assert fl["raw_values_supported"] == ["0"] and not fl["train_gate"] and not fl.get("tested")
    # 1000 x motor power.
    motor = de["numeric_fields"]["realtime:gl"]
    assert motor["best_reading"] == "motor_power_kw"
    fit = motor["hypotheses"]["motor_power_kw"]
    assert fit["slope"] == 1000.0 and fit["offset"] == 0.0 and fit["r_diff_train"] == 1.0
    assert fit["episodes"] == 8 and fit["clusters"] == 8 and fit["verdict"] == "predicts"
    assert fit["held_out"]["r"] == 1.0 and fit["held_out"]["mae"] == 0.0 and fit["held_out"]["pass"]
    # A constant field is listed with its one value and nowhere else.
    assert de["constant_fields"] == {"realtime:tirePressUnit": {"value": "1", "lines": paired + 2}}
    age = de["time_fields_age_s"]["realtime:time"]
    assert age["median"] == 0.5 and age["n"] == paired + 1
    # The noise is never flagged, and nothing else predicts.
    predicts = {(x["field"], x["reading"]) for x in de["ranking_enum"] + de["ranking_numeric"]
                if x["verdict"] == "predicts"}
    assert predicts == {("realtime:lightStopStatus", "doors_any_open"), ("realtime:powerGear", "gear"),
                        ("realtime:gl", "motor_power_kw")}
    assert de["tests"]["predicts"] == 3 and de["tests"]["hypotheses"] > 30 and de["tests"]["permutations"] == 2000
    for field in ("realtime:epb", "realtime:rate"):
        assert all(h["verdict"] == "no association" for h in _hypotheses(de, field).values()), field
    assert de["lags_secondary"]["realtime:lightStopStatus x doors_any_open"]["best_lag_s"] == 0
    assert "## 18. Decode: raw BYD fields against the car" in report
    assert "| realtime:lightStopStatus | doors_any_open | predicts |" in report
    assert f"| realtime:tirePressUnit | 1 | {paired + 2} |" in report


def test_decode_outputs_carry_no_raw_numbers_or_times(tmp_path: Path) -> None:
    b, noise = _decode_scenario()
    results, _ = b.run(tmp_path)
    outputs = [(tmp_path / "out" / name).read_text(encoding="utf-8") for name in ("results.json", "report.md")]
    for text in outputs:
        for value in noise:
            assert value not in text, value
        for row in b.raw:
            assert json.dumps(row, separators=(",", ":")) not in text
            if "fields" in row:
                assert json.dumps(row["fields"]) not in text
    assert _number_lists(results) == []
    assert "values" not in _keys(results)


def test_decode_is_deterministic() -> None:
    b, _ = _decode_scenario(3)
    assert _decode(b) == _decode(b)


def test_decode_with_no_raw_lines_says_so(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 1300)
    results, report = b.run(tmp_path)
    de = results["decode"]
    assert de["lines"]["raw_lines"] == 0 and de["ranking_enum"] == [] and de["constant_fields"] == {}
    assert de["tests"]["hypotheses"] == 0 and "## 18. Decode" in report


# 28 independent null fields: 14 slow enums and 14 numeric random walks, on real table paths.
NULL_ENUMS = ("okLight", "epb", "ins", "pwr", "eps", "esp", "svs", "srs", "ect", "lightStopStatus", "trunkLidLower",
              "sentryStatus", "airRunState", "flashChargeState")
NULL_WALKS = {"gl": (-400_000, 400_000, 5000), "totalPower": (-900, 900, 20), "rate": (-900, 900, 20),
              "elecPercent": (5, 95, 0.5), "enduranceMileage": (10, 1900, 5), "tempInCar": (-30, 70, 0.5),
              "mainSettingTempNew": (16, 30, 0.2), "oilPercent": (5, 95, 0.5), "totalOil": (10, 90_000, 50),
              "ectValue": (-30, 140, 1), "leftFrontTirepressure": (1, 400, 2), "rightFrontTirepressure": (1, 400, 2),
              "leftRearTirepressure": (1, 400, 2), "rightRearTirepressure": (1, 400, 2)}


def null_export(seed: int, blocks: int = 8) -> Pairs:
    """A car that parks, opens doors and drives, in blocks 15 min apart, and 28 raw fields unrelated to it."""
    rng = random.Random(seed)
    b = Pairs()
    enums = dict.fromkeys(NULL_ENUMS, 0)
    enum_until = dict.fromkeys(NULL_ENUMS, 0.0)
    walks = {key: rng.uniform(low, high) for key, (low, high, _) in NULL_WALKS.items()}
    t, odometer, door_open, door_until = 1000.0, 12000.0, False, 0.0
    speed, motor = 40.0, 10.0

    def raw(at: float) -> None:
        for key in NULL_ENUMS:
            if at >= enum_until[key]:
                enums[key], enum_until[key] = rng.randint(0, 2), at + rng.uniform(300, 2400)
        for key, (low, high, step) in NULL_WALKS.items():
            walks[key] = min(high, max(low, walks[key] + rng.gauss(0, step)))
        b.raw_row(at, "realtime", {**enums, **{key: round(value, 2) for key, value in walks.items()}})

    for _ in range(blocks):
        for _ in range(rng.randint(15, 30)):  # parked, on, a door opening and closing slowly
            if t >= door_until:
                door_open, door_until = not door_open, t + rng.uniform(120, 900)
            doors = {**CLOSED, "rr": "open"} if door_open else dict(CLOSED)
            b.car_row(t, "stationary", power_level="on", gear="P", speed_kph=0.0, soc_pct=70, doors=doors,
                      odometer_km=math.floor(odometer))
            raw(t + 1)
            t += 30
        for step in range(rng.randint(60, 150)):  # a drive
            speed = min(130.0, max(5.0, speed + rng.gauss(0, 3)))
            motor = min(150.0, max(-40.0, motor + rng.gauss(0, 5)))
            odometer += speed / 3600 * 2
            b.car_row(t, "moving", power_level="on", gear="D", speed_kph=round(speed, 1), soc_pct=70,
                      doors=dict(CLOSED), motor_power_kw=round(motor, 1), odometer_km=math.floor(odometer))
            if step % 3 == 0:
                raw(t + 0.5)
            t += 2
        t += 900
    return b


def test_unrelated_fields_are_never_said_to_predict() -> None:
    """A small version of the null simulation (the 200-export run is a script; see the hand-over)."""
    false_predicts = hypotheses = 0
    for seed in range(6):
        de = _decode(null_export(seed))
        false_predicts += de["tests"]["predicts"]
        hypotheses += de["tests"]["hypotheses"]
    assert false_predicts == 0 and hypotheses > 6 * 28


def test_a_mapping_that_flips_between_halves_does_not_predict() -> None:
    """Train clusters: 1 = door open; test clusters: 1 = door closed. The test half alone is perfect too."""
    b = Pairs()
    t = 1000.0
    for cluster in range(8):
        flip = cluster % 2 == 1
        for step in range(40):
            door_open = 10 <= step < 25
            doors = {**CLOSED, "fl": "open"} if door_open else dict(CLOSED)
            b.car_row(t, "stationary", power_level="on", gear="P", speed_kph=0.0, doors=doors)
            b.raw_row(t + 1, "realtime", {"lightStopStatus": int(door_open != flip)})
            t += 30
        t += 900
    de = _decode(b)
    hyp = de["enum_fields"]["realtime:lightStopStatus"]["hypotheses"]["doors_any_open"]
    assert hyp["train_gate"] and hyp["tested"]
    assert hyp["held_out"]["balanced_accuracy"] == 0.0 and not hyp["held_out"]["pass"]
    assert hyp["verdict"] == "observation" and de["tests"]["predicts"] == 0


def test_a_rising_counter_against_the_odometer_does_not_predict() -> None:
    rng = random.Random(11)
    b = Pairs()
    t, odometer, counter = 1000.0, 12000.0, 0
    for _ in range(8):
        for step in range(300):
            odometer += rng.uniform(5, 30) / 3600 * 2
            b.car_row(t, "moving", power_level="on", gear="D", speed_kph=50.0, doors=dict(CLOSED),
                      odometer_km=math.floor(odometer))
            if step % 5 == 0:
                counter += rng.choice((1, 2))
                b.raw_row(t + 0.5, "realtime", {"totalMileage": counter})
            t += 2
        t += 900
    de = _decode(b)
    fit = de["numeric_fields"]["realtime:totalMileage"]["hypotheses"]["odometer_km"]
    assert fit["r"] > 0.9  # the levels rise together...
    assert abs(fit["r_diff_train"]) < 0.8 and not fit["train_gate"]  # ...their steps do not
    assert fit["verdict"] != "predicts" and de["tests"]["predicts"] == 0


def test_one_use_codes_against_an_alternating_door_do_not_predict() -> None:
    b = Pairs()
    t = 1000.0
    for episode in range(16):
        doors = {**CLOSED, "fl": "open"} if episode % 2 else dict(CLOSED)
        for _ in range(8):
            b.car_row(t, "stationary", power_level="on", gear="P", speed_kph=0.0, doors=doors)
            b.raw_row(t + 1, "realtime", {"lightStopStatus": episode})  # a new code each episode
            t += 60
        t += 600
    de = _decode(b)
    hyp = de["enum_fields"]["realtime:lightStopStatus"]["hypotheses"]["doors_any_open"]
    assert hyp["all_pairs"]["lambda"] == 1.0  # a perfect but meaningless mapping
    assert hyp["raw_values_supported"] == [] and not hyp["train_gate"] and not hyp.get("tested")
    assert hyp["verdict"] == "observation" and de["tests"]["predicts"] == 0


def test_a_counter_within_one_short_sequence_does_not_predict() -> None:
    b = Pairs()
    for step in range(10):
        b.car_row(1000 + step * 2, "moving", power_level="on", gear="D", speed_kph=10.0 + step * 2,
                  doors=dict(CLOSED), motor_power_kw=20.0 + step)
        b.raw_row(1000 + step * 2 + 0.5, "realtime", {"rate": step, "okLight": step % 9})
    de = _decode(b)
    fit = de["numeric_fields"]["realtime:rate"]["hypotheses"]["motor_power_kw"]
    assert fit["r_diff_train"] is None and fit["clusters"] == 1 and not fit["repeated"]
    assert de["tests"]["predicts"] == 0 and de["tests"]["tested_on_test_half"] == 0


def test_a_periodic_field_in_step_with_a_periodic_door_is_not_significant() -> None:
    """Perfect on both halves and held out, but every second rotation aligns it as well."""
    b = Pairs()
    t = 1000.0
    for cluster in range(12):
        doors = {**CLOSED, "fl": "open"} if cluster % 4 >= 2 else dict(CLOSED)
        for _ in range(4):
            b.car_row(t, "stationary", power_level="on", gear="P", speed_kph=0.0, doors=doors)
            b.raw_row(t + 1, "realtime", {"lightStopStatus": int(cluster % 4 >= 2)})
            t += 60
        t += 900
    de = _decode(b)
    hyp = de["enum_fields"]["realtime:lightStopStatus"]["hypotheses"]["doors_any_open"]
    assert hyp["tested"] and hyp["held_out"]["pass"]
    assert hyp["p_value"] > 0.05 and not hyp["bh_pass"] and hyp["verdict"] == "observation"


def test_an_r_just_under_the_threshold_is_not_passed() -> None:
    rng = random.Random(3)
    drives = [[(round(rng.uniform(-30, 80), 1), rng.gauss(0, 1)) for _ in range(40)] for _ in range(8)]

    def values(k: float) -> list[list[tuple[float, float]]]:
        return [[(x, round(1000 * x + k * z, 3)) for x, z in drive] for drive in drives]

    def r_train(k: float) -> float:
        dx: list[float] = []
        dy: list[float] = []
        for drive in values(k)[::2]:  # the even clusters are the train half
            dx += [b[0] - a[0] for a, b in zip(drive, drive[1:])]
            dy += [b[1] - a[1] for a, b in zip(drive, drive[1:])]
        return compare._pearson(dx, dy)

    low, high = 0.0, 1e6  # r falls as the noise grows: find r in (0.7995, 0.8)
    for _ in range(200):
        mid = (low + high) / 2
        low, high = (mid, high) if r_train(mid) >= 0.7997 else (low, mid)
    k = low
    while r_train(k) >= 0.8:
        k += 1.0
    assert 0.7995 < r_train(k) < 0.8 and compare.r3(r_train(k)) == 0.8  # shows as 0.8, is under it
    b = Pairs()
    t = 1000.0
    for drive in values(k):
        for x, y in drive:
            b.car_row(t, "moving", power_level="on", gear="D", speed_kph=50.0, doors=dict(CLOSED), motor_power_kw=x)
            b.raw_row(t + 0.5, "realtime", {"gl": y})
            t += 2
        t += 1200
    fit = _decode(b)["numeric_fields"]["realtime:gl"]["hypotheses"]["motor_power_kw"]
    assert fit["r_diff_train"] == 0.8 and not fit["train_gate"] and fit["verdict"] != "predicts"


def test_stale_gps_sentinels_and_inferred_pairs_are_kept_out() -> None:
    b = Pairs()
    _parked_gap(b)
    for step in range(6):
        b.raw_row(3000 + step * 600, "realtime", {"okLight": step % 2})  # inside the inferred parked gap
    b.raw_row(1650, "realtime", {"okLight": 1, "time_t": 1650 - 301})  # 301 s old: stale
    b.raw_row(1600, "realtime", {"okLight": 0, "time_t": 1600 - 299})  # 299 s old: paired at its sample time
    # The car is known at the receive time, but BYD's fix time was 0: never paired at the receive time.
    b.raw_row(1610, "gps", {"data.speed": 0, "data.gpsTimeStamp_stale": True})
    b.raw_row(1620, "gps", {"data.speed": 0})  # no sample-time path at all: paired at its receive time
    de = _decode(b)
    assert de["lines"]["inferred_parked"] == 6 and de["lines"]["stale_sample"] == 1
    assert de["lines"]["sample_time_invalid"] == 1 and de["lines"]["paired_with_car"] == 2
    assert de["enum_fields"]["realtime:okLight"]["inferred_parked_pairs"] == 6


def test_doors_any_open_is_closed_only_when_every_door_is_read() -> None:
    b = Pairs()
    b.car_row(1000, "stationary", power_level="on", doors={"fl": "closed"})
    b.car_row(1060, "stationary", power_level="on", doors=dict(CLOSED))
    b.car_row(1120, "stationary", power_level="on", doors={"fr": "open"})
    analysis = compare.Analysis(b.meta(), [dict(row) for row in b.car], [], Counter(), [])
    readings = [analysis._enum_readings(t, analysis.car.snapshot(t)) for t in (1001, 1061, 1121)]
    assert "doors_any_open" not in readings[0] and readings[0]["doors.fl"] == "closed"
    assert readings[1]["doors_any_open"] == "closed" and readings[2]["doors_any_open"] == "open"


def test_a_sentinel_row_does_not_remove_a_field_from_the_fit() -> None:
    b = Pairs()
    t = 1000.0
    for cluster in range(6):
        for step in range(20):
            motor = 5.0 + step + cluster
            b.car_row(t, "moving", power_level="on", gear="D", speed_kph=50.0, doors=dict(CLOSED), motor_power_kw=motor)
            value: Any = round(motor * 2, 1)
            if step == 7:
                value = "--"  # BYD's "no reading": excluded from the fit, row by row
            elif step == 8:
                value = -1  # the path's -1 sentinel
            b.raw_row(t + 0.5, "realtime", {"nearestEnergyConsumption": value})
            t += 2
        t += 900
    fit = _decode(b)["numeric_fields"]["realtime:nearestEnergyConsumption"]["hypotheses"]["motor_power_kw"]
    assert fit["n"] == 6 * 18 and fit["slope"] == 2.0 and fit["r_diff_train"] == 1.0


def test_lags_are_compared_on_one_cohort() -> None:
    b = Pairs()
    rng = random.Random(5)
    t, state, history = 1000.0, "closed", []
    for _ in range(600):
        if rng.random() < 0.08:
            state = "open" if state == "closed" else "closed"
        doors = {**CLOSED, "fl": "open"} if state == "open" else dict(CLOSED)
        b.car_row(t, "stationary", power_level="on", gear="P", speed_kph=0.0, doors=doors)
        history.append((t, state))
        late = next((s for tt, s in reversed(history) if tt <= t + 1 - 60), "closed")
        b.raw_row(t + 1, "realtime", {"lightStopStatus": int(late == "open")})  # BYD 60 s behind
        t += 30 if len(history) % 40 else 900
    lag = _decode(b)["lags_secondary"]["realtime:lightStopStatus x doors_any_open"]
    assert lag["best_lag_s"] == 60 and lag["by_lag_s"]["60"] > lag["by_lag_s"]["0"]
    # The first rows after each gap have no car reading 120 s back: they are out of the cohort at every lag.
    assert lag["cohort_n"] < 600 - 10
    # Too few pairs at every lag: not comparable.
    b = Pairs()
    for step in range(12):
        doors = {**CLOSED, "fl": "open"} if step % 4 < 2 else dict(CLOSED)
        b.car_row(1000 + step * 30, "stationary", power_level="on", gear="P", doors=doors)
        b.raw_row(1001 + step * 30, "realtime", {"lightStopStatus": int(step % 4 < 2)})
    lag = _decode(b)["lags_secondary"]["realtime:lightStopStatus x doors_any_open"]
    assert lag["note"] == "lag not comparable" and lag["cohort_n"] < compare.DECODE_MIN_N


def test_tied_sample_times_with_null_values_do_not_crash() -> None:
    b = Pairs()
    t = 1000.0
    for cluster in range(6):
        for step in range(12):
            b.car_row(t, "moving", power_level="on", gear="D", speed_kph=50.0, doors=dict(CLOSED),
                      motor_power_kw=10.0 + step)
            for value in (None, 10.0 + step, None):  # three replies with the same sample time
                b.raw_row(t + 0.5, "realtime", {"gl": value, "okLight": None if value is None else step % 2,
                                                "time_t": t})
            t += 2
        t += 900
    de = _decode(b)
    assert de["lines"]["paired_with_car"] == 6 * 12 * 3
    assert "realtime:gl" in de["numeric_fields"]


def test_invalid_raw_lines_are_counted_and_never_used(tmp_path: Path) -> None:
    b = Pairs()
    b.stationary(1000, 1300)
    source = tmp_path / "pairs.jsonl"
    bad = ('{"type":"raw","t":1100,"source":"poll","endpoint":"realtime","fields":{"latitude":52.123456}}\n'
           '{"type":"raw","t":1100,"source":"mqtt","endpoint":"gps","fields":{"speed":1}}\n'
           '{"type":"raw","t":1100,"source":"poll","endpoint":"realtime","fields":{"gl":1e999999999999999999}}\n')
    source.write_text("".join(b.lines()) + bad, encoding="ascii")
    results = compare.compare(source, tmp_path / "out")
    assert results["inventory"]["skipped_lines"] == {"raw_invalid": 3}
    assert results["decode"]["lines"]["raw_lines"] == 0
    for name in ("results.json", "report.md"):
        assert "52.123456" not in (tmp_path / "out" / name).read_text(encoding="utf-8")


def test_decode_fits_tyres_against_every_slot_and_keeps_mixed_values_apart() -> None:
    b = Pairs()
    tyres = {"fl": 240, "fr": 236, "rl": 270, "rr": 274}
    for step in range(40):
        t = 1000 + step * 60
        kpa = {w: v + (step % 7 if w == "rl" else step % 5) for w, v in tyres.items()}
        b.car_row(t, "stationary", power_level="on", gear="P", speed_kph=0.0, tyre_kpa=kpa)
        # BYD's rear-left in bar against the car's rl slot; a unit that is sometimes null.
        b.raw_row(t + 1, "realtime", {"leftRearTirepressure": round(kpa["rl"] / 100, 2),
                                      "enduranceMileageV2Unit": ["km", None, "--"][step % 3]})
    de = _decode(b)
    tyre = de["numeric_fields"]["realtime:leftRearTirepressure"]
    assert tyre["best_reading"] == "tyre_kpa[rl]"
    assert tyre["hypotheses"]["tyre_kpa[rl]"]["slope"] == 0.01 and tyre["hypotheses"]["tyre_kpa[rl]"]["r"] == 1.0
    assert abs(tyre["hypotheses"]["tyre_kpa[fl]"]["r"]) < 0.8
    mixed = de["enum_fields"]["realtime:enduranceMileageV2Unit"]
    assert mixed["value_counts"] == {"--": 13, "km": 14, "null": 13}
    assert "realtime:enduranceMileageV2Unit" not in de["numeric_fields"]
