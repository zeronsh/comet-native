#!/bin/sh
# Fake Codex app-server for comet-harness tests.
#
# Speaks scripted JSON-RPC 2.0 over stdio: initialize handshake, thread
# start/resume, then a scenario picked from the turn/start prompt text. Driven
# by crates/harness/tests/codex.rs.

emit() { printf '%s\n' "$1"; }
rid() { printf '%s' "$1" | sed 's/.*"id":\([0-9]*\).*/\1/'; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

fail_turn() { # $1 = request id, $2 = message
  emit "{\"id\":$1,\"result\":{\"turn\":{\"id\":\"t-bad\"}}}"
  emit "{\"method\":\"turn/failed\",\"params\":{\"turn\":{\"id\":\"t-bad\",\"error\":{\"message\":\"$2\"}}}}"
}

# ---- handshake -------------------------------------------------------------
read -r line || exit 1 # initialize
has "$line" '"method":"initialize"' || exit 1
has "$line" '"experimentalApi":true' || exit 1
has "$line" '"name":"comet-native"' || exit 1
emit "{\"id\":$(rid "$line"),\"result\":{\"userAgent\":\"fake-codex\"}}"

read -r line || exit 1 # initialized notification (no reply)
has "$line" '"method":"initialized"' || exit 1

# ---- thread start / resume -------------------------------------------------
read -r line || exit 1
thread_line="$line"
if has "$line" '"method":"thread/resume"'; then
  if has "$line" '"threadId":"resume-fail"'; then
    # Missing/foreign rollout: reject, expect the fresh-start fallback.
    emit "{\"id\":$(rid "$line"),\"error\":{\"code\":-32600,\"message\":\"rollout not found\"}}"
    read -r line || exit 1
    has "$line" '"method":"thread/start"' || exit 1
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-fresh\"}}}"
  else
    emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-resumed\"}}}"
  fi
elif has "$line" '"method":"thread/start"'; then
  emit "{\"id\":$(rid "$line"),\"result\":{\"thread\":{\"id\":\"th-1\"}}}"
else
  exit 1
fi

# ---- first turn ------------------------------------------------------------
read -r turnline || exit 1
tid=$(rid "$turnline")

case "$turnline" in

*scenario:happy*)
  # Verify the turn/start + thread/start params the harness must send.
  for want in '"method":"turn/start"' '"effort":"ultra"' '"model":"gpt-5.6-sol"' \
    '"networkAccess":true' '"type":"workspaceWrite"' \
    '"approvalPolicy":"never"' '"summary":"auto"' \
    '"serviceTier":"fast"'; do
    has "$turnline" "$want" || { fail_turn "$tid" "turn param missing: $want"; exit 0; }
  done
  for want in '"approvalPolicy":"never"' '"sandbox":"workspace-write"' '"cwd":"/tmp"' \
    '"serviceTier":"fast"'; do
    has "$thread_line" "$want" || { fail_turn "$tid" "thread param missing: $want"; exit 0; }
  done
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  # Deltas — both field spellings must be accepted.
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"Hello"}}'
  emit '{"method":"item/reasoning/textDelta","params":{"itemId":"r1","textDelta":"thinking hard"}}'
  emit '{"method":"item/reasoning/summaryTextDelta","params":{"itemId":"r1","delta":"summary"}}'
  # Item lifecycles.
  emit '{"method":"item/started","params":{"item":{"id":"c1","type":"commandExecution","command":"ls -la"}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"c1","type":"commandExecution","command":"ls -la","status":"completed","exitCode":1}}}'
  emit '{"method":"item/started","params":{"item":{"id":"f1","type":"fileChange","changes":[{"path":"/tmp/new.rs","kind":"add"}]}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"f1","type":"fileChange","status":"completed","changes":[{"path":"/tmp/new.rs","kind":"add"}]}}}'
  emit '{"method":"item/started","params":{"item":{"id":"mcp1","type":"mcpToolCall","server":"linear","tool":"search","arguments":{"q":"bug"}}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"mcp1","type":"mcpToolCall","server":"linear","tool":"search","status":"failed"}}}'
  emit '{"method":"item/started","params":{"item":{"id":"w1","type":"webSearch","query":"rust"}}}'
  emit '{"method":"item/completed","params":{"item":{"id":"w1","type":"webSearch","query":"rust"}}}'
  # Completion-only lifecycle: must still open AND close the tool call.
  emit '{"method":"item/completed","params":{"item":{"id":"td1","type":"todoList","items":[{"text":"a","completed":true},{"text":"b","completed":false}]}}}'
  # Streamed agentMessage: completed text must NOT re-emit.
  emit '{"method":"item/completed","params":{"item":{"id":"m1","type":"agentMessage","text":"Hello world"}}}'
  # Never-streamed agentMessage: completed text is the fallback delta.
  emit '{"method":"item/completed","params":{"item":{"id":"m2","type":"agentMessage","text":"unstreamed tail"}}}'
  # Unknown notification methods must be tolerated.
  emit '{"method":"some/unknownNotification","params":{"x":1}}'
  emit '{"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"last":{"inputTokens":42,"outputTokens":7}}}}'
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

# NOTE: steer-race before steer — `case` takes the first matching glob.
*scenario:steer-race*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  has "$steerline" '"method":"turn/steer"' ||
    { emit "{\"id\":$sid,\"result\":{}}"; emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected turn/steer"}}}}'; exit 0; }
  # The turn completed under the steer: reject, then announce completion.
  emit "{\"id\":$sid,\"error\":{\"code\":-32602,\"message\":\"turn already completed\"}}"
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  # The harness must fall back to a follow-up turn/start carrying the text.
  read -r followline || exit 1
  fid=$(rid "$followline")
  if has "$followline" '"method":"turn/start"' && has "$followline" 'redirect please'; then
    emit "{\"id\":$fid,\"result\":{\"turn\":{\"id\":\"t-2\"}}}"
    emit '{"method":"turn/started","params":{"turn":{"id":"t-2"}}}'
    emit '{"method":"item/agentMessage/delta","params":{"itemId":"m2","delta":"fallback"}}'
    emit '{"method":"turn/completed","params":{"turn":{"id":"t-2"}}}'
  else
    fail_turn "$fid" "expected fallback turn/start with steer text"
  fi
  ;;

*scenario:steer*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"first"}}'
  read -r steerline || exit 1
  sid=$(rid "$steerline")
  if has "$steerline" '"method":"turn/steer"' &&
    has "$steerline" '"expectedTurnId":"t-1"' &&
    has "$steerline" 'redirect please'; then
    emit "{\"id\":$sid,\"result\":{}}"
    emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"steered"}}'
    emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  else
    emit "{\"id\":$sid,\"error\":{\"code\":-32600,\"message\":\"bad steer\"}}"
    emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"steer verification failed"}}}}'
  fi
  ;;

*scenario:approve*)
  # Wire policy is always "never" (unattended parity with the Claude
  # adapter); the requests below are the STRAY-approval path, which must
  # still round-trip as input questions.
  has "$thread_line" '"approvalPolicy":"never"' ||
    { fail_turn "$tid" "thread approvalPolicy should be never"; exit 0; }
  has "$turnline" '"approvalPolicy":"never"' ||
    { fail_turn "$tid" "turn approvalPolicy should be never"; exit 0; }
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"id":101,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"rm -rf /tmp/x"}}'
  read -r a1 || exit 1
  { has "$a1" '"id":101' && has "$a1" '"decision":"accept"'; } ||
    { emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"command approval not accepted"}}}}'; exit 0; }
  emit '{"id":102,"method":"item/fileChange/requestApproval","params":{"itemId":"f1","changes":[{"path":"/tmp/a.rs","kind":"update"}]}}'
  read -r a2 || exit 1
  { has "$a2" '"id":102' && has "$a2" '"decision":"accept"'; } ||
    { emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"file approval not accepted"}}}}'; exit 0; }
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:decline*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"id":201,"method":"item/commandExecution/requestApproval","params":{"itemId":"c1","command":"rm -rf /"}}'
  read -r a1 || exit 1
  { has "$a1" '"id":201' && has "$a1" '"decision":"decline"'; } ||
    { emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected decline"}}}}'; exit 0; }
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*scenario:interrupt*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}'
  read -r intline || exit 1
  iid=$(rid "$intline")
  if has "$intline" '"method":"turn/interrupt"' && has "$intline" '"turnId":"t-1"'; then
    emit "{\"id\":$iid,\"result\":{}}"
    emit '{"method":"turn/aborted","params":{"turn":{"id":"t-1"}}}'
  else
    emit "{\"id\":$iid,\"result\":{}}"
    emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"expected turn/interrupt"}}}}'
  fi
  ;;

*scenario:wedge*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"item/agentMessage/delta","params":{"itemId":"m1","delta":"working"}}'
  # Ignore turn/interrupt entirely — forces the SIGTERM escalation path.
  exec sleep 30
  ;;

*scenario:fail*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"turn/failed","params":{"turn":{"id":"t-1","error":{"message":"boom"}}}}'
  ;;

*scenario:resumed*)
  emit "{\"id\":$tid,\"result\":{\"turn\":{\"id\":\"t-1\"}}}"
  emit '{"method":"turn/started","params":{"turn":{"id":"t-1"}}}'
  emit '{"method":"turn/completed","params":{"turn":{"id":"t-1"}}}'
  ;;

*)
  fail_turn "$tid" "unknown scenario"
  ;;
esac
