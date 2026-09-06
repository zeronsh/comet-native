`resource-stream.jsonl` contains sanitized deltas from a successful Haiku
profiling response: an 80-section Rust ownership tutorial with code fences.
Only text/reasoning deltas and the successful completion marker are retained;
account, session, timestamp and usage metadata are omitted.

The assistant text is 51,769 bytes, SHA-256
`c1809f92c26c682c6f035478f7ca63980e25fb5e97ea7746dcb41ecd7dbb0a25`.
Reasoning contributes another 853 bytes. The production transcript's part
separators bring the combined short reply to 52,624 bytes.

Use `ZERON_REPLAY_REPEAT=10 ZERON_REPLAY_DELAY_MS=8` for the synthetic long
workload. See [the profiling report](../../docs/performance-resource-usage.md)
for build settings, commands and measured results.

`runway-short-stream.jsonl` is a synthetic short reply in 24-character chunks.
It keeps the own-send runway active through completion. Use a 400 ms replay
delay for the [runway performance comparison](../../docs/performance-runway-scroll.md).
