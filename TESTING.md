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

A reusable test helper:

```bash
run_one() {
    local sampler=$1 policy=$2 flag=$3
    rm -f /tmp/pg_otel_demo.out /tmp/pg_otel_demo.err
    OTEL_TRACES_SAMPLER=$sampler POSTGRES_OTEL_SAMPLER_HOOK_POLICY=$policy \
        /usr/local/pgsql/bin/postgres -D /tmp/pg_otel_demo_data \
        >/tmp/pg_otel_demo.out 2>/tmp/pg_otel_demo.err &
    sleep 1.8
    /usr/local/pgsql/bin/psql -p 54321 -d postgres \
        -c "SET otel.traceparent='00-aabbccddeeff00112233445566778899-1111111111111111-$flag'" \
        -c "SELECT 1" >/dev/null 2>&1
    /usr/local/pgsql/bin/pg_ctl -D /tmp/pg_otel_demo_data stop -m fast >/dev/null 2>&1
    wait 2>/dev/null
    grep -c '^	TraceId' /tmp/pg_otel_demo.out
}
```

Note: `pg_ctl stop -m fast` is required to leave the WAL in a clean
state for the next iteration; SIGKILL-ing postgres (via `pkill`) will
corrupt the WAL header.

## The `(policy, wire-bit, sampler) → spans` matrix

A single span is emitted when policy / wire-bit / sampler combine to a
"record" decision; zero spans when they combine to "drop."  Expected
values reflect contrib/otel's gate logic in
`decide_whether_to_record`.

| Wire bit | Policy | OTel sampler | Expected spans | Verified |
|---|---|---|---|---|
| `01` (sampled) | `hook_on_unsampled_bit` (default) | `always_off` | **1** (W3C wins — gate 4 short-circuits before hook) | ✓ |
| `01` (sampled) | `hook_always`                     | `always_off` | **0** (sampler overrides W3C; opt-in to non-spec behaviour) | ✓ |
| `01` (sampled) | `never_respect_bit`               | `always_off` | **1** (hook never called; pure wire-bit) | ✓ |
| `00` (unsampled) | `never_always_sample`           | `always_off` | **1** (hook ignored; record everything) | ✓ |
| `00` (unsampled) | `hook_on_unsampled_bit`         | `always_off` | **0** (OTel SDK ParentBased default) | ✓ |
| `00` (unsampled) | `hook_on_unsampled_bit`         | `always_on`  | **1** (hook promotes unsampled trace) | ✓ |
| `00` (unsampled) | `never_respect_bit`             | `always_on`  | **0** (wire bit wins; sampler ignored) | ✓ |

Reproduction:

```bash
echo "wire-bit=1, hook_on_unsampled_bit + always_off: $(run_one always_off hook_on_unsampled_bit 01) (expect 1)"
echo "wire-bit=1, hook_always + always_off          : $(run_one always_off hook_always 01) (expect 0)"
echo "wire-bit=1, never_respect_bit + always_off    : $(run_one always_off never_respect_bit 01) (expect 1)"
echo "wire-bit=0, never_always_sample + always_off  : $(run_one always_off never_always_sample 00) (expect 1)"
echo "wire-bit=0, hook_on_unsampled_bit + always_off: $(run_one always_off hook_on_unsampled_bit 00) (expect 0)"
echo "wire-bit=0, hook_on_unsampled_bit + always_on : $(run_one always_on hook_on_unsampled_bit 00) (expect 1)"
echo "wire-bit=0, never_respect_bit + always_on     : $(run_one always_on never_respect_bit 00) (expect 0)"
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

* **`TraceIdRatioBased=0.5`:** across 30 high-entropy trace_ids
  (× 2 statements per iteration = 60 spans emitted before sampling),
  the SDK sampler recorded 28 — within statistical noise of the
  expected 30.

* **`PG_MODULE_MAGIC`:** verified `Pg_magic_func` is exported in
  the dynamic symbol table via `nm -D --defined-only`.  Cluster
  starts cleanly with the module loaded (no "incompatible
  library" ABI errors).

## What's not yet automated

* TAP coverage for the policy × wire-bit × sampler matrix above.
  The reproduction commands are deterministic; converting to
  `t/001_policy_matrix.pl` is straightforward but hasn't been
  done yet.
* OTLP-against-collector smoke test.  The `otlp` exporter mode
  has been exercised at the build level (`opentelemetry-otlp`
  links and `OTEL_TRACES_EXPORTER=otlp` switches to its code
  path) but no integration test fires up a collector.

When this demo turns into the production version mentioned in
[README.md](README.md) (bgworker-owned SDK, shm queue), proper
TAP coverage becomes a precondition for merge.
