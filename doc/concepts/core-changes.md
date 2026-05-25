# Core postgres changes for the OTel trace-context work

This doc summarises the core changes that were necessary for the otel tracing design to work.

> [!NOTE]
> **This is not LLM content**

For specifics of why each change was required and alternatives explored, see [doc/implementation/core-changes-details.txt](../implementation/core-changes-details.txt).

## W3C Trace Context fields on `ErrorData`

`ErrorData` is extended with fields for OpenTelemetry trace context. Every log writer emits it without each one needing to
know about OpenTelemetry. Fields can be populated with trace context by a hook or explicitly set by the `ereport` caller. Allows for [*log/trace correlation*](https://opentelemetry.io/docs/specs/otel/logs/#log-correlation) via *structured log tagging*.

- `ErrorData` gains three fields: `trace_id`, `span_id`,
  `trace_flags` (all `char *`, NULL = unset).
- New `errtrace()` call producers use, parallel to `errmsg()` etc.,
  with the existing copy/free/throw plumbing extended to manage
  the new fields.
- Two new `log_line_prefix` escapes: `%T` (trace_id), `%S`
  (span_id).
- JSON-log and CSV-log writers extended to emit the three fields.

`ereport` emitters populate trace context explicitly via
`errtrace(...)` or via a log hook.

`contrib/otel` uses this to inject the trace context it tracks into logs via an `emit_log_hook`.

This is a fairly light-weight change.

### Why ereport support is necessary

Without core ereport support for trace-id and span-id, an extension injecting trace-ids would need to use `errcontext` to do so.

This has several limitations:

* `CONTEXT` output may be omitted entirely depending on the logging configuration
* Requires reallocations of context strings to append the context, adding overhead
* Requires log-consumers to have postgres-specific log parser logic (and spend extra CPU time) for consumers to parse the trace ID out of the `CONTEXT` string blob; cannot simply pluck out a standard json-log field or csv-log column with standard parsers.

It makes a lot more sense to natively support these fields.

### Alternatives to new log fields

Instead of adding specific fields for a trace-id and span-id, there is some argument for extending `ereport` with a key/value mapping of additional structured fields.

This would be more generic, but also significantly more intrusive. It might be difficult to implement this with minimal memory allocations and CPU overhead, which is crucial for a hot-path like logging. Especially on the before-emitter side where log level configuration will often cause the constructed log to be discarded unused.

## Per-message protocol headers

A new wire-level mechanism is added for the client to attach arbitrary key/value metadata to the next protocol message. This message headers mechanism is *not* specific to use by tracing, it provides a generic headers scheme to allow separation of cross-cutting concerns.

Tracing requires these headers for efficient and reliable [opentelemetry trace context propagation](https://opentelemetry.io/docs/concepts/context-propagation/). The client needs a zero-added-round-trips mechanism to send a trace-id and parent-span-id to the server along with the query to execute. Existing workarounds like sqlcommenter, `SET`/`SET LOCAL` GUC-based trace context transport, etc all have various performance issues and corner cases that make them inefficient and/or unreliable.

This headers design provides prefix-routed dispatch to in-backend handlers so different extensions can claim different header namespaces for their use.

- One new protocol message: A `'M'` (`RequestHeaders`) message.
- A new extension API: register interest in a key prefix at
  module init, with longest-prefix-wins dispatch. Three scopes —
  STATEMENT / TRANSACTION / SESSION — define when the handler's
  accumulated effect is torn down. Scope boundaries are wired
  internally (xact callback, `on_proc_exit`, pre-`ReadyForQuery`).
- Startup-packet negotiation via the existing `_pq_.` namespace,
  with the server-side feature gated by a GUC. Failed
  negotiation surfaces via `NegotiateProtocolVersion`.
- An additional `ParameterStatus` row keyed `protocol_features`
  is emitted in the initial burst, advertising features actually
  negotiated end-to-end. Ensures cross-version negotiation safety. Also defends against the proxy false-positive
  where an intermediary strips the opt-in but doesn't relay
  `NegotiateProtocolVersion` as the presence of `protocol_features`
  is the client's only reliable signal that the feature really
  works through whatever proxies are in the path.
- Three GUCs: server-side feature toggle (default on), max
  entries per message (default 64), max body bytes (default
  4 KiB, hard-capped at 10 KiB by the v3 small-message limit).
  Oversize messages are protocol violations (open to discussion whether they should be truncated or dropped instead)
- The `'M'` message is a prefix on the *next* operation, not
  its own query pipeline phase, so it deliberately does not flip
  extended-query mode.

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

- `errtrace(...)` + the new `ErrorData` fields: its
  `emit_log_hook` populates them, so JSON/CSV/`log_line_prefix`
  all get trace IDs for free.
- The headers-registration API: this is how `otel.traceparent`
  arrives via the `'M'` message and is auto-cleared at
  COMMIT/ROLLBACK.

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

* https://github.com/ringerc/postgres/pull/1 - postgres branch providing the upstream core work and `contrib/otel` API extension
* [doc/implementation/core-changes-details.md](../implementation/core-changes-details.md) for more details on these core changes
* [doc/implementation/otel-trace-context-summary.md](../implementation/otel-trace-context-summary.md) for detailed list of all work across this extension and the core branch, with current status