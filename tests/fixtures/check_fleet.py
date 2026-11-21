#!/usr/bin/env python3
"""Network-free service check. Its event log observes real plugin execution."""
import fcntl
import pathlib
import sys
import time

log, gate, host, service = sys.argv[1:]


def record(event):
    with open(log, "a", encoding="utf-8") as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        print(event, host, service, time.monotonic(), file=stream, flush=True)


record("start")
while not pathlib.Path(gate).exists():
    time.sleep(0.01)
time.sleep(0.05)
print("FLEET:" + "x" * 2048)
record("finish")
