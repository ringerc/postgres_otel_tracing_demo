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

### Per-backend SDK, not per-postmaster (and why this is suboptimal)

postgres `fork()`s a backend for each connection.  `BatchSpanProcessor`
spawns a worker thread — and threads don't survive fork.  If we built the
processor in `_PG_init` (which runs once in the postmaster), every backend
would inherit only the *sender* half of a channel whose receiver thread
was lost at fork; spans would silently vanish.

What this demo does: `_PG_init` only registers the emit hook.  The SDK
(BatchSpanProcessor, exporter, tokio runtime, resource) is constructed
lazily in each backend the first time it emits a span.

**This is fine for a demo but not the architecture you actually want.**
The opentelemetry-rust SDK is designed around long-lived in-process
state: one provider, one batch processor, one exporter connection,
threads shared across the whole workload.  We are using it
backwards — every backend pays for its own:

* tokio current-thread runtime (one OS thread per active backend)
* BatchSpanProcessor worker + bounded channel
* exporter state (for OTLP: its own gRPC connection, TLS handshake,
  HTTP/2 stream multiplex)
* Resource detection (env-var parsing, default-detector logic)

At idle these costs are noise; under load (hundreds of concurrent
backends, especially with frequent connect/disconnect) they add up:
RAM per backend creeps, every short-lived backend pays the OTLP
TLS handshake on its way to also dropping its last in-flight batch,
the collector sees N tiny streams instead of one fat one, and
batching becomes per-backend rather than across the cluster.

**The fix for production use** is the standard postgres pattern: a
single background worker owns the SDK, and backends hand spans off
to it via shared memory.  The bgworker is started by the postmaster
before any backend forks, so it can hold long-lived OTel SDK state,
hold one OTLP connection, and batch across the whole cluster.

Two reasonable shm queues:

* `shm_mq` (postgres-native ring buffer, single-producer-single-consumer
  — would need one queue per backend, all polled by the bgworker).
  Simple but the bgworker's wake/poll loop gets gnarly at high backend
  counts.
* A lock-free MPSC ring (e.g. an `Arc<crossbeam::queue::ArrayQueue>`
  over a postgres shared-memory segment, or a hand-rolled ring with
  atomics).  One queue for the whole cluster, backends are pure
  producers, the bgworker is the only consumer.  Much friendlier
  scaling characteristics, more code to get right.

Span serialization across the boundary needs a stable wire format
since the bgworker can't deref backend-owned `palloc` pointers — the
easiest is to encode each span as a pre-serialized OTLP-Span protobuf
in the backend (skipping the SDK's full processor) and just hand the
opaque byte buffer to the bgworker, which then drains a batch into
the SDK's exporter.

This demo intentionally skips all that for simplicity; expect a
follow-on (`postgres_otel_tracing` proper, dropping the `_demo`
suffix) to do it right.

### Force-set `TraceFlags::SAMPLED` on the SDK-side `SpanContext`

There are two distinct things commonly called "the sampled bit," and
opentelemetry-rust conflates them:

1. **W3C TraceContext wire signal.**  Bit 0 of `trace_flags` in the
   `traceparent` header.  Says: "upstream is recording this trace; if
   you want a continuous trace, you should too."  This is what
   contrib/otel parses into `OtelSpan.trace_flags`.
2. **opentelemetry-rust's local exporter gate.**  `SpanContext::
   is_sampled()` reads the same bit, but the SDK uses the result to
   answer a different question --- "should the local exporter
   export this span?"  `BatchSpanProcessor::on_end` silently drops
   any span where it returns false.

In a vanilla opentelemetry-rust pipeline these collapse into one
truth because the SDK's own sampler is what sets the bit.  Sampler
decides RECORD_AND_SAMPLE → SDK writes the bit → exporter sees the
bit and exports.  No drift possible.

We bypassed the SDK's sampler.  contrib/otel made the recording
decision in C and handed us a finished span.  The W3C bit on that
span reflects whatever the upstream client put on the wire, which
can be `00` even for spans contrib/otel definitely decided to
record --- e.g. `otel.trace_all_queries=on` overrode an unsampled
parent, or the sampler hook returned RECORD_AND_SAMPLE without the
wire bit being set.

If we passed that bit through unchanged, `BatchSpanProcessor::
on_end` would drop spans contrib/otel intentionally recorded.  The
fix is local to the SDK-side `SpanContext` we construct: force
`TraceFlags::SAMPLED`.  The wire bit in `OtelSpan.trace_flags` is
not touched (and is anyway not propagated anywhere by this demo);
all we're doing is telling the SDK "yes, export this."

### Why no `pgrx`?

[`pgrx`](https://github.com/pgcentralfoundation/pgrx) is the de-facto
framework for postgres extensions in Rust.  It would resolve some of
the friction this demo hits manually --- `pg_module_magic!()` replaces
the magic-func dance below, `BackgroundWorker` registration is
first-class, and `shm_mq` plumbing is wrapped in safe Rust types.

For this demo we deliberately stay on a bare `cdylib` for two
reasons:

1. **Story clarity.**  The point of contrib/otel's rendezvous API is
   that any out-of-tree Rust module can plug in; adding `pgrx` to the
   recipe would suggest it's a required ingredient.  The "minimum to
   consume contrib/otel" answer should be `opentelemetry-rust` + ~500
   lines + bindgen, not "and pull in pgrx + pg_sys."
2. **Dependency surface.**  `pgrx` brings the full `pg_sys` generated
   bindings (most of postgres' header tree), the `cargo-pgrx` CLI,
   and a release cadence pinned to specific postgres majors.  We
   need none of postgres' SQL-side machinery in a pure preload hook.

The **production** version --- the one with a postmaster-started
bgworker that owns the SDK and drains spans from a shared-memory
queue (see "Per-backend SDK" above) --- is a different conversation.
By the time we want `BackgroundWorker` registration, shared-memory
segment hooks, postgres GUCs for runtime config, and a real test
harness (`cargo pgrx test`), `pgrx` is the right call: every one of
those is much easier through `pgrx` than by hand.  This demo is the
"prove the API works" step; that one is "make it production-shaped."

### `Pg_magic_func` in pure Rust (pgrx-style, no C shim)

PostgreSQL requires every loadable module to expose a `Pg_magic_func`
that returns a versioned `Pg_magic_struct` describing the ABI it was
built against.  The canonical way to produce one is the
`PG_MODULE_MAGIC` macro in C.

We build it in pure Rust instead, the same way pgrx does: bindgen
extracts the constants we need from the postgres headers
(`PG_VERSION_NUM`, `FUNC_MAX_ARGS`, `INDEX_MAX_KEYS`, `NAMEDATALEN`,
`FLOAT8PASSBYVAL`, `FMGR_ABI_EXTRA`) as Rust `pub const`s, then
[src/lib.rs](src/lib.rs) emits a `Pg_magic_struct` static with those
values and exposes it via `#[no_mangle] pub extern "C" fn
Pg_magic_func`.

This sidesteps two problems the C-shim approach had:

* The shim had to be linked into the cdylib, but Rust's cdylib link
  applies a version script that demotes archive-pulled symbols to
  LOCAL.  We'd had to use a `#[no_mangle]` Rust trampoline to drag
  the name into the dynamic symbol table.  With pure-Rust, the
  `Pg_magic_func` definition IS the `#[no_mangle]` item and lands
  in dynsym directly.
* No `cc` build-dep, no C toolchain required at extension-build
  time.  bindgen still calls libclang under the hood, but the build
  is otherwise pure-cargo.

`Pg_magic_struct` contains `*const c_char` fields (the optional
`name` / `version`) so a plain `static` would fail to be `Sync`.
Wrapping in a `#[repr(transparent)] struct AssertSync<T>(T)` with
an `unsafe impl<T> Sync for AssertSync<T>` is the standard pgrx
move and is sound because we only ever read the static.

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
