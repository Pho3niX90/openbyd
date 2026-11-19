"""pybyd/models/push_notification.py"""

from __future__ import annotations

from pybyd.models.push_notification import PushNotificationState, PushSwitch

from parity.framework import Group
from parity.models_support import register_model

group = Group("models_push_notification")

register_model(
    group,
    "models.push_notification.PushSwitch",
    PushSwitch,
    samples=[
        {"type": 701, "state": 1},
        {"type": "1", "state": "0"},
        {"type": 5, "state": "--"},
    ],
)

register_model(
    group,
    "models.push_notification.PushNotificationState",
    PushNotificationState,
    samples=[
        {"vin": "TESTVIN0000000001", "switches": [{"state": 1, "type": 1}, {"state": 1, "type": 701}]},
        {"vin": "TESTVIN0000000001", "switches": [{"state": 0, "type": 701}, {"type": 2}]},
        {"switches": [{"state": "1", "type": "701"}], "vin": "--"},
        {"vin": "TESTVIN0000000001", "switches": []},
    ],
)
