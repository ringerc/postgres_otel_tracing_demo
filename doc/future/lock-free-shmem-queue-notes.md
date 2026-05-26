# Lock-free shared-memory MPSC queue for `contrib/otel`

Design notes on options for an N-producer → 1-consumer queue carrying
OpenTelemetry span records from individual Postgres backends to a
background worker that batches and exports them via the OTel SDK.

## Requirements

- **N producers, 1 consumer.** Every backend may emit spans; one
  bgworker drains.
- **Variable-size records.** Span payloads vary from ~50 B to a few KB
  depending on attributes.
- **Never block the producer.** Spans are emitted on the hot query
  path — a stalled backend (debugger, page fault, OS scheduling)
  must not stall the queue. Lock-free producers strongly preferred;
  if any mutex is involved it must be robust against holder death.
- **Bounded, drop-on-full.** Telemetry: dropping spans is preferable
  to backpressuring queries.
- **Shared memory across processes.** Producers and consumer are
  separate OS processes. All pointers internal to the queue must be
  offsets (position-independent) — the backing region is mapped at
  different virtual addresses in different backends.
- **Initialised on a caller-provided region.** Postgres allocates
  shared memory via `ShmemInitStruct` at postmaster startup and
  hands the extension a `void * + size`. The queue implementation
  must work on top of that, not allocate its own backing.
- **C ABI.** Even if implemented in Rust, must be wrappable behind
  `extern "C"` for use by other Postgres extensions.

## Postgres in-tree candidates

Survey of existing IPC primitives evaluated as starting points.

| Primitive | Shape | Variable-size | Lock-free producers | Verdict |
|---|---|---|---|---|
| [shm_mq](postgres/src/backend/storage/ipc/shm_mq.c) | 1:1 ring | Yes (iovec) | Atomic u64 head/tail | **Wrong shape.** Single sender / single receiver by design. Parallel query uses one mq per worker — N:1 by replication, not by sharing. Repurposing for true N:1 would mean rewriting the synchronisation. |
| [pg_atomic_*](postgres/src/include/port/atomics.h) | Primitives | n/a | Yes on x86-64 / ARM64 / PPC64; spinlock fallback on 32-bit | **Building blocks only.** u32/u64 CAS, fetch-add, barriers — enough to build the queue, not the queue itself. |
| DSM / DSA / dshash | Allocator + hash | Yes | No (per-bucket LWLocks) | **Storage, not queue.** Useful if spans needed dynamic allocation outside the ring; dshash is a map not a FIFO. |
| [NOTIFY/LISTEN](postgres/src/backend/commands/async.c) | M:N queue | Yes | No (heavyweight `NotifyQueueLock`) | **Wrong cost.** Real N-producer queue, but SLRU-backed (disk durability not needed) and serialised by a heavyweight lock. Reference design, not reusable code. |
| WAL insert (`WALInsertLocks`) | N:1 | n/a | Reserve-then-write w/ CAS | **Pattern only.** Algorithm is reusable; the implementation is welded to LSN/WAL semantics. |
| pgstat shared memory | Aggregator | Yes | Per-entry LWLock | **Pattern worth borrowing:** backends accumulate locally, flush in batches at commit/timeout. Reduces shared-memory contention. Not queue-shaped itself. |
| Backend status array | Own-slot writes | n/a | Per-slot spinlock | Own-slot pattern is lock-free against other backends, but not FIFO. |
| Latches / ProcSignal | Signalling | n/a | Yes | Needed for the *wakeup* half — consumer sleeps on its latch, producers `SetLatch` on empty→non-empty transitions. |

### What this rules in

The realistic Postgres-native paths:

1. **Per-backend SPSC ring + multiplexing consumer.** Mirror parallel
   query: give each backend its own shm_mq-style SPSC buffer, have
   the bgworker round-robin them. Zero producer-side contention.
   Trades shared-memory footprint (N × buffer) for simplicity.
   Caveat: bounded backend count, and `MaxBackends` × buffer-size
   shared-memory budget must be set at postmaster start.
2. **Custom MPSC ring buffer on `pg_atomic_uint64`.** Single shared
   ring, `fetch_add` to reserve a length-prefixed slot, publish via
   a per-record sequence number. Drop-on-full by comparing
   `head − tail` before the CAS. Standard MPSC pattern
   (Vyukov / LMAX / Agrona); maps cleanly onto Postgres atomics on
   platforms with native 64-bit CAS.

Pair either with pgstat-style **per-backend local batching** so the
hot path touches shared memory once per transaction (or once per
N spans), not once per span.

## Rust ecosystem candidates

Surveyed under the assumption the implementation could be written in
Rust and exposed via `extern "C"` for use by other extensions.

| Crate | XP? | Caller-provided buffer? | MPSC | VarSz | Status | Verdict |
|---|---|---|---|---|---|---|
| crossbeam-channel / crossbeam-queue | No | No | Yes | Boxed `T` | Active | `Arc<Inner>`, heap pointers. Thread-only. |
| flume | No | No | Yes | Boxed `T` | Active | Same as crossbeam. |
| rtrb, ringbuf, thingbuf | No | No | SPSC | No | Active | Thread-only; "shared" means across threads, not processes. |
| concurrent-queue, atomic-queue, lockfree | No | No | MPMC | Boxed `T` | Active (used by smol) | Heap-resident nodes / `Arc`. |
| bus, multiqueue | No | No | SPMC (broadcast) | No | Mixed | Wrong shape and in-process. |
| disruptor-rs | No | No | Yes | Slotted `T` | Active | LMAX Disruptor inter-thread; `Box`-managed ring + `Arc`. |
| **iceoryx2** (Eclipse, Rust-native) | **Yes** | **No — manages its own shm** | Yes (`max_publishers(N)` + 1 subscriber) | Bounded slices, `ZeroCopySend` payloads | Very active, pre-1.0 | **Close in shape, wrong in lifecycle.** Creates and owns POSIX-shm segments via its own services layer; can't sit on top of `ShmemInitStruct`. Preallocates one segment *per publisher* (N × size). No API-stability promise pre-1.0. |
| **rusteron-rb** | Yes | No — `new_with_capacity` allocates | Yes (`AeronMpscRb`) | Yes (`try_claim(typeId, len)`) | Active, 0.1.x | Wraps Agrona's `ManyToOneRingBuffer` via `libaeron` C bindings. **The algorithm is exactly what's wanted**; the crate isn't usable as a dependency (allocates, pulls in libaeron). Useful as reference implementation. |
| aeron-rs (UnitedTraders) | Yes | No | Yes | Yes | Stale | Requires running the Aeron media driver as a separate process. |
| **ipmpsc** | Yes | No — owns the file | Yes (literally named this) | Yes (bincode) | **Dormant since May 2021**, ~56 dl/month | Right shape, abandoned, not portable to PG-owned shm. |
| ipc-channel (Servo) | Yes (UDS) | No | Yes | Yes | Active | Sockets, not shared memory; producer can block on kernel buffer backpressure. |
| shared_memory + raw_sync | Yes | Yes (over any `&mut [u8]`) | n/a (primitives) | n/a | shared_memory active; raw_sync quiet | **Useful Lego, not a queue.** raw_sync's `Mutex` is plain pthread, not `PTHREAD_MUTEX_ROBUST` — backend death mid-push wedges the queue. Kills constraint #4. |
| memmap2, shmemfdrs, shmem | Yes | Yes | n/a | n/a | Mixed | Bare shm wrappers; you'd write the queue. |
| rkyv + bytecheck | n/a | n/a | n/a | Yes | Very active | **Payload encoding, not queue.** rkyv 0.8 zero-copy archive; bytecheck validates on the consumer end (producer memory is untrusted from the consumer's POV). Strong pairing with any hand-rolled queue. |

### What this rules out

Every in-process crate fails on cross-process pointer semantics
(`Arc`/`Box` are meaningless across address spaces). Every
cross-process crate fails on the "caller-provided buffer" check —
they all want to own their shm allocation.

The two closest matches both fail differently:

- **iceoryx2** is production-grade and the right shape, but it
  insists on managing its own shared-memory pool. Using it would
  mean abandoning the `ShmemInitStruct` model and running an
  iceoryx2 service-discovery layer alongside the postmaster — two
  parallel resource scopes tied to Postgres lifecycle.
- **rusteron-rb** wraps Agrona's `ManyToOneRingBuffer`, which is the
  exact algorithm the Postgres-native option #2 above would build.
  But the crate is a binding to libaeron and allocates its own
  backing, so it can't sit on `ShmemInitStruct` memory either.

### Bottom line: write it

The honest answer is that no Rust crate clears all four of:

1. Operates on a caller-provided `&'static [u8]` / `*mut u8 + len`.
2. Position-independent (offsets, not pointers).
3. Tolerates producer crash without freezing the consumer.
4. `no_std` / allocator-agnostic for clean `extern "C"` wrapping.

Most fail #1 because the Rust idiom is to own your storage.

What to build, then:

1. **`no_std` port of Agrona's `ManyToOneRingBuffer`** over
   `&'static [UnsafeCell<u8>]` or a `*mut u8 + len` pair. Producers
   CAS the tail counter, write `[len:u32][type:u32][payload]`
   32-byte-aligned, publish by storing length last with `Release`.
   Consumer reads with `Acquire`. Drop-on-full = "CAS failed,
   return false". Wrap handled with a padding record (`type = -1`).
   ~500–700 lines including tests. Reference implementations:
   [Agrona Java source](https://github.com/aeron-io/agrona/blob/master/agrona/src/main/java/org/agrona/concurrent/ringbuffer/ManyToOneRingBuffer.java)
   and [rusteron-rb](https://docs.rs/crate/rusteron-rb/latest).
2. **rkyv-encoded span payloads.** Producer serialises into the
   claimed slot in place; consumer validates with bytecheck before
   touching fields, since the producer-side memory is untrusted
   from the consumer's point of view (buggy/malicious producer
   could leave garbage).
3. **eventfd / Win32 event for wakeup.** Producers signal on
   empty→non-empty transitions; consumer waits with `poll` /
   `WaitForSingleObject`. **Not a cross-process mutex** —
   producer death must not block wakeup. On Linux, write to an
   eventfd is non-blocking and survives producer crash.

C ABI surface:

```c
pgotel_queue_init(void *shm, size_t bytes);
pgotel_queue_try_push(const void *span, size_t len);
pgotel_queue_pop(void *out, size_t *len);
pgotel_queue_wait(int timeout_ms);
```

Link the Rust crate into `contrib/otel` with `panic=abort` and a
minimal runtime. Other extensions can link the same C ABI to share
the queue.

## Open questions

- **Static vs. dynamic queue sizing.** `ShmemInitStruct` is sized at
  postmaster start. GUC `otel.queue_size_kb` reloaded at restart, or
  one large pre-allocated region with a free-list inside? Static
  + GUC is simpler and matches every other Postgres extension.
- **Backend-local batching threshold.** Flush per-span (latency,
  more shared-mem contention) vs. per-N-spans / per-xact (lower
  overhead, longer trace-visible delay). pgstat flushes at xact end
  and on a timer; reasonable starting point.
- **Drop accounting.** Per-backend counter of dropped spans,
  surfaced via `pg_stat_otel` view? Drops are silent without it.
- **Consumer-side validation cost.** bytecheck on every record may
  dominate consumer CPU; alternative is `unsafe` rkyv access with
  a producer-signed CRC. Punt until benchmarked.
- **Robustness vs. holder death.** Hand-rolled CAS-only design has
  no "holder" to die — a producer that crashes mid-write leaves an
  unpublished slot (no sequence-number bump), which the consumer
  skips. Confirm this property holds for the wrap-padding record
  case.

## See also

- [[contrib-otel-extension-api]] — public extension API that would
  expose this queue to other extensions.
- [[contrib-otel-metrics-api]] — same pattern for metrics.
- [[contrib-otel-rust-bindings]] — Rust binding plans; this queue
  could be the first non-trivial Rust component in `contrib/otel`.
- [[edb-wait-states-otel-integration]] — `edb_wait_states` would
  be a natural producer if integrated.
