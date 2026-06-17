# OpenTelemetry trace context in PostgreSQL — work summary

See also:

* [doc/concepts/core-changes.md](../concepts/core-changes.md) for a high level summary of the changes made to postgres core to support this work, and why they were made; and
* [doc/implementation/core-changes-details.md](core-changes-details.md) for the itemised list of core-only changes with justifications.

> [!WARNING]
> **LLM-generated material follows**

Snapshot of the work to date on adding W3C / OpenTelemetry trace
context support to PostgreSQL.

Branches:

- **postgres submodule** (postgres patches): `postgres-otel-tracing`
  on `ringerc/postgres` (PR #1), 35-commit series.  Sits on top of
  an octopus merge of three focused core PRs (#3, #4, #5) — see
  list below.
- **`ringerc/postgres_otel_tracing_demo`** (Rust out-of-tree
  exporter): `main`.

Test coverage across both repos: **all targeted TAP subtests
passing** across the suites — `test_trace_context`,
`libpq_trace_context`, `contrib/otel`, `otel_test_exporter`,
`contrib/otel_postgres_tracing`.

---

## (a) Core PostgreSQL changes

The core work has been split into three focused upstream PRs
against `ringerc/postgres`, each of which stands on its own
merits.  All three are prerequisites for PR #1.

| PR | Branch | Content |
|----|--------|---------|
| [#3](https://github.com/ringerc/postgres/pull/3) | `core-trace-context` | Single-purpose `TraceContext` message (`'M'`), apply-on-receipt dispatcher, protocol 3.3 negotiation, `RegisterTraceContextHandler` extension API, libpq `PQsetTraceContext` / `PQattachTraceContext` / `PQtraceContextAvailable` client API. |
| [#4](https://github.com/ringerc/postgres/pull/4) | `pre-ready-for-query-hook` | `pre_ready_for_query_hook` fired by `PostgresMain` just before each `ReadyForQuery`.  Independently useful; core now drives the trace-context RFQ clear via `clear_cb` directly, so `contrib/otel` no longer relies on this hook for trace-context scope. |
| [#5](https://github.com/ringerc/postgres/pull/5) | `core-elog-annotations` | Generic key/value annotations on `ErrorData` via `errannot()` / `errannotf()`; `%A` and `%{key}A` `log_line_prefix` escapes; JSON/CSV log surfacing. |

An earlier elog iteration shipped as
[PR #2](https://github.com/ringerc/postgres/pull/2)
(tracing-specific `errtrace()` + dedicated `trace_id` / `span_id` /
`trace_flags` fields on `ErrorData`).  That has been superseded by
PR #5's generic annotations approach — covers tracing today and
any future observability dimension without further ABI carve-outs.

### Functional content

1. **elog / ereport structured annotations (PR #5).** `ErrorData`
   gains an `ErrorAnnotation *annotations` list head; new
   `errannot(key, value)` / `errannotf(key, fmt, ...)` helpers;
   full lifecycle plumbing through `CopyErrorData`,
   `ThrowErrorData`, `ReThrowError`, `FreeErrorDataContents`.
   Well-known key constants `ERRANNOT_KEY_TRACE_ID` /
   `ERRANNOT_KEY_SPAN_ID` / `ERRANNOT_KEY_TRACE_FLAGS` so
   producers share spellings.  Reserved-key handling for
   collisions with core-owned JSON fields, surfaced through
   `pg_rejected_annotations` so the rejection is diagnosable from
   the log record without leaking the would-be value.

   JSON log writer emits each annotation as an additional
   top-level key; CSV log gains one trailing JSON-encoded
   annotations column; `log_line_prefix` gains `%A` (all
   annotations) and `%{key}A` (single annotation by name).  No
   server→client wire-side emission — W3C propagation is one-way
   by design.

2. **Single-purpose TraceContext message (PR #3).** New
   wire-protocol message `'M'` / `TraceContext` carrying exactly
   two fixed W3C fields: `traceparent` and `tracestate` as
   NUL-terminated strings (no open key/value entry list).
   Negotiated via a minor protocol version bump to 3.3 using the
   standard min/max negotiation; `NegotiateProtocolVersion` handles
   downgrade for older servers.  The startup-option opt-in and the
   affirmative acknowledgement `ParameterStatus` from the earlier
   design are removed.

   Extension API `RegisterTraceContextHandler(apply_cb, clear_cb,
   ctx)` — single consumer, no prefix, no registry.  `apply_cb`
   fires on receipt of each `'M'` (top-level-command state only);
   `clear_cb` fires from the ReadyForQuery emission path in core.
   Trace context is advisory: a malformed `traceparent` value is
   silently ignored (operation proceeds untagged); a malformed
   wire frame is a protocol violation.  A single kill-switch GUC
   `trace_context_enabled` (default on); no size/count GUCs.

3. **`pre_ready_for_query_hook` (PR #4).** Fired just before each
   `ReadyForQuery`.  Independently useful for any extension that
   needs end-of-command-cycle teardown.  For trace context, core
   now drives the RFQ clear directly via `clear_cb`; `contrib/otel`
   no longer relies on this hook for trace-context scope.

4. **libpq client API (PR #3).** `PQsetTraceContext` /
   `PQattachTraceContext` / `PQtraceContextAvailable`.
   - `PQsetTraceContext` arms the connection: libpq re-emits one
     `'M'` at the start of each subsequent pipeline until
     disarmed.  Pass `traceparent = NULL` to disarm.
   - `PQattachTraceContext` is one-shot: emits one `'M'` before
     the next pipeline, then stops.
   - `PQtraceContextAvailable` returns 1 when the negotiated
     protocol is ≥ 3.3 (`conn->pversion`); the bespoke
     `headersAvailable` flag is removed.  SGML documentation
     added.

### Test coverage (core)

- `test_trace_context/001_trace_context` — wire-level negotiation
  (3.3 vs. 3.2-capped downgrade), apply-on-receipt, until-RFQ
  scope, mid-window override, `SET otel_api.traceparent` survives
  RFQ clear, advisory malformed-value handling.
- `libpq_trace_context/001_libpq_trace_context` —
  `PQtraceContextAvailable` both modes, `PQsetTraceContext` armed
  re-emit across RFQ boundaries, `PQattachTraceContext` one-shot,
  disarm, and the feature-disabled-server fallback path.

### Why these had to be in core, not contrib

Itemised with citations against the postgres source in
[../concepts/core-changes.md](../concepts/core-changes.md).
The short version: no extension hook exists for the `ErrorData`
annotation list, for top-level keys in the JSON / CSV log writers,
for `log_line_prefix` format letters, for new wire-protocol
message types, for the wire-protocol round-trip boundary that
backs statement-scope cleanup, for protocol-version negotiation
in startup, or for libpq's StartupMessage / `PQsend*` paths.

---

## (b) `contrib/otel`

The OpenTelemetry consumer of the headers mechanism, in
`postgres-otel-tracing` (PR #1) on top of the three core PRs.
`contrib/otel/` + `contrib/otel_postgres_tracing/` (the
query-tracing module split off in Phase 4).

### Functional layers

1. **Trace-context ingestion.** Registers a single
   `RegisterTraceContextHandler` consumer (`otel_trace_context_apply_cb`
   / `otel_trace_context_clear_cb`).  `traceparent` / `tracestate`
   arriving in `'M'` messages land in the custom GUCs
   `otel_api.traceparent` and `otel_api.tracestate` via
   `set_config_option`; `clear_cb` resets them at each RFQ
   boundary, but only for `'M'`-installed context — an internal
   flag prevents clobbering a user's `SET otel_api.traceparent`.
   These GUCs are `PGC_USERSET`, so client-side `SET` / `SET LOCAL`
   is also supported as a fallback for clients that lack `'M'`
   support — with the round-trip / scope / pooler-leak caveats
   documented inline in `contrib/otel/otel.c`.
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
   `$(pg_config --includedir-server)/extension/otel_api/`.

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
3. **Single-purpose `TraceContext` message, not a generic
   key/value headers framework.** The earlier generic-headers
   design accreted a registry, four lifecycle scopes, proxy-flag
   lattice, and replay-at-every-binding-target machinery in service
   of generality no second consumer has yet asked for.  The
   decision was reversed: collapse to a closed, otel-owned field
   set (`traceparent`, `tracestate`), one scope, no registry.  A
   generic mechanism can be reintroduced later if a second consumer
   materialises, without the cost of shipping it now.
4. **`'M'` as the chosen message byte** (`'H'` was already taken
   for Flush and CopyOutResponse).
5. **GUCs as canonical state for contrib/otel.** Buys parallel-
   worker propagation for free via existing `RestoreGUCState`; no
   bespoke parallel-state plumbing.  Also opens the door to client
   `SET` / `SET LOCAL` as a documented fallback path (with the
   three failure modes documented in `core-changes.md` B.6).
6. **Protocol version 3.3 as the availability signal** rather than
   a bespoke `ParameterStatus` or startup-option opt-in.  Reuses
   standard min/max negotiation; `PQtraceContextAvailable` checks
   `conn->pversion >= 3.3` rather than a separate flag.
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
