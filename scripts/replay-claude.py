#!/usr/bin/env python3
"""Replay text/reasoning from a real Zeron Claude journal at a fixed cadence.

Set CLAUDE_CODE_EXECUTABLE to this file and ZERON_REPLAY_JOURNAL to the JSONL
captured by resource-profile.mjs. This is a deterministic adapter replay, not
a live API run. Tool workloads must use the real harness or integration tests.
ZERON_REPLAY_DELAY_MS defaults to 40; ZERON_REPLAY_REPEAT defaults to 1.
No network or API calls are made. Repetition is a synthetic stress workload.
"""
import json
import os
import sys
import time

if not any(flag in sys.argv for flag in ("--print", "-p")):
    print("2.1.228 (resource profile replay)")
    sys.exit(0)

with open(os.environ["ZERON_REPLAY_JOURNAL"]) as source:
    events = [json.loads(line)["event"] for line in source if line.strip()]
if not any(e["type"] == "done" and e["status"] == "completed" for e in events):
    raise ValueError("Replay requires a successfully completed journal")
if any(e["type"] in ("toolCall", "toolResult", "subagent", "error") for e in events):
    raise ValueError("Only text/reasoning journals are supported")
def emit(frame):
    print(json.dumps(frame), flush=True)


emit({"type": "system", "subtype": "init", "model": "haiku", "tools": [],
      "cwd": os.getcwd(), "session_id": "resource-profile-replay"})
delay = float(os.environ.get("ZERON_REPLAY_DELAY_MS", "40")) / 1000
repeats = int(os.environ.get("ZERON_REPLAY_REPEAT", "1"))
if repeats < 1:
    raise ValueError("ZERON_REPLAY_REPEAT must be positive")
# The adapter keeps one CLI alive across turns. Replay each prompt so native
# composer tests can exercise sends into an already populated transcript.
for request in sys.stdin:
    if not request.strip():
        continue
    for _ in range(repeats):
        for event in events:
            kind = event["type"]
            if kind not in ("textDelta", "reasoningDelta"):
                continue
            text = event["text"]
            delta = {"type": "text_delta", "text": text} if kind == "textDelta" else {
                "type": "thinking_delta", "thinking": text}
            emit({"type": "stream_event", "parent_tool_use_id": None,
                  "event": {"type": "content_block_delta", "delta": delta}})
            time.sleep(delay)
    emit({"type": "result", "subtype": "success", "is_error": False,
          "session_id": "resource-profile-replay", "result": ""})
