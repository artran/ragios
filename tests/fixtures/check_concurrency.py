#!/usr/bin/env python3
"""Simulate a shared endpoint that reports concurrent requests in its response."""
import fcntl
import json
import sys
import time

state_path, check_id, allowed, hold_seconds = sys.argv[1:]


def update(starting):
    with open(state_path, "a+") as state_file:
        fcntl.flock(state_file, fcntl.LOCK_EX)
        state_file.seek(0)
        state = json.loads(state_file.read() or "{}")
        active = state.setdefault("active", {})
        if starting:
            state["overlap"] = state.get("overlap", False) or active.get(check_id, 0) > 0
            active[check_id] = active.get(check_id, 0) + 1
            state["peak"] = max(state.get("peak", 0), sum(active.values()))
            state["exceeded"] = state.get("exceeded", False) or state["peak"] > int(allowed)
        else:
            active[check_id] -= 1
            state["completed"] = state.get("completed", 0) + 1
        state_file.seek(0)
        state_file.truncate()
        json.dump(state, state_file)
        return state


update(True)
time.sleep(float(hold_seconds))
result = update(False)
print("completed={completed} peak={peak} overlap={overlap} exceeded={exceeded}".format(**result))
sys.exit(2 if result["overlap"] or result["exceeded"] else 0)
