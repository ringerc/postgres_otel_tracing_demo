# postgres_otel_tracing_demo

A demo PostgreSQL extension that consumes spans from `contrib/otel` and ships
them through the **real `opentelemetry-rust` SDK** — to stdout (default) or to
an OTLP collector via gRPC.

Builds against **stock postgres** (19devel / `master`) with `contrib/otel`
installed, OR against [patched postgres](https://github.com/ringerc/postgres/pull/1)
for the full trace-context and log-correlation feature set.  See
[Patched vs unpatched postgres](#patched-vs-unpatched-postgres) below.

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
  `$(pg_config --includedir-server)/extension/otel/otel.h`).  Stock
  upstream postgres works; see the matrix below for what changes
  when the optional core patches are also applied.
* Rust toolchain (stable, edition 2021).
* `pg_config` on `$PATH`, or `PG_CONFIG=/path/to/pg_config` in your env.

### Patched vs unpatched postgres

`contrib/otel` and this demo are designed to build against an
unpatched server.  Two optional core-postgres patch series unlock
additional capabilities; `contrib/otel`'s Makefile / `meson.build`
detect them at compile time and adapt automatically.

| Feature | Stock postgres | + [PR #3][pr3] | + [PR #5][pr5] |
|---------|----------------|----------------|----------------|
| Build `contrib/otel` and this demo | Yes | Yes | Yes |
| Statement spans via executor hooks | Yes | Yes | Yes |
| Trace context via `SET otel.traceparent` | Yes | Yes | Yes |
| Trace context via [sqlcommenter][sqlc] comments | Yes | Yes | Yes |
| Trace context via the `M` (RequestHeaders) protocol message | — | Yes | Yes |
| Structured trace context in JSON / CSV log output | — | — | Yes |
| `%A` / `%{trace_id}A` `log_line_prefix` escapes | — | — | Yes |
| Textual `trace_id=...` fallback appended to log `CONTEXT:` | Yes (always-on fallback) | Yes | — (superseded) |

[pr3]: https://github.com/ringerc/postgres/pull/3 "core: protocol headers ('M' message + _pq_.headers negotiation)"
[pr5]: https://github.com/ringerc/postgres/pull/5 "core: generic key/value annotations on ErrorData"

Patch series, in dependency order:

* **[PR #3][pr3] — protocol headers.**  Adds the `'M'`
  (RequestHeaders) frontend message, the `_pq_.headers=1` startup
  negotiation, and the `RegisterProtocolHeaderHandler` extension
  API.  `contrib/otel` registers an `otel.` prefix handler so
  clients can attach `traceparent` / `tracestate` to each query
  out-of-band, without having to put trace context inside the SQL
  text (sqlcommenter) or burn a round-trip on `SET`.  Depends on
  [PR #4][pr4] (`pre_ready_for_query_hook`) only for
  statement-scope semantics, which `contrib/otel` does not
  currently use.
* **[PR #5][pr5] — elog annotations.**  Adds a generic
  key/value annotations list on `ErrorData`, the `errannot()` /
  `errannotf()` helpers, structured emission through the JSON and
  CSV log writers, and `%A` / `%{key}A` escapes in
  `log_line_prefix`.  `contrib/otel` attaches `trace_id`,
  `span_id`, `trace_flags` as annotations under well-known keys so
  operators can correlate log lines to traces in any log format.
  Without this patch, `contrib/otel` falls back to appending a
  textual `trace_id=... span_id=... trace_flags=...` line to
  `edata->context`, which still surfaces in stderr / syslog log
  output but isn't structured.

[pr4]: https://github.com/ringerc/postgres/pull/4 "core: pre_ready_for_query_hook"
[sqlc]: https://google.github.io/sqlcommenter/

Detection mechanism (informational): `contrib/otel`'s build
probes the installed server headers for `libpq/protocol_headers.h`
and for an `errannot` prototype in `utils/elog.h`, and defines
`-DOTEL_HAVE_PROTOCOL_HEADERS` / `-DOTEL_HAVE_ERRANNOT`
accordingly.  No configure step or feature flags are required on
the consumer side; this demo links against the same headers and
inherits the matching code paths.

## Build & install

```bash
make            # cargo build --release
sudo make install
```

`make install` drops `postgres_otel_tracing_demo.so` into
`$(pg_config --pkglibdir)`.  No SQL surface, no `.control` file — the module
is activated purely via `shared_preload_libraries`.

## Configuration

Load order matters.  Three modules in `shared_preload_libraries`, in
order:

* `otel` &mdash; the API/infrastructure module.  Publishes the
  `OtelTracingApi` rendezvous variable that subsequent modules
  consume.  Must come first.
* `otel_postgres_tracing` &mdash; the query-instrumentation
  consumer that actually produces statement spans via executor
  hooks.  Without it, the demo exporter loads but never sees any
  query spans (only any spans emitted directly via the producer
  API).  Must come after `otel`.
* `postgres_otel_tracing_demo` &mdash; this crate.  Locates the
  api via the rendezvous variable and registers as a span
  exporter.  Order relative to `otel_postgres_tracing` doesn't
  matter for correctness as long as both come after `otel`, but
  listing this last reads naturally.

```ini
# postgresql.conf
shared_preload_libraries = 'otel,otel_postgres_tracing,postgres_otel_tracing_demo'

# Optional: emit a span for every query rather than only those
# carrying a client-supplied traceparent.
otel.trace_all_queries = on
```

The earlier (pre-split) configuration that listed only
`shared_preload_libraries = 'otel,postgres_otel_tracing_demo'` no
longer produces query spans &mdash; the query-tracing hooks
moved out of `contrib/otel` into the separate
`contrib/otel_postgres_tracing` module.  See the
[contrib/otel split design notes](doc/concepts/core-changes.md)
(if a copy is in the repo) or the postgres tree's SGML docs for
the split rationale.

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

### `OTEL_TRACES_SAMPLER`

Honoured, with one important scoping note: contrib/otel's sampler
hook is only consulted when the propagated W3C `sampled` bit is
**unset** (see "decide_whether_to_record" gate 5 in contrib/otel).
So:

| Situation | Effect of `OTEL_TRACES_SAMPLER` |
|-----------|------|
| Client traceparent has `sampled=1` | Always recorded (W3C compliance, gate 4); sampler env var is ignored. |
| `otel.trace_all_queries=on` | Always recorded (gate 2); sampler env var is ignored. |
| Client traceparent has `sampled=0` | The configured sampler decides. |
| No client traceparent and `trace_all_queries=off` | Span is dropped before the hook is reached (gate 3). |

Supported sampler names (case-insensitive):

* `always_on` / `parentbased_always_on` (the default)
* `always_off` / `parentbased_always_off`
* `traceidratio` / `parentbased_traceidratio`  — uses `OTEL_TRACES_SAMPLER_ARG` as the ratio (`0.0`–`1.0`, default `1.0`)
* `jaeger_remote` / `parentbased_jaeger_remote` — **NOT supported** in this demo; the SDK requires extra plumbing (HTTP client, dedicated runtime) that we don't pull in.  Falls back to the contrib/otel default (drop on unset bit).

The `parentbased_*` prefix is informational here: contrib/otel
already applies parent-based logic in C, so we strip the prefix
and use the delegate sampler directly.

If you want the OTel sampler to be able to **override** an
upstream-sampled (`sampled=1`) bit — e.g. apply local rate limits
to traces that arrived already sampled — change the
sampler-hook **invocation policy** (next section).

### `POSTGRES_OTEL_SAMPLER_HOOK_POLICY` (non-standard)

contrib/otel exposes a policy knob that controls WHEN the sampler
hook gets called.  This is not an OTel-spec concept (W3C and the
OTel SDK both treat the sampled bit as authoritative), so the env
var carries a project-prefixed name.

| Value | When the SDK sampler runs | When the W3C bit is respected |
|-------|---------------------------|-------------------------------|
| `hook_on_unsampled_bit` (default) | Only when wire `sampled=0` | `sampled=1` always records |
| `hook_always` | Every span, regardless of wire bit | Sampler can override `sampled=1` |
| `never_respect_bit` | Never (hook ignored) | `sampled=1` → record, `sampled=0` → drop |
| `never_always_sample` | Never (hook ignored) | Wire bit ignored; record everything |

Use cases:

* `hook_on_unsampled_bit` — strict W3C compliance.  The default.
* `hook_always` — you want local rate limits or tail-based sampling
  to be able to drop traces that arrived already sampled.
  Trade-off: violates the W3C "sampled=1 means downstream should
  see this span" guarantee.
* `never_respect_bit` — you trust the upstream's wire signal
  exclusively; the sampler env vars become no-ops.  Lowest hot-
  path cost.
* `never_always_sample` — debug mode: record every span that
  reached gate 4 (has propagated context).  Sampler is ignored.

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
* `jaeger_remote` sampler (needs HTTP client + dedicated runtime).
* Configuration via postgres GUCs.  Env vars only.

## License

PostgreSQL License — see [LICENSE](LICENSE).
