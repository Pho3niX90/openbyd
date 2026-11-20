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
import subprocess
import sys
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
        self.first_seen_t = first_seen_t

    def car_row(self, t: float, cadence: str, **fields: Any) -> None:
        self.car.append({"type": "car", "t": round(t, 3), "cadence": cadence, **fields})

    def cloud_row(self, t: float, source: str, kind: str, **fields: Any) -> None:
        self.cloud.append({"type": "cloud", "t": round(t, 3), "source": source, "kind": kind, **fields})

    def meta(self) -> dict[str, Any]:
        rows = {key: 0 for key in ep.ROW_KINDS}
        for row in self.cloud:
            bucket = f"{row['source']}_{row['kind']}"
            rows[bucket if bucket in rows else f"{row['source']}_other"] += 1
        extra = {} if self.first_seen_t is None else {"first_seen_t": round(self.first_seen_t, 3)}
        return {"type": "meta", "version": 1, "start": "2026-03-01T00:00:00Z", "car_days": 29, "cloud_days": 13,
                "vin": pairs.SYNTHETIC_VIN, "model": "seal", "energy": "ev", "device_source": "car_app", **extra,
                "stats": {"car_rows": len(self.car), "cloud_rows": rows, "dropped_car_keys": {},
                          "dropped_cloud_keys": {}, "invalid_values": {}, "dropped_details": 0,
                          "unparsable_last_updated": 0, "cloud_fixes_matched": 0, "cloud_fixes_unmatched": 0}}

    def lines(self) -> list[str]:
        rows = [self.meta()] + sorted(self.car, key=lambda r: r["t"]) + sorted(self.cloud, key=lambda r: r["t"])
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
    assert unplug["poll"]["delay_s"]["values"] == [600]
    assert unplug["poll"]["old_state_shown_after_transition"] == 1
    assert unplug["push"]["delay_s"]["values"] == [20]
    assert unplug["push"]["old_state_shown_after_transition"] == 0
    stop = results["plug_charging"]["transitions"]["charge_stop"]
    assert stop["poll"]["delay_s"]["values"] == [600]
    assert stop["push"]["delay_s"]["values"] == [20]
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
    assert s["agreement_by_rounding_rule"]["rows"] == {"exact": {"differ": 5}, "floor": {"agree": 5},
                                                       "round_half_up": {"differ": 5}}
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
    assert tr["charge_start"]["push_vehicleInfo_power"]["delay_s"]["values"] == [40]
    assert tr["charge_stop"]["push_vehicleInfo_power"]["delay_s"]["values"] == [30]
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
    assert pu["first_cloud_row_after_car_event"]["power_on"]["mqtt:vehicleInfo"]["delay_s"]["values"] == [25]
    assert pu["first_cloud_row_after_car_event"]["power_off"]["mqtt:vehicleInfo"]["delay_s"]["values"] == [500]
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
    assert res["asleep_true_after_power_off"]["delay_s"]["values"] == [700]
    assert res["online_false_after_power_off"]["delay_s"]["values"] == [700]
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
