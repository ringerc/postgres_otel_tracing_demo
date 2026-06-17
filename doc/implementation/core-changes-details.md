# Core changes required by this series

This **LLM-assisted** doc elaborates on
[doc/concepts/core-changes.md](../concepts/core-changes.md) to cover
specifics of each change made, why it was made, etc.

> [!WARNING]
> **LLM-generated material** follows

Each item below is a change to `src/backend`, `src/include`, or
`src/interfaces/libpq` that cannot be done as an out-of-tree
extension using existing hooks. The two contrib modules
(`contrib/otel`, `contrib/otel_exporter`) and the new
`src/test/modules/*` TAP suites are deliberately **not** on this
list — they build on the core surface below and could in principle
live out of tree.

Each "why core" justification has been verified against the
PostgreSQL source (master + this series). Citations are
`file:line` against the postgres submodule.

## A. elog / ereport: structured annotations on log records

- [ ] **`ErrorData` gains an `annotations` list head**
      (`src/include/utils/elog.h`)
      *Compile-time struct layout; extensions cannot add fields, and
      side-state would be invisible to core log writers.  A linked
      list rather than fixed fields so the API generalizes to any
      observability metadata, not just tracing.*

- [ ] **`errannot()` / `errannotf()` helpers + lifecycle in
      `CopyErrorData` / `ThrowErrorData` / `ReThrowError` /
      `FreeErrorDataContents`**
      (`src/backend/utils/error/elog.c`)
      *These are plain functions with no hook points.  The only
      error-handling hook is `emit_log_hook`, which fires at message
      emission — far past the lifecycle path.  Needs the field from
      the previous item anyway.*

- [ ] **Well-known annotation key constants exported from `elog.h`**
      (`ERRANNOT_KEY_TRACE_ID`, `ERRANNOT_KEY_SPAN_ID`,
      `ERRANNOT_KEY_TRACE_FLAGS`)
      *Producers across the tree should agree on spellings; constants
      live with the API to keep that contract enforceable.*

- [ ] **Reserved-key handling: collisions with core-owned JSON top-
      level fields are rejected and aggregated under
      `pg_rejected_annotations`**
      (`src/backend/utils/error/elog.c`)
      *Annotations and JSON log fields share a flat namespace.  Core
      owns the reservation list; an out-of-tree extension cannot
      negotiate against future built-in additions.*

- [ ] **JSON log writer emits each annotation as an additional
      top-level key**
      (`src/backend/utils/error/jsonlog.c`)
      *`emit_log_hook` is observe-only with respect to logging: it
      can *suppress* core's writer by setting
      `edata->output_to_server = false`, but cannot supplement it
      or inject new top-level keys.  Reimplementing the JSON writer
      in a contrib means reimplementing the entire log destination
      plumbing (file rotation, `log_destination`, syslog/eventlog
      routing), and producing a non-standard schema that log-
      aggregation pipelines won't recognize.*

- [ ] **CSV log writer appends one trailing annotations column**
      (`src/backend/utils/error/csvlog.c`)
      *CSV column layout is hardcoded in the writer; no hook lets an
      extension add columns.  Same suppress-and-reimplement trade-off
      as JSON.  Column is JSON-encoded so the format stays stable as
      the annotation surface grows.*

- [ ] **`log_line_prefix` gains `%A` (all annotations) and
      `%{key}A` (single annotation by name)**
      (`src/backend/utils/error/elog.c`, the `log_status_format`
      switch)
      *Prefix formatter dispatches on format letters via a hardcoded
      switch with no extension hook.  Unknown letters are silently
      ignored.*

## B. Wire protocol: single-purpose TraceContext message, zero added round trips

- [ ] **New protocol message byte `'M'` (`TraceContext`), carrying
      two fixed W3C fields: `traceparent` and `tracestate`**
      (`src/backend/tcop/postgres.c`, the `PostgresMain`
      message-byte switch; `src/include/libpq/protocol.h`;
      `src/backend/libpq/trace_context.c`)
      *The dispatch is a switch on `firstchar` with a `default:` that
      `ereport(ERROR, ...)`s on unknown bytes. No "register new wire
      message type" hook exists, and there cannot be one without
      protocol-version bookkeeping that only core does. The message
      carries exactly two NUL-terminated strings — `traceparent` and
      `tracestate` — not an open key/value entry list. This is the
      change that enables per-message, zero-round-trip context
      delivery.*

  **Concrete motivation — sqlcommenter vs. client-side
  prepared-statement caches.** The widely-cited alternative to a
  wire-level message is
  [**sqlcommenter**](https://google.github.io/sqlcommenter/), which
  injects trace context as a SQL-comment prefix
  (`/*traceparent='00-{trace-id}-{span-id}-{flags}'*/ SELECT …`)
  and is currently the standard recommendation in the OpenTelemetry
  ecosystem for database clients that lack protocol-level
  propagation. It interacts badly with client drivers that cache
  prepared statements per connection keyed on the raw SQL text — a
  common pattern used to amortise Parse/Describe round-trips.
  Because the trace and span IDs change on every call, the
  sqlcommenter-prefixed SQL text is unique per call, so every
  traced query is a guaranteed cache miss. The mechanical
  consequences:

  1. Every traced query forces a fresh server-side `PREPARE`,
     adding exactly the Parse/Describe round-trip the cache exists
     to amortise.
  2. Once the cache is full (drivers commonly use a bounded LRU),
     every miss evicts an entry. The evicted entry is by definition
     a genuinely-reusable statement; the new entry is single-use.
     The cache effectively stops working for *untraced* queries on
     the same connection — collateral damage.
  3. Each eviction triggers a server-side `DEALLOCATE`, producing
     continuous churn in `pg_prepared_statements`.
  4. Explicit transactions are not exempt: drivers typically
     delegate `tx.Exec` to the same exec path, so the failure mode
     applies inside `BEGIN`/`COMMIT` blocks as well.

  Available out-of-tree workarounds for the cache-busting problem
  all have material costs: bypassing the prepared-statement cache
  for traced call sites surrenders the cache's benefit on exactly
  the queries where tracing is most valuable (the hot path), and
  switching to simple-query mode loses binary parameter encoding.
  The other commonly-proposed alternative — propagating context
  via `SET` / `SET LOCAL` — has its own set of failure modes,
  covered in the next subsection.

  Only a per-message, out-of-band wire message attaches context
  without mutating SQL text and without adding a round trip.
  This is the failure mode that
  [sqlcommenter](https://google.github.io/sqlcommenter/) cannot
  escape on any driver that caches prepared statements by SQL
  string — i.e., effectively all modern PostgreSQL drivers that
  use the extended-query protocol with a statement cache enabled
  by default.

  **Concrete motivation — `SET` / `SET LOCAL` as a propagation
  mechanism.** The other commonly-proposed out-of-tree mechanism
  is to deliver trace context via a GUC, set per-statement with a
  preceding `SET LOCAL otel.traceparent = '…'` (or session-wide
  with `SET`). This appears attractive — no SQL-text mutation, no
  new wire mechanism, works with existing protocols today — but
  has three independent failure modes:

  1. **Extra round trip per traced unit, unless the client
     pipelines.** Setting the GUC requires a separate statement
     (`SET LOCAL …` or `SELECT set_config(…)`) sent before the
     traced operation. On a sequential client, that is an
     additional client → server → response round trip per traced
     statement (or, at minimum, per traced transaction).
     Pipelining or batching modes — libpq pipeline mode, JDBC
     `addBatch`, pgx pipeline mode — can amortise this, but
     pipelining is not universally available across drivers and
     requires the application to be rewritten against the
     event-loop-shaped pipeline API, with consequent changes to
     error handling. Driver-level support is uneven: some drivers
     offer it, some don't, and middleware/wrapper-level
     instrumentation generally cannot introduce pipelining without
     rewriting the application's data-access path. The `'M'`
     message rides on the same message turn as the operation it
     labels; no extra round trip is possible because there is no
     extra message.

  2. **Cannot label different executions of the same portal with
     different trace contexts.** The extended-query protocol
     permits multiple `Execute` messages against a portal
     (including the unnamed portal `""`) within a single
     transaction. `SET LOCAL` is transaction-scoped — once set, it
     cannot vary between successive Executes in the same
     transaction without another `SET LOCAL` (each adding its own
     round trip per (1) above). The same problem applies to
     pipelined or batched workloads that interleave multiple
     distinct traced operations on the same connection inside one
     transaction. The `'M'` message is per-pipeline-window, so
     each window can carry its own distinct trace context without
     disturbing transaction state.

  3. **No reliable per-statement clearing.** `SET LOCAL` persists
     to end-of-transaction; `SET` persists to end-of-session.
     Neither clears at statement end. For statement-level
     tracing — where each statement should have its own trace
     context, potentially with a different parent span and a
     different sampling decision — there is no protocol-level
     mechanism to clear the GUC at the boundary between
     statements: it must be either explicitly cleared (another
     round trip and another opportunity for application code to
     forget) or implicitly overwritten by the next traced
     statement. If the application forgets to clear and the next
     statement is *untraced*, that statement silently inherits the
     previous trace's context — a correctness failure that
     produces miscorrelated spans in the trace backend. The `'M'`
     message's until-RFQ lifecycle is a protocol invariant: core
     clears at every ReadyForQuery boundary via `clear_cb`, so
     there is no leftover-state failure mode by construction.

  Taken together, sqlcommenter and `SET`/`SET LOCAL` cover the two
  paths an extension-only solution can take to deliver trace
  context to the server, and both are unworkable for production
  tracing of any non-trivial workload. The protocol-message
  approach is what closes that gap.

- [ ] **Protocol version 3.3 negotiation**
      (`src/backend/tcop/backend_startup.c`,
      `src/include/libpq/pqcomm.h`)
      *`PG_PROTOCOL_LATEST` is bumped to `PG_PROTOCOL(3,3)`.
      Existing min/max negotiation clamps `FrontendProtocol` and
      fires `NegotiateProtocolVersion` on a higher-minor request,
      handling downgrade transparently. The per-message gate checks
      `PG_PROTOCOL_MINOR(FrontendProtocol) >= 3`. The startup-option
      opt-in branch and the affirmative acknowledgement
      `ParameterStatus` from the earlier design are removed — the
      minor version conveys availability without them.*

- [ ] **Apply-on-receipt dispatch: parse `'M'` and invoke
      `apply_cb(traceparent, tracestate, ctx)` immediately**
      (`src/backend/libpq/trace_context.c`,
      `src/backend/tcop/postgres.c` top-level message switch)
      *Context is applied the moment the message is received (in
      top-level-command state only). There is no deferred replay at
      Q/P/B/E entry points — the GUC-backed consumer state persists
      naturally until the RFQ clear. Protocol violations (`'M'` in
      wrong state, `'M'` on a <3.3 connection) are `ERROR`, not
      `FATAL`.*

- [ ] **`pre_ready_for_query_hook` for end-of-cycle cleanup (PR #4)**
      (`src/backend/tcop/postgres.c`, `src/include/tcop/tcopprot.h`)
      *Fires just before each `ReadyForQuery`. Independently useful
      for any extension that needs end-of-command-cycle teardown.
      For trace context, core now drives the RFQ clear directly via
      `clear_cb`; `contrib/otel` no longer relies on this hook for
      trace-context scope.*

- [ ] **`RegisterTraceContextHandler(apply_cb, clear_cb, ctx)`
      extension API — single consumer, no registry**
      (`src/include/libpq/trace_context.h`,
      `src/backend/libpq/trace_context.c`)
      *The single registered consumer's `apply_cb` fires on each
      `'M'` receipt; `clear_cb` fires from the ReadyForQuery
      emission path. Only one consumer may register — a second call
      is an error. No prefix argument, no scope enum, no
      per-binding-target replay.*

- [ ] **`trace_context_enabled` GUC (single kill-switch)**
      *Extensions can define GUCs, but the feature gate must be
      evaluated where `'M'` comes off the wire. There are no
      entry-count or byte-size GUCs — the payload is two fixed-shape
      strings bounded by `PQ_SMALL_MESSAGE_LIMIT`.*

## C. libpq: client-side API and protocol participation

- [ ] **`PQsetTraceContext` / `PQattachTraceContext` /
      `PQtraceContextAvailable` API**
      (`src/interfaces/libpq/fe-trace-context.c` etc.)
      *libpq has no plugin/hook/callback model anywhere in
      `src/interfaces/libpq/`. An external library cannot add
      functions to libpq's API, intercept `PQsend*`, or carry
      connection-lifetime state without forking the library.*
      - `PQsetTraceContext(conn, traceparent, tracestate)` — arms
        the connection; libpq re-emits one `'M'` at the start of
        each subsequent pipeline until disarmed (pass `traceparent
        = NULL` to disarm). Refuses unless negotiated protocol ≥
        3.3.
      - `PQattachTraceContext(conn, traceparent, tracestate)` —
        one-shot: emits one `'M'` before the next message; not
        re-sent after that pipeline's RFQ.
      - `PQtraceContextAvailable(conn)` — returns 1 when
        `PG_PROTOCOL_MINOR(conn->pversion) >= 3` (negotiated at
        connect time).

- [ ] **libpq requests protocol 3.3 in StartupMessage**
      (`src/interfaces/libpq/fe-connect.c`)
      *StartupMessage construction is hardcoded in libpq's
      connection-setup path; no injection point. The bespoke
      `conn->headersAvailable` flag is removed; availability is
      determined by `conn->pversion` after negotiation.*

## D. Documentation

- [ ] **SGML updates: `log_line_prefix` table, CSV column list,
      JSON keys table, libpq function reference, protocol-message
      reference**
      *Documents the changes in A–C; lives with them.*

---

## Not on this list (could be out of tree)

- `contrib/otel` — `RegisterTraceContextHandler` consumer, GUCs
  for trace context (`otel_api.traceparent` / `.tracestate`),
  `emit_log_hook` integration, span emission via
  `ExecutorStart`/`ExecutorEnd`/`ProcessUtility_hook`, sampler
  hook, JSON log emitter, versioned rendezvous API.
- `contrib/otel_exporter` — file emitter.
- Parallel-worker propagation of trace context — falls out of
  existing `RestoreGUCState` because `contrib/otel` stores state in
  GUCs; no core change needed.
- Span emission hooks themselves (`ExecutorStart`, `ExecutorEnd`,
  `ProcessUtility_hook`, `emit_log_hook`) — already in core.
- Per-transaction and per-session trace-context clearing — the
  consumer uses an internal flag to distinguish `'M'`-installed
  context from user `SET` context; `clear_cb` resets only the
  former at RFQ. Longer-lived (`SET` / `SET LOCAL`) semantics are
  managed by the GUC machinery, not by core `clear_cb`.
