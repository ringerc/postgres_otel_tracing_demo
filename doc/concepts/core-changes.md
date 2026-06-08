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

## Per-message protocol headers

A new wire-level mechanism is added for the client to attach arbitrary key/value metadata to the next protocol message. This message headers mechanism is *not* specific to use by tracing, it provides a generic headers scheme to allow separation of cross-cutting concerns.

Tracing requires these headers for efficient and reliable [opentelemetry trace context propagation](https://opentelemetry.io/docs/concepts/context-propagation/). The client needs a zero-added-round-trips mechanism to send a trace-id and parent-span-id to the server along with the query to execute. Existing workarounds like sqlcommenter, `SET`/`SET LOCAL` GUC-based trace context transport, etc all have various performance issues and corner cases that make them inefficient and/or unreliable.

This headers design provides prefix-routed dispatch to in-backend handlers so different extensions can claim different header namespaces for their use.

- One new protocol message: A `'M'` (`RequestHeaders`) message.
- A new extension API: register interest in a key prefix at
  module init, with longest-prefix-wins dispatch.  The core
  dispatcher is intentionally lifecycle-free — it routes each
  `(key, value)` entry to the matching extension's `set_cb` and
  does nothing else.  Each extension owns its own state lifecycle
  (transaction / session / statement scope) via the appropriate
  PostgreSQL hook: `RegisterXactCallback`, `on_proc_exit`, or
  `pre_ready_for_query_hook` respectively.  See "Statement-scope
  clear hook" below for the new core hook that supports the
  third option.
- Dispatch is *deferred*: a received `'M'` is parsed and stashed,
  and the registered `set_cb`s fire at the start of the next
  Query / Parse / Bind / Execute — not at `'M'` receipt time.
  This binds a handler error to the SQL operation those headers
  were intended to prefix, so a malformed value or a misbehaving
  handler fails that operation rather than producing a
  standalone error and letting the operation run with
  half-applied state.
- Atomic frame parsing: an `'M'` with trailing garbage or a
  truncated entry is rejected as a whole before any handler runs.
- Startup-packet negotiation via the existing `_pq_.` namespace,
  with the server-side feature gated by a GUC. Failed
  negotiation surfaces via `NegotiateProtocolVersion`. Only
  `_pq_.headers=1` opts in; other values are reported as
  unrecognized so future value-based semantics remain available.
- An additional `ParameterStatus` row keyed `protocol_features`
  is emitted in the initial burst, advertising features actually
  negotiated.  Defends against the proxy false-positive where an
  intermediary strips the opt-in but doesn't relay
  `NegotiateProtocolVersion`: presence of `protocol_features`
  signals that the startup opt-in reached a supporting server.
  Intermediaries must still relay `'M'` messages for the feature
  to work end-to-end.
- Three GUCs: server-side feature toggle (default on), max
  entries per message (default 64), max bytes per single
  `(key, value)` entry (default 4 KiB).  Setting either cap to 0
  refuses the feature at handshake.  Oversize messages are
  protocol violations (open to discussion whether they should be
  truncated or dropped instead).
- The `'M'` message is a prefix on the *next* operation, not
  its own query pipeline phase, so it deliberately does not flip
  extended-query mode.

### Statement-scope clear hook

`pre_ready_for_query_hook` is added in core, fired by
`PostgresMain` just before each `ReadyForQuery`.  Independently
useful for any extension that needs end-of-command-cycle cleanup,
and used by header consumers that want statement-scope semantics
(the wire-protocol round-trip is the right boundary; not
`ExecutorEnd_hook`, which would fire mid-cycle for a
multi-statement simple Query).

A server-to-client response-headers message may also be added to provide a backchannel; potentially useful for e.g. server informing client when an xid is allocated; extension sending client query performance stats; etc.

The headers mechanism is the bigger core change required.

### Why headers are required

Trace context propagation from client -> postgres *must* add minimal performance impact, otherwise tracing will be enabled and used in production so it's available when required.

Adding new client/server round-trips excessively degrades query execution latency and TPS, especially when executing many small unbatched queries or where network latency is high (e.g. cloud databases).

Context propagation cannot be reliably and efficiently accomplished using existing strategies such as GUC-based propagation or SQL comment parsing:

* Can't use trace context as GUC injected in the startup packet, as this will break tracing when client-side or proxy based connection poolers are in use. It would prevent transaction-level and statement-level context entirely.
* Can't efficiently use `SET` or `SET LOCAL` for a trace context GUC because these require an added client/server round-trip. Particularly problematic if using statement-level trace contexts as is otel convention. Also has issues with ensuring that context is reliably cleared on error.
* Can't use extension-exposed SQL-callable functions to set/clear trace context for same reasons as above, plus more downsides around error handling etc. Particularly problematic if function calls are required to bracket real client SQL including after-statement calls.
* Can't reasonably evade the added round trips by wrapping client queries in `DO` blocks or wrapper PL/PgSQL funcs that take a SQL string + params: breaks prepared statement use, unsuitable for some utility statements, requires extensive and intrusive app changes, significant performance overhead, `DO` requires insecure parameter handling.
* [SQLcommenter](https://google.github.io/sqlcommenter/)'s approach of injecting a trace context as a `-- comment` appended to query-text can work in simple cases, but:
  * It cannot associate executions of a protocol-level prepared statement with a trace context, as there is no query-text to inject a comment into; and
  * It breaks some client-side prepared statement pools/caches e.g. `pgx v5`'s `QueryExecModeCacheStatement`

Of these approaches, the sqlcommenter approach and the `SET` / `SET LOCAL` GUC-based trace context transport are the most viable.

If the added `SET` command is bundled into a [pipeline](https://www.postgresql.org/docs/current/libpq-pipeline-mode.html) with the real query it can avoid adding a round trip - but this requires client driver support, extensive client app changes and added complexity to adopt pipelining. Similar issues apply with bundling the `SET` / `SET LOCAL` into a batch for drivers that support this, and batches often have extra limitations on supported statement types.

For `sqlcommenter`, it's mostly usable if the app is willing to forgo the benefit of reusing server-side prepared statements. It can still use protocol-level bind parameters, so there is no security impact, just a performance loss. But a key requirement for successful otel tracing adoption is to achieve _negligible performance cost_, and to make adoption very low-effort.

### Alternatives to adding header messages

A more tracing-specific `TraceContext` protocol message could be added. This doesn't seem worth it though; the complexity cost of a generic headers mechanism isn't much greater, but the potential benefits increase much more than the cost.

Existing messages could be extended with headers, instead of adding a new message type. But this would require a major, breaking new protocol version - which isn't easily justified when there is a reasonably compatible, low-risk alternative.

## What `contrib/otel` consumes

The `contrib/otel` extension uses these core changes:

- `errannot(...)` + the well-known `ERRANNOT_KEY_TRACE_ID` /
  `ERRANNOT_KEY_SPAN_ID` / `ERRANNOT_KEY_TRACE_FLAGS` constants:
  its `emit_log_hook` attaches the trace context, so
  JSON/CSV/`log_line_prefix` all surface it for free.
- The headers-registration API plus its own `RegisterXactCallback`:
  this is how `otel.traceparent` arrives via the `'M'` message
  and is cleared at top-level COMMIT/ROLLBACK.  The lifecycle is
  owned by `contrib/otel`, not by the core dispatcher.

The contrib module needs no other core changes. The sampler hook, policy, span emission, sqlcommenter support etc all live entirely in `contrib/otel`.

## Core changes avoided

Notable areas where no core change was required or could be worked around:

- No new hooks for span emission. `contrib/otel` uses existing
  `ExecutorStart_hook`, `ExecutorEnd_hook`, `ProcessUtility_hook`,
  `emit_log_hook`. The whole tracing surface fits inside existing
  extension mechanisms.
- No core-side concept of OpenTelemetry. The protocol-headers
  mechanism is deliberately namespace-agnostic; any prefix
  (`baggage.`, `obs.`, vendor-specific) could register a handler, and the header mechanism is usable for more than just tracing/opentelemetry.
- No protocol version bump. The opt-in piggybacks on the
  existing `_pq_.` namespace; the new message type is gated by
  negotiation and rejected as a protocol violation otherwise.

## See also

The core work has been split into focused upstream PRs that each
stand on their own merits.  Tracing-demo / `contrib/otel` requires
all three core PRs plus PR #1:

* https://github.com/ringerc/postgres/pull/1 — `contrib/otel`
  itself (depends on the three below).
* https://github.com/ringerc/postgres/pull/3 — per-message
  protocol headers (`'M'` / `RequestHeaders`), deferred-apply
  dispatcher, libpq `PQattachHeader` client API.
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