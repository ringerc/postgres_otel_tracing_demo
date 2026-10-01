# Testing

This is a proof-of-concept; there is no automated test harness yet.
What follows is the manual smoke-test matrix used while building it.
Every cell in the table below was verified end-to-end against a
postgres backend with `contrib/otel` + `postgres_otel_tracing_demo`
loaded.

## Setup

```bash
# Build + install postgres with contrib/otel (patched tree)
# Build + install this crate
sudo make install

# Initialize a dedicated cluster
PGDATA=/tmp/pg_otel_demo_data
rm -rf $PGDATA && /usr/local/pgsql/bin/initdb -D $PGDATA --auth=trust -U $USER

cat >> $PGDATA/postgresql.auto.conf <<'EOF'
shared_preload_libraries = 'otel,otel_postgres_tracing,postgres_otel_tracing_demo'
port = 54321
logging_collector = off
log_destination = 'stderr'
EOF
```

A reusable test helper.  Sampling is entirely otel_api's own policy now
(`otel_api.sampler` / `otel_api.sampler_arg`, postgres-cdq.18); this
module has no sampler hook or invocation-policy env var any more, so
the helper sets the GUCs with `-c` instead of exporter env vars:

```bash
run_one() {
    local sampler=$1 flag=$2
    rm -f /tmp/pg_otel_demo.out /tmp/pg_otel_demo.err
    /usr/local/pgsql/bin/postgres -D /tmp/pg_otel_demo_data \
        -c otel_api.sampler=$sampler \
        >/tmp/pg_otel_demo.out 2>/tmp/pg_otel_demo.err &
    sleep 1.8
    /usr/local/pgsql/bin/psql -p 54321 -d postgres \
        -c "SET otel_api.traceparent='00-aabbccddeeff00112233445566778899-1111111111111111-$flag'" \
        -c "SELECT 1" >/dev/null 2>&1
    /usr/local/pgsql/bin/pg_ctl -D /tmp/pg_otel_demo_data stop -m fast >/dev/null 2>&1
    wait 2>/dev/null
    grep -c '^	TraceId' /tmp/pg_otel_demo.out
}
```

Note: `pg_ctl stop -m fast` is required to leave the WAL in a clean
state for the next iteration; SIGKILL-ing postgres (via `pkill`) will
corrupt the WAL header.

## The `(otel_api.sampler, wire-bit) → spans` matrix

A single span is emitted when `otel_api.sampler` and the remote
parent's W3C sampled bit combine to a "record" decision; zero spans
when they combine to "drop."  The exhaustive, deterministic version of
this matrix (including `traceidratio` with fixed trace IDs) lives in
`postgres_otel_api`'s `tests/otel_api_conformance/t/023_sampler.pl`;
this is the manual, this-repo-only smoke check that otel_api.sampler
actually reaches a real exporter.

| Wire bit | `otel_api.sampler` | Expected spans | Verified |
|---|---|---|---|
| `01` (sampled) | `always_off` | **0** (ignores the remote bit entirely) | ✓ |
| `01` (sampled) | `parentbased_always_off` | **1** (follows the remote bit) | ✓ |
| `00` (unsampled) | `always_on` | **1** (ignores the remote bit entirely) | ✓ |
| `00` (unsampled) | `parentbased_always_on` | **0** (follows the remote bit) | ✓ |

Reproduction:

```bash
echo "wire-bit=1, always_off          : $(run_one always_off 01) (expect 0)"
echo "wire-bit=1, parentbased_always_off: $(run_one parentbased_always_off 01) (expect 1)"
echo "wire-bit=0, always_on           : $(run_one always_on 00) (expect 1)"
echo "wire-bit=0, parentbased_always_on : $(run_one parentbased_always_on 00) (expect 0)"
```

## Other things verified manually

* **End-to-end trace context propagation:** client `traceparent`
  → contrib/otel `OtelSpan` → opentelemetry-rust SDK `SpanData`
  → stdout exporter.  Client-supplied trace_id appears in
  output as-is; client-supplied span_id becomes `parent_span_id`;
  a fresh span_id is generated.

* **Success vs error spans:** `SELECT 1` produces a span with
  `Status: Unset`; `SELECT 1/i FROM generate_series(0,0) AS i`
  (forces a runtime divide-by-zero past constant folding)
  produces a span with `Status: Error` and an event carrying
  `postgres.sqlstate=22012`, `postgres.elevel=21`,
  `code.filepath=int.c`, `code.function=int4div`.

* **`otel_api.sampler=traceidratio`:** exercised deterministically
  (fixed trace IDs on both sides of the rejection threshold, not a
  statistical sample) in `postgres_otel_api`'s
  `tests/otel_api_conformance/t/023_sampler.pl`.

* **`PG_MODULE_MAGIC`:** verified `Pg_magic_func` is exported in
  the dynamic symbol table via `nm -D --defined-only`.  Cluster
  starts cleanly with the module loaded (no "incompatible
  library" ABI errors).

## What's not yet automated

* TAP coverage for this repo's own sampler smoke matrix above (the
  exhaustive matrix is already automated, but in
  `postgres_otel_api`, not here).  The reproduction commands are
  deterministic; converting to `t/001_sampler_matrix.pl` is
  straightforward but hasn't been done yet.
* OTLP-against-collector smoke test.  The `otlp` exporter mode
  has been exercised at the build level (`opentelemetry-otlp`
  links and `OTEL_TRACES_EXPORTER=otlp` switches to its code
  path) but no integration test fires up a collector.

When this demo turns into the production version mentioned in
[README.md](README.md) (bgworker-owned SDK, shm queue), proper
TAP coverage becomes a precondition for merge.
