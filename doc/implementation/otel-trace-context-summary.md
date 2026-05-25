# OpenTelemetry trace context in PostgreSQL — work summary

See also:

* [doc/concepts/core-changes.md](../concepts/core-changes.md) for a high level summary of the changes made to postgres core to support this work, and why they were made; and
* [doc/implementation/core-changes-details.md](core-changes-details.md) for the itemised list of core-only changes with justifications.

> [!WARNING]
> **LLM-generated material follows**

Snapshot of the work to date on adding W3C / OpenTelemetry trace
context support to PostgreSQL.

Branches:

- **postgres submodule** (postgres patches): `postgres-otel-tracing`,
  head `814e3d2317a`.  22-commit series on top of upstream
  PostgreSQL master.
- **`ringerc/postgres_otel_tracing_demo`** (Rust out-of-tree
  exporter): `main`, head `940a89a`.  9 commits.

Test coverage across both repos: **107 TAP subtests, all passing**,
across five suites — `test_protocol_headers`, `libpq_headers`,
`contrib/otel`, `otel_test_exporter`, `contrib/otel_exporter`.

---

## (a) Core PostgreSQL changes

Eight commits in the postgres submodule touch core
(`src/backend`, `src/include`, `src/interfaces/libpq`).  These are
the changes that genuinely require upstream patches; everything
else is contrib.

```
462011ca8d9  Add W3C Trace Context fields to elog/ereport
38816e5536d  Add per-message protocol headers (RequestHeaders, 'M')
b7d6f660a51  Add test_protocol_headers TAP test
63d2eaa8d39  Affirmatively acknowledge _pq_.headers via ParameterStatus
e9fa58bc6d1  libpq: add PQattachHeader / PQclearHeaders / PQheadersAvailable
de27e9a13c0  doc: PQattachHeader, PQclearHeaders, PQheadersAvailable
8baa656bd8e  libpq_headers: broader coverage for the headers client API
2f313bf698c  Add contrib/otel: OpenTelemetry trace-context consumer
```

(The last one introduces `contrib/otel` but does not modify core
otherwise.)

### Functional content

1. **elog / ereport trace-context fields.** `ErrorData` gains
   `trace_id` / `span_id` / `trace_flags`; new `errtrace()` helper;
   full lifecycle plumbing through `CopyErrorData`,
   `ThrowErrorData`, `ReThrowError`, `FreeErrorDataContents`.
   JSON log writer emits the fields as top-level keys; CSV log
   gains three trailing columns; `log_line_prefix` gains `%T` and
   `%S`.  No server→client wire-side emission — W3C propagation is
   one-way by design.

2. **Per-message protocol headers (`'M'` / RequestHeaders).** New
   wire-protocol message carrying namespaced `(key, value)`
   entries.  Negotiated via `_pq_.headers=1` startup option, with
   affirmative `protocol_features` `ParameterStatus` to defend
   against pgbouncer-class proxies that silently strip the opt-in.
   Extension API `RegisterProtocolHeaderHandler` with three scopes
   (per-statement / per-transaction / per-session); server GUCs
   gate the feature and bound header sizes.

3. **libpq client API.** `PQattachHeader` / `PQclearHeaders` /
   `PQheadersAvailable`.  Pre-attach buffering model with
   auto-flush at the start of each `PQsend*` operation.
   StartupMessage always advertises `_pq_.headers=1`;
   `headersAvailable` flips true only on receipt of the affirmative
   `protocol_features` ParameterStatus.  Backward-compat fix in
   `pqGetNegotiateProtocolVersion3` so libpq does not break against
   feature-disabled or older servers.  SGML documentation added.

### Test coverage (core)

- `test_protocol_headers/001_headers` — wire-level negotiation,
  dispatch, scope clearing, ParameterStatus acknowledgement.
- `libpq_headers/001_libpq_headers` — `PQheadersAvailable` both
  modes, attach + send, clear, queue reset, NULL-key defence, and
  the feature-disabled-server fallback path with server restart.

### Why these had to be in core, not contrib

Itemised with citations against the postgres source in
[../concepts/core-changes.md](../concepts/core-changes.md).
The short version: no extension hook exists for adding fields to
`ErrorData`, for top-level keys in the JSON / CSV log writers, for
`log_line_prefix` format letters, for new wire-protocol message
types, for `_pq_.*` startup-option negotiation, or for libpq's
StartupMessage / `PQsend*` paths.

---

## (b) `contrib/otel`

The OpenTelemetry consumer of the headers mechanism.  Twelve
commits, all in the postgres submodule under `contrib/otel/`.

```
2f313bf698c  Add contrib/otel: OpenTelemetry trace-context consumer
63ec1339aed  contrib/otel: propagate trace context to parallel workers
6dd507c4aac  contrib/otel: expand tracestate rationale + note baggage is out of scope
48499c4c6e1  contrib/otel: span data model + exporter hook API; test exporter
d1bf7cb354d  contrib/otel: span lifecycle via ExecutorStart/End hooks
26026ea6da8  contrib/otel: capture ereport as span events
c0725415ba7  contrib/otel: spans for utility commands via ProcessUtility_hook
1527784f0d5  contrib/otel: built-in JSON log-line span emitter
7cf6b83581d  contrib/otel: sampler hook + ParentBased default for unsampled traces
216b148d5ab  contrib/otel: versioned rendezvous API + install header
3912811838b  contrib/otel: configurable sampler-hook invocation policy
6e3df8a7b4b  contrib/otel: document client-side SET / SET LOCAL propagation
```

### Functional layers

1. **Trace-context ingestion.** Registers an `_otel.*` protocol
   header handler.  `traceparent` / `tracestate` arriving in `'M'`
   messages land in the custom GUCs `otel.traceparent` and
   `otel.tracestate`.  These are `PGC_USERSET`, so client-side
   `SET` / `SET LOCAL` is also supported as a fallback for clients
   that lack `'M'` support — with the round-trip / scope /
   pooler-leak caveats documented inline in `contrib/otel/otel.c`.
   SQL introspection via `otel_current_traceparent()`.

2. **Parallel-worker propagation.** Falls out of the GUC choice —
   `RestoreGUCState` carries `otel.traceparent` and
   `otel.tracestate` into parallel workers, plus the
   leader-span-id GUC needed to thread worker spans to the right
   parent.  No bespoke parallel-state plumbing.

3. **Span emission.** Hook integration:

   - `ExecutorStart` / `ExecutorEnd` — per-query span lifecycle.
   - `ProcessUtility_hook` — spans for utility commands.
   - `emit_log_hook` — `ereport` captured as span events.
   - Built-in JSON log-line emitter as a default sink (writes
     spans to the postgres log).

4. **Sampler hook with configurable invocation policy.**
   ParentBased by default; the sampler hook is consulted only when
   the propagated W3C sampled bit is unset, so unsampled traces
   pay no span-construction cost.  Configurable via
   `OtelSamplerHookPolicy` and `api->set_sampler_policy`, with
   four regimes — strict W3C default, always-defer-to-hook,
   never-call-hook-trust-the-bit, never-call-hook-record-everything
   — letting exporters opt into tail-based sampling, rate-limited
   tracing, or zero-cost W3C compliance.

5. **Versioned rendezvous API.**  External exporters locate
   `contrib/otel` via the rendezvous variable named
   `OTEL_TRACING_API_RENDEZVOUS_NAME`, version-check the
   `OtelTracingApi` struct, and register through it.  Replaced an
   earlier scheme that exported raw `PGDLLEXPORT` globals —
   awkward on Windows, no path for ABI evolution.  Current API
   version is **2** (bumped from 1 when the configurable sampler
   policy landed).  Strict-equality version check: existing
   consumers need a rebuild against the new header but their
   source is unchanged.  Header (`otel.h`) is installed into
   `$(pg_config --includedir-server)/extension/otel/`.

### Test coverage (`contrib/otel`)

- `contrib/otel/t/001_otel` — handshake, trace-context end-to-end,
  scope persistence, malformed / empty handling, parallel-worker
  propagation.

### Companion test module — `src/test/modules/otel_test_exporter`

A test-only consumer of the rendezvous API, used to drive
contrib/otel through the assertions a real exporter cannot make
from outside.  Three suites:

- `001_basic` — span lifecycle across executor + utility paths,
  ereport-as-event, sampler behaviour with the default policy.
- `002_log_emitter` — the built-in JSON log-line emitter.
- `003_sampler_policy` — 11-cell matrix covering the four
  sampler-hook policies × wire `sampled` bit × sampler decisions
  (`record_and_sample` / `record_only` / `drop`).  All cells run
  on the same backend so per-session GUC + policy state holds.

---

## (c) `contrib/otel_exporter`

A bare-minimum file emitter that exercises the rendezvous API
end-to-end.  Two commits.

```
64c5808d337  contrib/otel_exporter: bare-minimum file emitter
497d75d388a  contrib/otel_exporter: rigorous end-to-end + parallel-worker test cover
```

### What it is

~200 lines of C, no out-of-tree dependencies.  Locates the
`OtelTracingApi` at `_PG_init`, registers an emit callback,
appends one JSON object per span to a file path configured by the
`otel_exporter.output_file` GUC.  Each backend opens the file with
`O_APPEND` and keeps its own FILE*; JSON lines are short enough to
hit POSIX's `PIPE_BUF` atomic-append guarantee, so concurrent
leader + worker writes don't interleave.

Deliberately a sketch, not a real exporter.  The JSON shape
carries only headline fields (trace IDs, name, status, timing,
`db.statement`) — anyone wanting full attribute / event capture
should plug a real OpenTelemetry SDK into the same API (see (d)).
This module exists to demonstrate the API is usable and to provide
a target for TAP tests that need to assert on concrete output.

Failure handling: any allocation / I/O failure inside the emit
hook is caught and logged at LOG level, never escalated to the
user's query path.  Preserves contrib/otel's "best-effort under
OOM" contract.

### Test coverage (`contrib/otel_exporter`)

- `001_file_exporter` — 20 subtests (was 8 in the initial
  commit).  Sequential coverage with precise field-equality
  assertions on `trace_id`, `parent_span_id` (== client-supplied
  `span_id`), `db.statement`, plus a check that the leader's
  `span_id` is a fresh 16-hex value, not an echo of the client's
  parent.

  Parallel coverage uses `SET debug_parallel_query=regress` and
  asserts end-to-end that:

  1. The rendezvous-installed emit hook fires inside parallel
     workers (otherwise no worker span would appear).
  2. `trace_id` flows client → leader → workers via
     `otel.traceparent`'s parallel-state propagation.
  3. Parent-span linkage is the *leader's* `span_id`, not a
     re-root at the client — so traces form a tree rather than a
     flat fan-out.
  4. Every parallel span carries the same `trace_id`; worker
     `span_id`s are distinct from each other, from the leader,
     and from the client-supplied parent.

  Incidental: an "every non-empty line parses as a complete JSON
  object" assertion guards O_APPEND atomicity for the concurrent
  leader + worker writes.

---

## (d) `ringerc/postgres_otel_tracing_demo`

External Rust crate at <https://github.com/ringerc/postgres_otel_tracing_demo>
that demonstrates a real OpenTelemetry SDK consuming the
contrib/otel rendezvous API.  9 commits on `main`.

```
36df984  Initial demo OTel exporter for contrib/otel
b0532ce  docs: Link required patched postgres tree
d5d2155  README: document per-backend SDK overhead + bgworker alternative
9fbee7f  README: clearer "Force-set SAMPLED" explanation
249d8c1  README: document why this demo is bare cdylib, not pgrx
66f3b82  Drop C shim; emit Pg_magic_func from pure Rust (pgrx-style)
fc78792  Register sampler hook; honour OTEL_TRACES_SAMPLER env var
bb704d1  Honour POSTGRES_OTEL_SAMPLER_HOOK_POLICY env var
940a89a  Add TESTING.md with manual smoke-test matrix
```

### What it is

A `cdylib` postgres loadable module written in pure Rust (no C
shim) that:

- Locates `contrib/otel`'s `OtelTracingApi` table via the
  rendezvous variable at `_PG_init`, version-checks it, and
  registers an emit callback.
- On each span, translates the C `OtelSpan` into an
  `opentelemetry_sdk::trace::SpanData` and pushes it through the
  SDK's `BatchSpanProcessor`.
- Exports via the standard OpenTelemetry rust exporters: stdout
  (default), `console`, `logging`, or OTLP/gRPC to a collector
  endpoint.
- Honours the full set of standard OpenTelemetry environment
  variables (`OTEL_TRACES_EXPORTER`, `OTEL_EXPORTER_OTLP_ENDPOINT`
  / `_PROTOCOL` / `_HEADERS` / `_TIMEOUT`, `OTEL_SERVICE_NAME`,
  `OTEL_RESOURCE_ATTRIBUTES`).
- Registers a sampler hook driven by `OTEL_TRACES_SAMPLER`, and
  honours `POSTGRES_OTEL_SAMPLER_HOOK_POLICY` (one of the four
  regimes contrib/otel exposes) so an operator can pick the W3C
  / tail-based / always-record / never-call-hook posture
  end-to-end via env vars.

### Why this exists

Two purposes:

1. **Worked example for would-be exporter authors.** The
   contrib/otel rendezvous API is meant to be the integration
   point; this crate is the proof that it works through a real
   SDK with all the attendant async, tokio, batch-processor, OTLP
   serialisation machinery.  Anyone building a Rust- or
   C++-based exporter can fork or reference this.

2. **Keeps the heavy OTel SDK dependency out of the postgres
   tree.**  PostgreSQL would not (and should not) take on
   `opentelemetry-rust`, `tokio`, `tonic`, and the rest as core
   or contrib build-time dependencies.  Out-of-tree is where this
   lives.

### Status

Proof-of-concept.  Smoke-tested against postgres 19devel with
`contrib/otel` installed.  TESTING.md (commit `940a89a`) records
a manual smoke-test matrix covering exporter / sampler / policy
permutations.  Per-backend SDK overhead is documented in the
README as the main caveat — every backend loading the SDK pays
its initialisation cost; the README sketches a bgworker
alternative as future work for high-fan-out workloads.

Notably the crate is a plain `cdylib`, not pgrx.  README commit
`249d8c1` explains the reasoning: pgrx's value is in safe access
to postgres SPI / SQL surface, which this module doesn't need —
it only talks to the rendezvous API, which is plain C.

---

## Significant design decisions captured along the way

1. **Server does NOT send trace context to the client.** W3C
   propagation is one-way; the client already holds what it sent.
2. **sqlcommenter rejected as the long-term inbound mechanism.**
   Silently incorrect under named prepared statements, and
   actively damages client-side prepared-statement caches that key
   on raw SQL text — see the "Concrete motivation" subsection of
   B.6 in `core-changes.md`.
3. **Generic protocol-headers framework, not a one-off
   `_pq_.traceparent`.** Class-of-problems shape, mirroring
   HTTP/2 / gRPC / AMQP precedent.  Future use-cases (audit,
   log-control flags, IDS hints) slot in without further wire
   changes.
4. **`'M'` as the chosen message byte** (`'H'` was already taken
   for Flush and CopyOutResponse).
5. **GUCs as canonical state for contrib/otel.** Buys parallel-
   worker propagation for free via existing `RestoreGUCState`; no
   bespoke parallel-state plumbing.  Also opens the door to client
   `SET` / `SET LOCAL` as a documented fallback path (with the
   three failure modes documented in `core-changes.md` B.6).
6. **Affirmative `ParameterStatus`** rather than
   absence-of-`NegotiateProtocolVersion` as the libpq "feature is
   available" signal — defends against pgbouncer-class strippers.
7. **contrib/otel deliberately does NOT handle baggage.** W3C
   Baggage is a separate spec with a different audience,
   namespace, and size budget — a sibling `contrib/baggage` is
   the right home for it.
8. **`ParentBased` default sampler.** Honours the propagated
   decision so unsampled traces stay cheap end-to-end.
9. **Configurable sampler-hook invocation policy.** Four regimes
   covering strict W3C, tail-based sampling, lowest-cost
   trust-the-bit, and record-everything debug modes.  Exporters
   pick via `api->set_sampler_policy`.
10. **Versioned rendezvous API for exporters.** Out-of-tree
    exporters plug in without linking against `contrib/otel`
    symbols; the version field lets the contract evolve without
    breaking installed modules (already used once: bump to v2 for
    `set_sampler_policy`).

---

## Open follow-ups

1. **W3C Baggage support** — explicitly out of scope; sibling
   `contrib/baggage` module to be a future piece of work.
2. **Response headers direction (`'h'`)** — the protocol mechanism
   is single-directional today (client → server).  Server →
   client response headers are deferred until a consumer
   materialises.
3. **`tracestate` propagation outbound** — currently stored and
   parallel-propagated but not otherwise threaded.  Will become
   actively used once an outbound-propagation consumer exists.
4. **Push of branches.** `postgres-otel-tracing` is at
   `ringerc/postgres-otel-tracing`; latest commits may be ahead
   of the remote.  Same for parent-repo `main`.
5. **Statement-text and parameter capture as span attributes**,
   with GUC control and size caps — currently spans carry
   headline fields only.
6. **Redaction of sensitive utility statements** (passwords in
   `ALTER ROLE … PASSWORD …`, etc.).
7. **Span links between transaction- and statement-level traces**
   — emit transaction spans on xid/xmin acquisition and link
   statement spans to them.
8. **Submission upstream to `pgsql-hackers`** — the core changes
   and contrib modules are intended for upstream review.

## Todo list status

Empty.  No outstanding tracked task.
