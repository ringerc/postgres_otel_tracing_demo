# Core postgres changes for the OTel trace-context work

This doc summarises the core changes that were necessary for the otel tracing design to work.

> [!NOTE]
> **This is not LLM content**

For specifics of why each change was required and alternatives explored, see [doc/implementation/core-changes-details.txt](../implementation/core-changes-details.txt).

## Generic key/value annotations on `ErrorData`

`ErrorData` is extended with a generic key/value annotation list.
Every log writer surfaces annotations without each writer needing to
know about OpenTelemetry, and any extension that wants to attach
observability metadata to a log line — not just tracing — can do so
through the same mechanism.  Allows for
[*log/trace correlation*](https://opentelemetry.io/docs/specs/otel/logs/#log-correlation)
via *structured log tagging*.

- `ErrorData` gains a `ErrorAnnotation *annotations` list head.
- New `errannot(key, value)` and `errannotf(key, fmt, ...)` calls
  parallel `errmsg()` etc., with the existing
  copy/free/throw plumbing extended to manage the linked list.
- Well-known key constants exported from `elog.h`:
  `ERRANNOT_KEY_TRACE_ID`, `ERRANNOT_KEY_SPAN_ID`,
  `ERRANNOT_KEY_TRACE_FLAGS` so producers share spellings.
- Two new `log_line_prefix` escapes: `%A` (dump all annotations as
  space-separated `key="value"` pairs) and `%{key}A` (emit one
  annotation by name).  Tracing log_line_prefix is
  `%{trace_id}A` / `%{span_id}A` / `%{trace_flags}A`.
- JSON-log: each annotation becomes an additional top-level key.
- CSV-log: one trailing column carrying a JSON-encoded object.
- Top-level JSON-key collisions with core-owned fields are rejected
  and aggregated under the reserved key `pg_rejected_annotations` so
  the collision is diagnosable from the log record without leaking
  the would-be value.

`ereport` emitters populate annotations explicitly via `errannot()`
or via a log hook.

`contrib/otel` uses this to attach the trace context it tracks (under
`trace_id` / `span_id` / `trace_flags`) to log lines via an
`emit_log_hook`.

### Why generic annotations over tracing-specific fields

An earlier iteration of this work added three tracing-specific
fields (`trace_id` / `span_id` / `trace_flags`) and a dedicated
`errtrace()` helper.  That worked for the immediate tracing use case
but didn't generalize: every other observability dimension would
have wanted its own ABI carve-out.  The generic-annotations API
covers tracing today and any future observability metadata (e.g.
tenant IDs, request IDs, user-supplied attributes) without further
core changes.  Cost is one linked-list walk per log line and a
small per-annotation palloc, both negligible compared to the rest
of the log path.

### Why ereport support is necessary

Without core ereport support for structured annotations, an
extension injecting trace context would need to use `errcontext` to
do so.

This has several limitations:

* `CONTEXT` output may be omitted entirely depending on the logging configuration
* Requires reallocations of context strings to append the context, adding overhead
* Requires log-consumers to have postgres-specific log parser logic (and spend extra CPU time) for consumers to parse the trace ID out of the `CONTEXT` string blob; cannot simply pluck out a standard json-log field or csv-log column with standard parsers.

It makes a lot more sense to natively support these annotations.

## Per-message trace context (TraceContext message)

A new wire-level mechanism is added for the client to attach W3C
trace context to the next protocol operation with zero added
round trips.

Tracing requires this for efficient and reliable [OpenTelemetry
trace context propagation](https://opentelemetry.io/docs/concepts/context-propagation/).
The client needs to send a `traceparent` and `tracestate` to the
server alongside the query, out-of-band, without putting them
inside the SQL text (sqlcommenter) or burning a round-trip on
`SET`.

- One new protocol message: `'M'` (`TraceContext`), carrying
  exactly two fixed W3C fields: `traceparent` and `tracestate`
  as NUL-terminated strings.  There is no open key/value entry
  list — the field set is closed and otel-specific.
- A single registered consumer via
  `RegisterTraceContextHandler(apply_cb, clear_cb, ctx)`.  No
  prefix, no handler registry, no dispatch table — only one
  consumer may register; a second registration is an error.
- Context is applied on receipt: `apply_cb(traceparent,
  tracestate, ctx)` fires immediately when the `'M'` is
  received (top-level-command state only).  There is no deferred
  dispatch to Q/P/B/E entry points.
- Scope is **until the next ReadyForQuery (RFQ)**, or until a
  later `'M'` overrides it within the same pipeline.  Core drives
  the clear at RFQ by invoking the consumer's `clear_cb`.  No
  per-binding-target replay — GUC-backed state persists naturally
  across the whole pipeline window.
- Trace context is advisory: a malformed `traceparent` value is
  ignored (the operation proceeds untagged), never an error to the
  client.  A malformed wire frame is still a protocol violation.
- Negotiation is a minor **protocol version bump to 3.3**,
  reusing the standard min/max version negotiation.
  `NegotiateProtocolVersion` handles downgrade for older servers.
  The startup-option opt-in and the affirmative acknowledgement
  `ParameterStatus` from the earlier design are removed.
- One runtime kill-switch GUC: `trace_context_enabled` (default
  on).  A 3.3 client still connects when it is off; `'M'` simply
  errors.  The old size/count GUCs are gone — there is no entry
  list to bound.

### Statement-scope clear hook (PR #4)

`pre_ready_for_query_hook` is added in core, fired by
`PostgresMain` just before each `ReadyForQuery`.  Independently
useful for any extension that needs end-of-command-cycle cleanup.
For trace context specifically, core now drives the clear via
`clear_cb` at RFQ directly, so `contrib/otel` no longer relies
on `pre_ready_for_query_hook` for trace-context scope.

### Why a dedicated TraceContext message is required

Trace context propagation from client to postgres *must* add
minimal performance impact.  Existing strategies are inadequate:

* Can't use trace context as a GUC in the startup packet — breaks
  pooled connections and precludes per-transaction or
  per-statement context.
* Can't efficiently use `SET` / `SET LOCAL` — requires an extra
  round trip per traced unit; `SET LOCAL` cannot clear between
  statements within the same transaction without yet another round
  trip.
* [SQLcommenter](https://google.github.io/sqlcommenter/) — works
  for simple cases but cannot attach context to executions of a
  named prepared statement, and causes cache misses in every
  driver that keys its prepared-statement cache on raw SQL text.

The `'M'` message rides on the same message turn as the operation
it labels; no extra round trip is possible because there is no
extra message.  See
[doc/implementation/core-changes-details.md](../implementation/core-changes-details.md)
for the full failure-mode analysis of each alternative.

## What `contrib/otel` consumes

The `contrib/otel` extension uses these core changes:

- `errannot(...)` + the well-known `ERRANNOT_KEY_TRACE_ID` /
  `ERRANNOT_KEY_SPAN_ID` / `ERRANNOT_KEY_TRACE_FLAGS` constants:
  its `emit_log_hook` attaches the trace context, so
  JSON/CSV/`log_line_prefix` all surface it for free.
- `RegisterTraceContextHandler` with an `apply_cb` and `clear_cb`:
  this is how `otel.traceparent` / `otel.tracestate` arrive via
  the `'M'` message and are cleared at each RFQ boundary.  Core
  drives the clear; `contrib/otel` keeps an internal flag so the
  RFQ clear only resets `'M'`-installed context, never a user's
  `SET otel_api.traceparent`.

The contrib module needs no other core changes. The sampler hook, policy, span emission, sqlcommenter support etc all live entirely in `contrib/otel`.

## Core changes avoided

Notable areas where no core change was required or could be worked around:

- No new hooks for span emission. `contrib/otel` uses existing
  `ExecutorStart_hook`, `ExecutorEnd_hook`, `ProcessUtility_hook`,
  `emit_log_hook`. The whole tracing surface fits inside existing
  extension mechanisms.
- No core-side concept of OpenTelemetry. The `TraceContext`
  message carries closed, otel-owned fields; the extension
  registers the single consumer that interprets them.  Core
  dispatches blindly to whatever consumer registered.
- No major protocol break. The minor version bump to 3.3 reuses
  the existing min/max negotiation and `NegotiateProtocolVersion`
  downgrade path; older clients and servers interoperate.

## See also

The core work has been split into focused upstream PRs that each
stand on their own merits.  Tracing-demo / `contrib/otel` requires
all three core PRs plus PR #1:

* https://github.com/ringerc/postgres/pull/1 — `contrib/otel`
  itself (depends on the three below).
* https://github.com/ringerc/postgres/pull/3 — single-purpose
  `TraceContext` message (`'M'`), apply-on-receipt dispatcher,
  protocol 3.3 negotiation, libpq `PQsetTraceContext` /
  `PQattachTraceContext` / `PQtraceContextAvailable` client API.
* https://github.com/ringerc/postgres/pull/4 —
  `pre_ready_for_query_hook` (independently useful; used by
  consumers that want statement-scope cleanup).
* https://github.com/ringerc/postgres/pull/5 — generic key/value
  annotations on `ErrorData` (`errannot()` / `errannotf()`,
  `%A` / `%{key}A` log_line_prefix escapes).

Earlier iterations of the elog work shipped as
https://github.com/ringerc/postgres/pull/2 (tracing-specific
`errtrace()` + dedicated fields).  That has been superseded by
PR #5's generic annotations approach.

* [doc/implementation/core-changes-details.md](../implementation/core-changes-details.md) for more details on these core changes
* [doc/implementation/otel-trace-context-summary.md](../implementation/otel-trace-context-summary.md) for detailed list of all work across this extension and the core branch, with current status