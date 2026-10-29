# Protection test capture

The `protection-test-capture` Cargo feature adds test-only WebSocket operations
`{"StartProtectionTestCapture": ID}` and `{"StopProtectionTestCapture": ID}`.
ID is a positive signed-64-bit integer, strictly increasing for this process.
There is one session at a time; starting another cannot discard an unfinished
capture. A timed-out Stop retains its pool until a later RPC can reclaim it
after all writers retire; Start rejects while one remains. The audio owner never
reclaims that pool. The normal production build contains neither operation nor capture
storage. The feature changes no guard, control or output-admission behavior.

Start allocates a fixed pool of 128 records on the RPC owner. Audio owners use
only `try_lock`, `try_recv`, fixed-size copies and `try_send`; no file, socket,
allocation, blocking mutex acquisition or serialization occurs there. Each
record is consumed once, without overwrite. Capacity exhaustion, contention,
abandoned records and writers still outstanding at the bounded 100 ms Stop
wait make the capture incomplete. Stop retains available records and counters;
a failed prefix must never count as a passing attempt. At two records per
256-frame chunk the nominal capacity is about 341 ms, not a guaranteed time
window. Longer or stalled experiments must fail rather than silently truncate.

The reply identifies schema, process, boot/time namespace, fork revision and
capture ID. Guard records retain graph/profile identities, pipeline/backend/
reset epoch, frame range, exact channel-major IEEE-754 binary64 bits before and
after the guard, shape validity, guard output admission (`guard_active`) and
persistent trip.
`times_ns` for guards are capture-entry, candidate-copy completion, decision,
coherent-publication completion and forwarded-copy completion, on
CLOCK_MONOTONIC. Conversion records retain exact interleaved S24_3_LE bytes,
clipped count and conversion begin/end before the ordinary status counter path.
Conversion times repeat end in the decision/publication positions; their final
position is capture-copy completion. This is conversion evidence, not successful
PCM consumption. Only the existing direct hardware-ring progress proves that.

Capture copy overhead and binary identity belong in the instrumentation
qualification. The capture-entry clock is not physical source onset. Inspect
all relevant guard and conversion records, retain overflow/failed attempts,
and separately bind parent decision, actual source-onset uncertainty and
REMOTE_SENSE. No fixture or capture grants target timing or activation credit.

Offline fixture export (no ALSA device opened):

```sh
P4_CAPTURE_FIXTURE=/absolute/path/capture.json cargo test --locked \
  protection::capture::tests::exact_candidate_forwarded_and_conversion_fixture
```

The fixture exercises the real guard and PCM converter on an unowned guard
that trips on the injected peak. Production Buildroot must not select `protection-test-capture`.
