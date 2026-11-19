# logmon-nextest-wrapper

Record one lifecycle per Rust test attempt on Unix. The run-wrapper uses nextest's
`NEXTEST_BINARY_ID` and `NEXTEST_TEST_NAME` to construct a stable name such as
`rust:catalog::tests::round_trip`. Each invocation, including a retry, generates
a fresh UUIDv4 as 32 lowercase hex digits. The child receives it through
`LOGMON_TEST_TRACE_ID`. The lifecycle selector is `test_attempt_id`, whose
value matches that ID. The tracing-init test attribute puts that field on
telemetry emitted by the test process.

Build once from a tracing-init checkout:

```sh
cargo install --locked --path nextest-wrapper
```

For an opt-in profile in the tested repository's `.config/nextest.toml`:

```toml
experimental = ["wrapper-scripts"]

[scripts.wrapper.logmon]
command = "logmon-nextest-wrapper"

[[profile.nightly-logmon.scripts]]
filter = 'all()'
run-wrapper = "logmon"
```

The profile can inherit existing nightly settings. If the project already enables
experimental features, add `wrapper-scripts` to its list. Use the run wrapper
for execution; listing runs the test binary directly. These nextest environment
variables require nextest 0.9.116 or newer. The hook has been exercised with
0.9.148. [nextest wrapper scripts](https://nexte.st/docs/configuration/wrapper-scripts/)
and [nextest environment variables](https://nexte.st/docs/configuration/env-vars/).

After the driver opens a logmon test-log, launch nextest with these variables:

| Variable | Purpose |
|---|---|
| `LOGMON_TEST_SELECTOR` | Defaults to `test_attempt_id` for process attribution, including work that adopts a different trace. Set `trace_id` when the attempt owns the end-to-end trace and you want standard focused case loading or evidence from downstream services that inherit the trace. Records following another trace are outside this selection. |
| `STORE_TEST_LOG_RUN_ID` | Identity used at begin. With no run ID the wrapper executes the test directly. |
| `STORE_TEST_LOG_DOMAIN` | Broker domain. When omitted, a numeric `STORE_DEV_TRACK` selects `t<track>`; otherwise use the broker default. |
| `STORE_TEST_LOG_EVENTS_PATH` | Local append-only journal for starts, original end payloads, delivery decisions and latency. Choose a path inside this run's directory. |
| `STORE_TEST_LOG_BREAKER_PATH` | Optional path shared by this run's wrappers. Defaults to the journal path with its extension replaced by `.breaker`. |
| `STORE_TEST_LOG_ACK_TIMEOUT_MS` | Deadline covering SDK connection, send and acknowledgement. Default 100 ms for both start and end. |
| `STORE_TEST_LOG_ESTIMATES_PATH` / `STORE_TEST_LOG_TIMEOUTS_PATH` | Optional JSON objects mapping stable Rust names to estimated duration or hard timeout in milliseconds. |
| `STORE_TEST_LOG_RERUN_OF` | The original execution ID when the nightly runs a red test in isolation. |

```sh
cargo nextest run --profile nightly-logmon
```

An unavailable broker leaves the test runnable. The run-local circuit breaker
uses a single recovery probe after 5 seconds, with failed probes backing off
to 30 seconds. A refusal from another run owner leaves that owner's breaker alone.
Replay the journal's unconfirmed end events before closing the test-log.

Stdout and stderr go to nextest as they arrive. For a failed test the end event
also carries the final 8192 input bytes of each stream as lossily decoded UTF-8,
observed byte counts and truncation flags. The test's exit code or Unix signal
passes through. A signalled child reports ABORTED. Nextest owns timeout policy;
if nextest kills the wrapper before end delivery, logmon's configured execution
deadline captures what was retained.

To exercise the actual nextest hook, run from this crate:

```sh
cargo build --locked
python3 tests/nextest.py target/debug/logmon-nextest-wrapper
```

The fixture verifies stable names, fresh IDs on retry, output truncation and
forwarding, child status and fail-open operation while the broker is unavailable.

For the warm timing probe, first build the logmon broker and CLI. Run from the
wrapper crate, passing the actual logmon source checkout:

```sh
python3 tests/bench.py --logmon-repo /path/to/logmon --wrapper target/debug/logmon-nextest-wrapper
```

With 48 cheap tests, 16 slots, five measured rounds per arm, and a private broker
on the development machine, median suite time was 107 ms without the wrapper,
132 ms through the inactive wrapper, 137 ms with lifecycle reporting, and 196 ms
with reporting plus the tracing attribute. Median end-to-end CLI time was 485,
555, 510 and 586 ms respectively. Each active arm acknowledged all 240 starts
and 240 ends. The attributed arm’s p99 was 15.5 ms for starts and 21.2 ms for
ends. This measures process startup, a small test body and a single application
log per attempt; use the Store nightly workload to set its rollout threshold.
