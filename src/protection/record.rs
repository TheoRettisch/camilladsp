//! Fixed-size atomic records and their only word layouts.
//!
//! Each record type encodes and decodes its fields through one cursor, in one
//! order, in this file. No other module indexes a published word.

use super::control::{self, Control, Mode, Receipt, State};
use super::{CHANNELS, Reason, Snapshot, Trip, identity};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const GUARD_WORDS: usize = 45;
pub(super) const PLAYBACK_WORDS: usize = 43;

/// Every element is atomic; the sequence counter does not excuse non-atomic
/// seqlock reads. SeqCst gives a simple auditable ordering for this small record.
#[derive(Debug)]
pub(super) struct AtomicRecord<const N: usize> {
    sequence: AtomicU64,
    values: [AtomicU64; N],
}

impl<const N: usize> Default for AtomicRecord<N> {
    fn default() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            values: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl<const N: usize> AtomicRecord<N> {
    // There is exactly one audio owner per record. Readers never write or wait.
    pub(super) fn publish(&self, values: [u64; N]) {
        let sequence = self.sequence.load(Ordering::SeqCst);
        assert!(
            sequence < i64::MAX as u64 - 2,
            "Protection sequence exhausted"
        );
        self.sequence.store(sequence + 1, Ordering::SeqCst);
        for (target, value) in self.values.iter().zip(values) {
            target.store(value, Ordering::SeqCst);
        }
        self.sequence.store(sequence + 2, Ordering::SeqCst);
    }

    pub(super) fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::SeqCst)
    }

    pub(super) fn read(&self) -> Option<(u64, [u64; N])> {
        (0..3).find_map(|_| self.read_once())
    }

    pub(super) fn read_once(&self) -> Option<(u64, [u64; N])> {
        let before = self.sequence.load(Ordering::SeqCst);
        if before & 1 != 0 {
            return None;
        }
        let values = std::array::from_fn(|i| self.values[i].load(Ordering::SeqCst));
        (before == self.sequence.load(Ordering::SeqCst)).then_some((before, values))
    }
}

/// Writes exactly N words; a layout that does not fill the record fails the
/// first publication of any test instead of silently shifting fields.
struct Encoder<const N: usize> {
    words: [u64; N],
    at: usize,
}

impl<const N: usize> Encoder<N> {
    fn new() -> Self {
        Self {
            words: [0; N],
            at: 0,
        }
    }

    fn put(&mut self, word: u64) {
        self.words[self.at] = word;
        self.at += 1;
    }

    fn flag(&mut self, value: bool) {
        self.put(value as u64);
    }

    fn finish(self) -> [u64; N] {
        assert_eq!(self.at, N, "Protection record layout is incomplete");
        self.words
    }
}

struct Decoder<'a> {
    words: std::slice::Iter<'a, u64>,
}

impl<'a> Decoder<'a> {
    fn new(words: &'a [u64]) -> Self {
        Self {
            words: words.iter(),
        }
    }

    fn take(&mut self) -> u64 {
        *self
            .words
            .next()
            .expect("Protection record layout overruns")
    }

    fn flag(&mut self) -> bool {
        self.take() != 0
    }

    fn finish(self) {
        assert_eq!(
            self.words.len(),
            0,
            "Protection record layout is incomplete"
        );
    }
}

/// Decode by the same discriminant `as u64` encodes, so neither direction
/// carries its own table of numbers.
macro_rules! variants {
    ($type:ty: $($variant:ident),+) => {
        impl $type {
            fn decode(word: u64) -> Option<Self> {
                [$(Self::$variant),+].into_iter().find(|v| *v as u64 == word)
            }
        }
    };
}
variants!(Reason: Peak, Nonfinite, Coverage, Rms, ShortPower, LongPower, Tone, DcFrozen);
variants!(State: Inactive, Qualifying, Active, Tripped);
variants!(Mode: Prearm, Armed);
variants!(control::Result: Applied, Invalid, StateMismatch, NotQualified);

impl Snapshot {
    /// `sequence` belongs to the seqlock and `command_high_water` to the
    /// owner; neither is part of the published record.
    pub(super) fn encode(&self) -> [u64; GUARD_WORDS] {
        let mut e = Encoder::new();
        e.put(self.pipeline);
        e.put(self.backend);
        e.put(self.reset);
        e.put(self.frame_floor);
        e.put(self.frames);
        e.flag(self.processing_valid);
        e.put(self.source_begin_ns);
        e.put(self.source_end_ns);
        e.put(self.state as u64);
        e.flag(self.candidate_observable);
        e.put(self.qualified_frames);
        e.flag(self.short_power_veto);
        e.flag(self.attempt_required);
        e.put(self.trip_generation);
        // Zero is no trip: every Reason discriminant is positive.
        e.put(self.trip.map_or(0, |t| t.reason as u64));
        e.put(self.trip.map_or(0, |t| t.generation));
        e.put(
            self.trip
                .and_then(|t| t.channel)
                .map_or(0, |c| c as u64 + 1),
        );
        e.put(self.trip.map_or(0, |t| t.frame));
        e.put(self.trip.map_or(0, |t| t.at_ns));
        // Zero is no owner: the control owner rejects a zero UUID.
        let owner = self.control.owner.unwrap_or(0);
        e.put((owner >> 64) as u64);
        e.put(owner as u64);
        e.put(self.control.revision);
        e.put(self.control.parent_reset_generation);
        e.put(self.control.qualification_id);
        e.put(self.control.mode as u64);
        // Zero is no receipt: every Result discriminant is positive.
        let receipt = self.control.last_command;
        e.put(receipt.map_or(0, |r| r.result as u64));
        e.put(receipt.map_or(0, |r| r.command_id));
        e.put(receipt.map_or(0, |r| r.pipeline));
        e.put(receipt.map_or(0, |r| r.backend));
        e.put(receipt.map_or(0, |r| r.reset));
        e.put(receipt.map_or(0, |r| r.revision));
        e.put(receipt.map_or(0, |r| r.parent_reset_generation));
        e.put(receipt.map_or(0, |r| r.qualification_id));
        for word in self.identity.graph.into_iter().chain(self.identity.profile) {
            e.put(word);
        }
        e.flag(self.final_guard);
        e.put(self.activity_generation);
        e.put(self.silent_frames);
        e.put(self.last_active_end_frame);
        e.finish()
    }

    /// None for a word that no owner publishes; the caller reports it as an
    /// incoherent read, never as a default state.
    pub(super) fn decode(sequence: u64, words: &[u64; GUARD_WORDS]) -> Option<Self> {
        let mut d = Decoder::new(words);
        let pipeline = d.take();
        let backend = d.take();
        let reset = d.take();
        let frame_floor = d.take();
        let frames = d.take();
        let processing_valid = d.flag();
        let source_begin_ns = d.take();
        let source_end_ns = d.take();
        let state = State::decode(d.take())?;
        let candidate_observable = d.flag();
        let qualified_frames = d.take();
        let short_power_veto = d.flag();
        let attempt_required = d.flag();
        let trip_generation = d.take();
        let reason = d.take();
        let (generation, channel, frame, at_ns) = (d.take(), d.take(), d.take(), d.take());
        let trip = match reason {
            0 => None,
            reason => Some(Trip {
                generation,
                reason: Reason::decode(reason)?,
                channel: channel.checked_sub(1).map(|c| c as usize),
                frame,
                at_ns,
            }),
        };
        let owner = (u128::from(d.take()) << 64) | u128::from(d.take());
        let revision = d.take();
        let parent_reset_generation = d.take();
        let qualification_id = d.take();
        let mode = Mode::decode(d.take())?;
        let result = d.take();
        let receipt: [u64; 7] = std::array::from_fn(|_| d.take());
        let last_command = match result {
            0 => None,
            result => Some(Receipt {
                result: control::Result::decode(result)?,
                command_id: receipt[0],
                pipeline: receipt[1],
                backend: receipt[2],
                reset: receipt[3],
                revision: receipt[4],
                parent_reset_generation: receipt[5],
                qualification_id: receipt[6],
            }),
        };
        let identity = identity::Identity {
            graph: std::array::from_fn(|_| d.take()),
            profile: std::array::from_fn(|_| d.take()),
        };
        let final_guard = d.flag();
        let activity_generation = d.take();
        let silent_frames = d.take();
        let last_active_end_frame = d.take();
        d.finish();
        Some(Self {
            sequence,
            pipeline,
            backend,
            reset,
            frame_floor,
            frames,
            processing_valid,
            source_begin_ns,
            source_end_ns,
            state,
            candidate_observable,
            qualified_frames,
            short_power_veto,
            attempt_required,
            trip_generation,
            trip,
            control: Control {
                owner: (owner != 0).then_some(owner),
                revision,
                parent_reset_generation,
                qualification_id,
                mode,
                last_command,
                // Not exported: it is an owner-side replay fence, not status.
                command_high_water: 0,
            },
            identity,
            final_guard,
            activity_generation,
            silent_frames,
            last_active_end_frame,
        })
    }
}

/// The last chunk whose every frame the PCM accepted, with its forwarded power.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Telemetry {
    pub valid: bool,
    pub frame_begin: u64,
    pub frame_end: u64,
    pub source_begin_ns: u64,
    pub source_end_ns: u64,
    pub write_begin_ns: u64,
    pub write_end_ns: u64,
    pub rms_power: [f64; CHANNELS],
}

/// Coherent playback-owner record, exactly as that owner last published it.
/// Whether it is evidence for the current reset lifetime is the reader's check.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct PlaybackRecord {
    pub pipeline: u64,
    pub backend: u64,
    pub reset: u64,
    pub floor: u64,
    pub submitted_frames: u64,
    pub consumed_frames: u64,
    pub consumption_valid: bool,
    pub source_begin_ns: u64,
    pub source_end_ns: u64,
    pub telemetry: Telemetry,
    pub conversion_valid: bool,
    pub clipped_samples: u64,
    pub conversion_at_ns: u64,
}

impl PlaybackRecord {
    pub(super) fn encode(&self) -> [u64; PLAYBACK_WORDS] {
        let mut e = Encoder::new();
        e.put(self.pipeline);
        e.put(self.backend);
        e.put(self.reset);
        e.put(self.floor);
        e.put(self.submitted_frames);
        e.put(self.consumed_frames);
        e.flag(self.consumption_valid);
        e.put(self.source_begin_ns);
        e.put(self.source_end_ns);
        e.flag(self.telemetry.valid);
        e.put(self.telemetry.frame_begin);
        e.put(self.telemetry.frame_end);
        e.put(self.telemetry.source_begin_ns);
        e.put(self.telemetry.source_end_ns);
        e.put(self.telemetry.write_begin_ns);
        e.put(self.telemetry.write_end_ns);
        for power in self.telemetry.rms_power {
            e.put(power.to_bits());
        }
        e.flag(self.conversion_valid);
        e.put(self.clipped_samples);
        e.put(self.conversion_at_ns);
        e.finish()
    }

    pub(super) fn decode(words: &[u64; PLAYBACK_WORDS]) -> Self {
        let mut d = Decoder::new(words);
        let record = Self {
            pipeline: d.take(),
            backend: d.take(),
            reset: d.take(),
            floor: d.take(),
            submitted_frames: d.take(),
            consumed_frames: d.take(),
            consumption_valid: d.flag(),
            source_begin_ns: d.take(),
            source_end_ns: d.take(),
            telemetry: Telemetry {
                valid: d.flag(),
                frame_begin: d.take(),
                frame_end: d.take(),
                source_begin_ns: d.take(),
                source_end_ns: d.take(),
                write_begin_ns: d.take(),
                write_end_ns: d.take(),
                rms_power: std::array::from_fn(|_| f64::from_bits(d.take())),
            },
            conversion_valid: d.flag(),
            clipped_samples: d.take(),
            conversion_at_ns: d.take(),
        };
        d.finish();
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field differs from every other, so a swapped or shifted word
    /// cannot round-trip.
    fn snapshot() -> Snapshot {
        Snapshot {
            sequence: 2,
            pipeline: 101,
            backend: 102,
            reset: 103,
            frame_floor: 104,
            frames: 105,
            processing_valid: true,
            source_begin_ns: 106,
            source_end_ns: 107,
            state: State::Tripped,
            candidate_observable: true,
            qualified_frames: 108,
            short_power_veto: true,
            attempt_required: true,
            trip_generation: 109,
            trip: Some(Trip {
                generation: 110,
                reason: Reason::Tone,
                channel: Some(1),
                frame: 111,
                at_ns: 112,
            }),
            control: Control {
                owner: Some((113 << 64) | 114),
                revision: 115,
                parent_reset_generation: 116,
                qualification_id: 117,
                mode: Mode::Armed,
                last_command: Some(Receipt {
                    command_id: 118,
                    result: control::Result::NotQualified,
                    pipeline: 119,
                    backend: 120,
                    reset: 121,
                    revision: 122,
                    parent_reset_generation: 123,
                    qualification_id: 124,
                }),
                command_high_water: 0,
            },
            identity: identity::Identity {
                graph: [125, 126, 127, 128],
                profile: [129, 130, 131, 132],
            },
            final_guard: true,
            activity_generation: 133,
            silent_frames: 134,
            last_active_end_frame: 135,
        }
    }

    #[test]
    fn guard_record_round_trips_every_field_and_each_flag_alone() {
        let full = snapshot();
        assert_eq!(Snapshot::decode(2, &full.encode()), Some(full));
        let flags: [fn(&mut Snapshot) -> &mut bool; 5] = [
            |s| &mut s.processing_valid,
            |s| &mut s.candidate_observable,
            |s| &mut s.short_power_veto,
            |s| &mut s.attempt_required,
            |s| &mut s.final_guard,
        ];
        for flag in flags {
            let mut one = snapshot();
            for other in flags {
                *other(&mut one) = false;
            }
            *flag(&mut one) = true;
            assert_eq!(Snapshot::decode(2, &one.encode()), Some(one));
        }
    }

    #[test]
    fn guard_record_round_trips_absent_and_every_enumerated_value() {
        let mut empty = snapshot();
        empty.trip = None;
        empty.control.owner = None;
        empty.control.last_command = None;
        assert_eq!(Snapshot::decode(2, &empty.encode()), Some(empty));
        let mut unattributed = snapshot();
        unattributed.trip.as_mut().unwrap().channel = None;
        assert_eq!(
            Snapshot::decode(2, &unattributed.encode()),
            Some(unattributed)
        );
        for reason in [
            Reason::Peak,
            Reason::Nonfinite,
            Reason::Coverage,
            Reason::Rms,
            Reason::ShortPower,
            Reason::LongPower,
            Reason::Tone,
            Reason::DcFrozen,
        ] {
            let mut value = snapshot();
            value.trip.as_mut().unwrap().reason = reason;
            assert_eq!(Snapshot::decode(2, &value.encode()), Some(value));
        }
        for state in [
            State::Inactive,
            State::Qualifying,
            State::Active,
            State::Tripped,
        ] {
            for mode in [Mode::Prearm, Mode::Armed] {
                let mut value = snapshot();
                value.state = state;
                value.control.mode = mode;
                assert_eq!(Snapshot::decode(2, &value.encode()), Some(value));
            }
        }
        for result in [
            control::Result::Applied,
            control::Result::Invalid,
            control::Result::StateMismatch,
            control::Result::NotQualified,
        ] {
            let mut value = snapshot();
            value.control.last_command.as_mut().unwrap().result = result;
            assert_eq!(Snapshot::decode(2, &value.encode()), Some(value));
        }
    }

    #[test]
    fn unpublished_enumeration_words_are_rejected_not_defaulted() {
        let words = snapshot().encode();
        let mut rejected = 0;
        for index in 0..GUARD_WORDS {
            let mut corrupt = words;
            corrupt[index] = u64::MAX;
            if Snapshot::decode(2, &corrupt).is_none() {
                rejected += 1;
            }
        }
        // State, trip reason, control mode and receipt result.
        assert_eq!(rejected, 4);
    }

    #[test]
    fn playback_record_round_trips_every_field() {
        let record = PlaybackRecord {
            pipeline: 201,
            backend: 202,
            reset: 203,
            floor: 204,
            submitted_frames: 205,
            consumed_frames: 206,
            consumption_valid: true,
            source_begin_ns: 207,
            source_end_ns: 208,
            telemetry: Telemetry {
                valid: true,
                frame_begin: 209,
                frame_end: 210,
                source_begin_ns: 211,
                source_end_ns: 212,
                write_begin_ns: 213,
                write_end_ns: 214,
                rms_power: std::array::from_fn(|i| 0.001 * (i + 1) as f64),
            },
            conversion_valid: true,
            clipped_samples: 215,
            conversion_at_ns: 216,
        };
        assert_eq!(PlaybackRecord::decode(&record.encode()), record);
        let only_conversion = PlaybackRecord {
            conversion_valid: true,
            ..Default::default()
        };
        assert_eq!(
            PlaybackRecord::decode(&only_conversion.encode()),
            only_conversion
        );
        assert_eq!(
            PlaybackRecord::decode(&[0; PLAYBACK_WORDS]),
            PlaybackRecord::default()
        );
    }

    #[test]
    fn atomic_read_is_bounded_repeatable_and_rejects_a_publication_in_progress() {
        let record = AtomicRecord::<3>::default();
        assert_eq!(record.read(), Some((0, [0; 3])));
        record.publish([7, 8, 9]);
        assert_eq!(record.read(), Some((2, [7, 8, 9])));
        assert_eq!(record.read(), Some((2, [7, 8, 9])));
        record.sequence.store(3, Ordering::SeqCst);
        assert!(record.read().is_none());
    }
}
