# Proposal: selectable monitor-channel aggregation for Compressor and NoiseGate

**Status: design-only draft for discussion; not an implemented engine feature.**

Target: `HEnquist/camilladsp:next5`, reviewed at commit
[`b438410e734389c0cc12f8c4a0dac60552856ec2`](https://github.com/HEnquist/camilladsp/commit/b438410e734389c0cc12f8c4a0dac60552856ec2).

This draft proposes an optional `monitor_mode` for the existing Compressor and
NoiseGate processors. The objective is to make multichannel detection robust
against cancellation between monitored channels without changing existing
configurations. The exact name and scope are open for maintainer feedback.

This document and the accompanying reference script are discussion artifacts.
They do not change audio processing, add a dependency, or establish that a Rust
implementation has passed tests. A final implementation would update the normal
README, examples, changelog, and Rust tests rather than retain a separate proposal
as the user documentation.

## Motivation and current behavior

Both processors currently sum the signed samples of `monitor_channels` into a
scratch buffer, then calculate `20 * log10(abs(sample) + 1e-9)` and apply the
existing attack/release smoothing:

- [Compressor: `sum_monitor_channels` and `estimate_loudness`](https://github.com/HEnquist/camilladsp/blob/b438410e734389c0cc12f8c4a0dac60552856ec2/src/processors/compressor.rs)
- [NoiseGate: `sum_monitor_channels` and `estimate_loudness`](https://github.com/HEnquist/camilladsp/blob/b438410e734389c0cc12f8c4a0dac60552856ec2/src/processors/noisegate.rs)

For monitored samples `L = 0.5`, `R = -0.5`, the sum is zero even though both
channels are active. With sustained opposite-polarity signals, the compressor
can consequently stop attenuating and the noise gate can close after their
envelopes settle. At exact cancellation, the existing epsilon gives a finite
-180 dB detector input before smoothing, not a literal logarithmic infinity.

Identical signals on two channels instead produce a detector amplitude twice
that of one channel: approximately +6.02 dB. Thus detection depends on how the
channels combine, not only on their individual activity.

Signed summation can be intentional when monitoring a mono sum. It should remain
available. The proposal is an opt-in alternative, not a blanket replacement.

### Concrete use case

In a four-channel pipeline, channels 0/1 carry music and channels 2/3 carry a
separate stereo navigation/announcement source. The compressor monitors 2/3 and
attenuates only 0/1 before the streams are mixed. Activity on either announcement
channel should be able to trigger attenuation; reversing a channel's polarity
should not suppress detection. The same property is useful for linked
compression and noise gating outside this application.

### Existing next5 precedent

The [LookaheadLimiter processor](https://github.com/HEnquist/camilladsp/blob/b438410e734389c0cc12f8c4a0dac60552856ec2/src/processors/lookahead_limiter.rs)
already implements sample-wise maximum absolute amplitude across its monitored
channels in `detect_peaks`; see also [PR #504](https://github.com/HEnquist/camilladsp/pull/504).

`Max` below would use that same channel-aggregation principle, followed by the
Compressor/NoiseGate's existing envelope and gain calculations. It would not make
these processors lookahead processors. This proposal does **not** change the
limiter's peak detection or expose a cancellation-prone limiter mode.

## Proposed configuration and semantics

Add optional `monitor_mode: Sum | Max | Rms` to Compressor and NoiseGate.
Omitting the field preserves `Sum`. Explicit `null` should follow the project's
existing optional-field conventions and resolve to the same default.

Proposed fragment, **not valid on unmodified next5**:

```yaml
# Within an otherwise complete Compressor configuration:
parameters:
  channels: 4
  monitor_channels: [2, 3]
  process_channels: [0, 1]
  monitor_mode: Max
  # Keep the normal attack/release, threshold, factor, etc. settings.
```

Let `x_i[n]` be the sample on monitored channel `i`, and `N` the number of resolved
monitored channels. The amplitude presented to the existing level estimator is:

| Mode | Amplitude at sample n | Interpretation |
| --- | --- | --- |
| `Sum` | `abs(sum_i x_i[n])` | Existing signed sum, then rectification |
| `Max` | `max_i abs(x_i[n])` | Largest individual channel magnitude |
| `Rms` | `sqrt(sum_i x_i[n]^2 / N)` | Root mean square across channels at this sample |

Aggregation occurs **before** the existing dB conversion and attack/release
smoothing. These modes do not calculate independent per-channel envelopes and
then combine them.

`Rms` here is a spatial/channel aggregation, not a time-windowed RMS detector or
perceptual loudness estimator. The name should be documented carefully; an
alternative such as `ChannelRms` is open for discussion. Adding a new temporal
RMS envelope would be a separate change.

`Max` and `Rms` are invariant to individual polarity reversals. They avoid signed
cross-channel cancellation, but do not promise identical envelopes for arbitrary
time shifts or phase changes that alter the instantaneous channel magnitudes.

### Level examples

Values below are linear amplitudes before dB conversion and smoothing:

| Monitored samples | Sum | Max | Rms |
| --- | ---: | ---: | ---: |
| `[0.5]` | 0.5 | 0.5 | 0.5 |
| `[0.5, 0]` | 0.5 | 0.5 | 0.353553... |
| `[0, 0.5]` | 0.5 | 0.5 | 0.353553... |
| `[0.5, 0.5]` | 1.0 | 0.5 | 0.5 |
| `[0.5, -0.5]` | 0.0 | 0.5 | 0.5 |
| `[0, 0]` | 0.0 | 0.0 | 0.0 |
| `[0.5, 0.5, 0.5, 0.5]` | 2.0 | 0.5 | 0.5 |

For `Rms`, one active channel among two is about 3.01 dB below two equally active
channels. Adding silent monitored channels lowers that mode's reading. `Max`
does not have that dependence and is the preferred mode for the announcement
use case. Neither new mode requires retuning unless explicitly selected.

## Compatibility and implementation boundaries

Preserve the default even for v5 unless a separate migration decision is made.
Switching from `Sum` to `Max` for identical stereo content lowers the detector
reading by about 6.02 dB; silently changing the default would change users'
compression and gate thresholds.

The intended implementation should:

1. Add a shared aggregation enum and optional fields to the two configuration
   structs. Preserve existing omitted/empty monitor-list expansion to all
   channels, validate the resolved channel selection, and reject unknown modes.
2. Extract only the channel-reduction operation into a small shared helper.
   Preserve the legacy signed-sum accumulation order and existing dB/envelope
   calculations for `Sum`; do not broaden this into an envelope refactor.
3. Reuse preallocated scratch storage, use `CamillaFloat` in processing, and
   follow next5's conversion conventions. Introduce no per-chunk allocations,
   audio delay, backend coupling, or external dependency.
4. Apply the selected mode both during construction and parameter updates.
   Preserve existing envelope state on a mode-only update, then let it evolve
   under the existing smoothing. Test that transition explicitly; do not claim
   that changing detector calibration during playback is inaudible.
5. Define handling of non-finite samples explicitly rather than accidentally
   changing it through a reduction primitive. An allocation/performance check
   and numerical tests in both processing precisions belong in the implementation.
6. Document the modes and calibration differences in README, add a complete
   sidechain example, and record the user-facing addition in CHANGELOG.

A shared helper should initially serve Compressor and NoiseGate. Reusing it in
LookaheadLimiter is unnecessary for this change and should not risk altering
its existing protection behavior.

## Regression and acceptance tests for the Rust implementation

These are requirements, **not tests already implemented by this draft**.

| Area | Required coverage |
| --- | --- |
| Legacy behavior | Omitted/default and explicit `Sum` reproduce the current path, including correlated and opposite-polarity inputs, across multiple chunks. |
| Channel reduction | Silence, one channel, left-only, right-only, identical stereo, opposite-polarity stereo, unequal amplitudes, more than two channels, and selected subsets. |
| New-mode invariants | Per-channel polarity inversion and channel reordering leave `Max`/`Rms` unchanged within appropriate numerical tolerances. |
| RMS normalization | Identical channels retain per-channel amplitude; one active of two gives amplitude divided by sqrt(2). |
| Compressor integration | After settling, an opposite-polarity sidechain triggers gain reduction in `Max`/`Rms`; only process channels change, with linked channels receiving the same gain. |
| Gate integration | After settling, active opposite-polarity monitor channels keep the gate open in the new modes; silence closes it according to existing behavior. |
| Configuration | YAML parsing/round-trip, default/null policy, unknown-mode rejection, omitted/empty lists, invalid channel indices, and mode-only configuration updates. |
| Streaming | Chunk boundaries and partitioning do not change the result; mode updates preserve state as specified; scratch data cannot leak between calls. |
| Numerics and cost | Both f64 and `camillafloat_f32`, finite extreme values, explicit non-finite policy, and no steady-state allocation. |

Per next5's own contributor guidance, validation should include `cargo fmt`,
`cargo clippy --all-targets --all-features`, and `cargo test`, plus an explicit
`RUSTFLAGS="--cfg camillafloat_f32"` test/build. `--all-features` does not select
the f32 processing configuration.

## What this does not solve

A more robust channel detector is not a complete fixed-depth ducker. The
compressor still applies level-dependent reduction according to threshold and
ratio. Hold time, hysteresis, fixed attenuation, and lookahead would need
separate design and tests.

It also does not identify navigation versus music within an already mixed
announcement input, qualify USB/S/PDIF stream health, suppress arbitrary invalid
PCM, or guarantee pop-free reacquisition. Device recovery and speaker protection
remain separate responsibilities. In particular, a strong invalid sample can
activate a detector just as a strong valid sample can.

## Validation of this draft

The accompanying [reference script](monitor_aggregation_reference.py) checks the
proposed equations, not CamillaDSP. It has 27 tabulated amplitude checks and 80
polarity/channel-order checks, all passing under Python 3. It requires only the
standard library:

```sh
python3 proposals/monitor_aggregation_reference.py
```

No Rust implementation, Cargo build, integration test, hardware measurement, or
benchmark is claimed by this draft. All engine-level acceptance items above
remain open.

## Questions for maintainer feedback

- Is optional monitor aggregation desirable for v5, and is `monitor_mode` a good
  name relative to the existing `monitor_channels` terminology?
- Should the first implementation include all three modes, or start with
  `Sum`/`Max` and defer the spatial RMS mode?
- Is preserving `Sum` as default the preferred compatibility policy for next5?
- Is a small shared reducer for Compressor and NoiseGate the appropriate scope?
