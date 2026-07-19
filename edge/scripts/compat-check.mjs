// Cross-language compat gate: a snapshot written by the Rust `comet-doc` crate must load in
// loro-crdt JS and materialize the same tail shape the edge serves.
//
// Usage: node scripts/compat-check.mjs <snapshot-path>
// (Generate the snapshot with: cargo run -p comet-doc --example gen_fixture -- /tmp/fixture.loro)

import { readFileSync } from "node:fs";
import { LoroDoc } from "loro-crdt";

const path = process.argv[2];
if (!path) {
  console.error("usage: compat-check.mjs <snapshot-path>");
  process.exit(2);
}

let failures = 0;
const check = (name, cond, detail) => {
  if (cond) console.log(`ok   ${name}`);
  else {
    failures++;
    console.error(`FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
};

const doc = new LoroDoc();
doc.import(new Uint8Array(readFileSync(path)));
const json = doc.toJSON();

check("meta.chatId", json.meta?.chatId === "chat-fixture-1", JSON.stringify(json.meta));
check("meta.schemaVersion", json.meta?.schemaVersion === 1);

const messages = json.messages ?? [];
check("two messages", messages.length === 2, `got ${messages.length}`);

const user = messages[0];
check("user role", user?.role === "user");
check("user text part", user?.parts?.[0]?.kind === "text" && user.parts[0].text === "Run the tests please",
  JSON.stringify(user?.parts));
check("user deviceId", user?.deviceId === "device-rust");
check("user createdAt", user?.createdAt === 1700000000000);

const assistant = messages[1];
check("assistant role", assistant?.role === "assistant");
check("assistant status complete", assistant?.status === "complete");
const parts = assistant?.parts ?? [];
check("assistant part count", parts.length === 3, JSON.stringify(parts.map((p) => p.kind)));
check(
  "streamed text merged",
  parts[0]?.kind === "text" && parts[0].text === "Sure — running them now.",
  JSON.stringify(parts[0])
);
check(
  "tool part shape",
  parts[1]?.kind === "tool" &&
    parts[1].call?.kind === "exec" &&
    parts[1].call?.command === "cargo test" &&
    parts[1].isError === false,
  JSON.stringify(parts[1])
);
check("post-tool text", parts[2]?.kind === "text" && parts[2].text === "All green.");

const commands = json.commands ?? [];
check("one command", commands.length === 1);
check("command outcome applied", commands[0]?.status === "applied", JSON.stringify(commands[0]));
check("command payload kind", commands[0]?.payload?.kind === "steer", JSON.stringify(commands[0]?.payload));

// Tail materialization through the edge's own vendored code path (plain JSON walk equivalent):
// replicate materializeTail's join+slice here to keep this script dependency-light.
const joined = messages.filter((m) => !m.continuationOf);
check("tail joins cleanly", joined.length === 2);

if (failures > 0) {
  console.error(`\n${failures} compat check(s) failed`);
  process.exit(1);
}
console.log("\nall compat checks passed — Rust-written doc reads cleanly in loro-crdt JS");
