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

## B. Wire protocol: per-message trace context, zero added round trips

- [ ] **New protocol message byte `'M'` (RequestHeaders), carrying
      namespaced `(key, value)` entries**
      (`src/backend/tcop/postgres.c:4868`, the `PostgresMain`
      message-byte switch; `src/include/libpq/protocol.h`)
      *The dispatch is a switch on `firstchar` with a `default:` that
      `ereport(FATAL, ...)`s on unknown bytes (postgres.c:5148). No
      "register new wire message type" hook exists, and there cannot
      be one without protocol-version bookkeeping that only core
      does. This is the change that enables per-message,
      zero-round-trip context delivery.*

  **Concrete motivation — sqlcommenter vs. client-side
  prepared-statement caches.** The widely-cited alternative to a
  wire-level header is
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

  Only a per-message, out-of-band header in the wire protocol
  attaches context without mutating SQL text and without adding a
  round trip. This is the failure mode that
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
     header rides on the same message turn as the operation it
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
     transaction. The `'M'` header is per-message, so each Execute
     can carry its own distinct trace context without disturbing
     transaction state.

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
     header has per-message lifecycle clearing as a protocol
     invariant: there is no leftover-state failure mode by
     construction.

  Taken together, sqlcommenter and `SET`/`SET LOCAL` cover the two
  paths an extension-only solution can take to deliver trace
  context to the server, and both are unworkable for production
  tracing of any non-trivial workload. The protocol-message
  approach is what closes that gap.

- [ ] **`_pq_.headers=1` startup-option negotiation**
      (`src/backend/tcop/backend_startup.c:807`, the `_pq_.*` arm of
      `ProcessStartupPacket`)
      *Unknown `_pq_.*` options are collected into
      `unrecognized_protocol_options` and later reported via
      `NegotiateProtocolVersion` — by design, with no extension hook
      anywhere in the path. `shared_preload_libraries` extensions
      load before `ProcessStartupPacket` runs but have no
      registration point inside it.*

- [ ] **Affirmative `protocol_features` `ParameterStatus` at startup**
      (`src/backend/tcop/postgres.c:4407`,
      `BeginReportingGUCOptions` / `SendProtocolFeaturesParameterStatus`)
      *An extension `GUC_REPORT` GUC would be advertised at startup —
      but blindly, with no way to condition the value on the result
      of `_pq_.*` negotiation, which the extension cannot observe
      (see previous item). The whole point of the ack is to defend
      against pgbouncer-class proxies that silently strip the opt-in
      from `StartupMessage`, which requires the server to actually
      respond to negotiation, not just to a static GUC default.*

- [ ] **`pre_ready_for_query_hook` for statement-scope cleanup**
      (`src/backend/tcop/postgres.c`, `src/include/tcop/tcopprot.h`)
      *Fires just before each `ReadyForQuery`.  The wire-protocol
      command-cycle boundary is the right granularity for header
      effects that should not leak past one round-trip, and it
      cannot be expressed by existing per-statement hooks
      (`post_parse_analyze_hook`, `ExecutorEnd_hook`,
      `ProcessUtility_hook`) — those fire mid-cycle for a
      multi-statement simple Query.  Independently useful for any
      extension that needs end-of-command-cycle teardown.*

- [ ] **Deferred dispatch: parse `'M'` on receipt, fire `set_cb`s
      at the start of the next Query / Parse / Bind / Execute**
      (`src/backend/libpq/protocol_headers.c`,
      `src/backend/tcop/postgres.c` Q/P/B/E case entries)
      *Binds a handler error to the SQL operation those headers
      were intended to prefix.  If `set_cb` ran on receipt, a
      handler `ERROR` would produce a standalone error, top-level
      recovery would `ReadyForQuery`, and the client's pipelined
      next operation would still run — with half-applied header
      state.  Cannot be expressed without dispatching from inside
      the Q/P/B/E case entries.*

- [ ] **`RegisterProtocolHeaderHandler` extension API
      (lifecycle-free signature: prefix + set_cb + ctx)**
      *The registry has to be invoked from the wire dispatch above;
      once that's in core, the registry must live with it.  The
      dispatcher is intentionally lifecycle-free — clear callbacks
      and scope semantics are the extension's responsibility, wired
      via `RegisterXactCallback`, `on_proc_exit`, and the new
      `pre_ready_for_query_hook`.*

- [ ] **Server GUCs gating the feature and bounding header sizes**
      *Extensions can define GUCs, but the feature-switch and
      byte-size cap must be evaluated where headers come off the
      wire — in core's dispatch path.*

## C. libpq: client-side API and protocol participation

- [ ] **`PQattachHeader` / `PQclearHeaders` / `PQheadersAvailable`
      API** (`src/interfaces/libpq/fe-exec.c` etc.)
      *libpq has no plugin/hook/callback model anywhere in
      `src/interfaces/libpq/`. An external library cannot add
      functions to libpq's API, intercept `PQsend*`, or carry
      connection-lifetime state without forking the library.*

- [ ] **libpq always sends `_pq_.headers=1` in StartupMessage**
      (`src/interfaces/libpq/fe-connect.c`)
      *StartupMessage construction is hardcoded in libpq's
      connection-setup path; no injection point.*

- [ ] **`pqGetNegotiateProtocolVersion3` recognizes `_pq_.headers`
      as an acknowledged param**
      *Backward-compat fix forced by the previous item: once libpq
      always advertises `_pq_.headers`, the receive side must accept
      the corresponding entry from feature-disabled or older
      servers.*

## D. Documentation

- [ ] **SGML updates: `log_line_prefix` table, CSV column list,
      JSON keys table, libpq function reference, protocol-message
      reference**
      *Documents the changes in A–C; lives with them.*

---

## Not on this list (could be out of tree)

- `contrib/otel` — `_otel.*` namespace handler, GUCs for trace
  context, `emit_log_hook` integration, span emission via
  `ExecutorStart`/`ExecutorEnd`/`ProcessUtility_hook`, sampler
  hook, JSON log emitter, versioned rendezvous API.
- `contrib/otel_exporter` — file emitter.
- Parallel-worker propagation of trace context — falls out of
  existing `RestoreGUCState` because `contrib/otel` stores state in
  GUCs; no core change needed.
- Span emission hooks themselves (`ExecutorStart`, `ExecutorEnd`,
  `ProcessUtility_hook`, `emit_log_hook`) — already in core.
- Per-transaction and per-session header-scope clearing — `if`
  headers exist, a contrib could clear them at the right times
  using `RegisterXactCallback` and `on_proc_exit`. Only per-message
  scope is genuinely core-only.
