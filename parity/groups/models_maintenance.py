"""pybyd/models/maintenance.py"""

from __future__ import annotations

import copy

from pybyd.models.maintenance import MaintenanceHistory, MaintenanceRecord

from parity.framework import Group
from parity.models_support import TIMESTAMP_VALUES, json_safe, register_model, timestamp_safe

group = Group("models_maintenance")

RECORD_SAMPLES = [
    {
        "vin": "TESTVIN0000000001",
        "ro_no": "RO-NEW",
        "total_mileage": 99678,
        "last_balance_date": 1791771512000,
        "shop_name": "BYD Test",
        "dealer_code": "D001",
        "repair_type_code": "JSXM",
    },
    {
        "vin": "TESTVIN0000000001",
        "order_no": "RO-OLD",
        "mileage": "50210",
        "service_date": 1760000000000,
        "shopName": "BYD Workshop",
        "dealerCode": "D002",
        "repairTypeCode": "BY",
    },
    {"ro_no": "RO-EMPTY", "total_mileage": "--", "last_balance_date": 0},
    {},
]

record_cases = register_model(group, "models.maintenance.MaintenanceRecord", MaintenanceRecord, samples=RECORD_SAMPLES)

HISTORY_SAMPLES = [
    {
        "vin": "TESTVIN0000000001",
        "records": [
            {
                "vin": "TESTVIN0000000001",
                "ro_no": "RO-OLD",
                "total_mileage": 50210,
                "last_balance_date": 1760000000000,
                "shop_name": "BYD Old",
            },
            {
                "vin": "TESTVIN0000000001",
                "ro_no": "RO-NEW",
                "total_mileage": 99678,
                "last_balance_date": 1791771512000,
                "shop_name": "BYD New",
            },
        ],
    },
    {"vin": "TESTVIN0000000001", "records": []},
    {
        "vin": "TESTVIN0000000001",
        "records": [
            {"ro_no": "RO-NONE"},
            {"ro_no": "RO-DATED", "last_balance_date": "2024-05-07T10:00:00+02:00"},
            {"ro_no": "RO-TIE", "last_balance_date": "2024-05-07T08:00:00Z"},
            {"ro_no": "RO-LATER", "last_balance_date": "2024-05-07T08:00:00.000001Z"},
        ],
    },
    {
        "vin": "TESTVIN0000000001",
        "records": [{"ro_no": "RO-1"}, {"ro_no": "RO-2"}],
    },
]

history_cases = register_model(
    group, "models.maintenance.MaintenanceHistory", MaintenanceHistory, samples=HISTORY_SAMPLES
)


@group.recorded
def recorded():
    for stamp in [stamp for stamp in TIMESTAMP_VALUES if timestamp_safe(stamp)][:14]:
        yield "models.maintenance.MaintenanceRecord", {
            "raw": {**RECORD_SAMPLES[0], "last_balance_date": stamp},
            "context": None,
        }
    # Tied dates keep the first maximum; an offset pushing below 0001-01-01T00:00Z
    # still compares lower than datetime.min UTC (None).
    yield "models.maintenance.MaintenanceHistory", {
        "raw": {
            "vin": "TESTVIN0000000001",
            "records": [
                {"ro_no": "RO-BEFORE-MIN", "last_balance_date": "0001-01-01T00:00:00+05:00"},
                {"ro_no": "RO-NONE"},
            ],
        },
        "context": None,
    }
    yield "models.maintenance.MaintenanceHistory", {
        "raw": {
            "vin": "TESTVIN0000000001",
            "records": [
                {"ro_no": "RO-A", "last_balance_date": "2024-05-07T10:00:00+02:00"},
                {"ro_no": "RO-B", "last_balance_date": "2024-05-07T08:00:00Z"},
            ],
        },
        "context": None,
    }


@group.fuzz
def fuzz(rng):
    while True:
        payload = history_cases.fuzz(rng)
        if isinstance(payload, dict) and rng.random() < 0.4:
            payload["records"] = [copy.deepcopy(record_cases.fuzz(rng)) for _ in range(rng.randint(0, 4))]
        if json_safe(payload):
            yield "models.maintenance.MaintenanceHistory", {"raw": payload, "context": None}
