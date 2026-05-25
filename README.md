# postgres_otel_tracing_demo

A demo PostgreSQL extension that consumes spans from `contrib/otel` and ships
them through the **real `opentelemetry-rust` SDK** — to stdout (default) or to
an OTLP collector via gRPC.

Requires [patched postgres + `contrib/otel`](https://github.com/ringerc/postgres/pull/1)

Status: proof-of-concept, smoke-tested against postgres 19devel.

## What it demonstrates

`contrib/otel` ships trace-context plumbing and a span data model, but the
postgres project doesn't want a hard dependency on any specific OTel SDK.
This crate is the worked example of how an out-of-tree module plugs in:

* Loads as a postgres `shared_preload_libraries` module.
* Locates `contrib/otel`'s `OtelTracingApi` table via the rendezvous variable
  `OtelTracingApi`, version-checks it.
* Registers a span-emit callback through that API.
* On each span, translates the C `OtelSpan` into an `opentelemetry_sdk`
  `SpanData` and pushes it through the SDK's `BatchSpanProcessor`.
* Honours the standard OpenTelemetry environment variables for exporter
  selection and resource attributes.

## Requirements

* PostgreSQL with `contrib/otel` installed (header lives at
  `$(pg_config --includedir-server)/extension/otel/otel.h`).
* Rust toolchain (stable, edition 2021).
* `pg_config` on `$PATH`, or `PG_CONFIG=/path/to/pg_config` in your env.

## Build & install

```bash
make            # cargo build --release
sudo make install
```

`make install` drops `postgres_otel_tracing_demo.so` into
`$(pg_config --pkglibdir)`.  No SQL surface, no `.control` file — the module
is activated purely via `shared_preload_libraries`.

## Configuration

Load order matters: `otel` MUST come before this module so the rendezvous
variable is populated by the time we look for it.

```ini
# postgresql.conf
shared_preload_libraries = 'otel,postgres_otel_tracing_demo'

# Optional: have contrib/otel produce a span for every query rather than
# only for queries carrying a client-supplied traceparent.
otel.trace_all_queries = on
```

### Environment variables

All [standard OTel env vars](https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/)
that the rust SDK recognizes are honoured.  The most useful ones:

| Variable | Effect |
|----------|--------|
| `OTEL_TRACES_EXPORTER` | `stdout` (default), `console`, `logging`, `otlp`, `none`.  Unknown values default to stdout. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP collector endpoint (default: `http://localhost:4317` for gRPC) |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc` (only protocol enabled by the demo's feature flags) |
| `OTEL_EXPORTER_OTLP_HEADERS` | Comma-separated `k=v` HTTP/gRPC headers (e.g. tenant tokens) |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in ms |
| `OTEL_SERVICE_NAME` | Service name (default: `postgres`) |
| `OTEL_RESOURCE_ATTRIBUTES` | Additional resource k=v pairs |

`OTEL_TRACES_SAMPLER` is intentionally NOT honoured here.  Sampling is
contrib/otel's responsibility (via its own sampler hook and the
`otel.trace_all_queries` GUC); by the time a span reaches this exporter,
it has already been sampled in.

Set env vars in whatever launches postgres (systemd unit, docker compose,
shell wrapper) so the postmaster inherits them.

## Architecture notes

### Per-backend SDK, not per-postmaster

postgres `fork()`s a backend for each connection.  `BatchSpanProcessor`
spawns a worker thread — and threads don't survive fork.  If we built the
processor in `_PG_init` (which runs once in the postmaster), every backend
would inherit only the *sender* half of a channel whose receiver thread
was lost at fork; spans would silently vanish.

Mitigation: `_PG_init` only registers the emit hook.  The SDK
(BatchSpanProcessor, exporter, runtime, resource) is constructed lazily
in each backend the first time it emits a span.  Cost: one worker thread
+ one exporter connection per active backend.  This matches the postgres
process model.

### Force-set `TraceFlags::SAMPLED`

`BatchSpanProcessor::on_end` silently drops any span whose
`SpanContext.is_sampled()` returns false.  The W3C trace_flags byte that
arrives in `OtelSpan` reflects the propagated client signal, which may
have `sampled=0`.  contrib/otel's sampler has already decided to record
the span by the time it reaches our emit hook, so we force `sampled=1` on
the SpanContext we hand to the SDK.  The propagated flags are otherwise
preserved.

### `Pg_magic_func` exported via a C shim + Rust trampoline

PostgreSQL requires every loadable module to expose a `Pg_magic_func`
that returns a versioned ABI struct.  Reproducing the struct contents
from Rust would mean re-implementing `pg_config.h` macros; we cheat by
compiling a tiny `c_shim/magic.c` that uses the canonical
`PG_MODULE_MAGIC` macro under a renamed function name, then re-exporting
that under the postgres-expected name via a `#[no_mangle]` Rust
trampoline.  Rust's cdylib link applies a version script that demotes
archive-pulled symbols to LOCAL; the trampoline is what gets the
postgres-visible name into the dynamic symbol table.

### Shutdown via `on_proc_exit`

BatchSpanProcessor's last in-flight batch would be lost when the backend
process exits and its worker thread dies.  We register an `on_proc_exit`
callback (declared as an extern from Rust) that calls `force_flush` +
`shutdown` on the processor.  Without this, anything emitted in the
final `OTEL_BSP_SCHEDULE_DELAY` window vanishes.

## What's NOT in here (yet)

* OTLP HTTP protocol (the `grpc-tonic` feature is the only one enabled;
  adding `http-proto` is a Cargo.toml flip).
* `OTEL_TRACES_SAMPLER` honouring (deliberate — see above).
* Sampler-hook registration (contrib/otel exposes
  `register_sampler_hook` in the same api but this demo only registers
  emit).
* Configuration via postgres GUCs.  Env vars only.

## License

PostgreSQL License — see [LICENSE](LICENSE).
