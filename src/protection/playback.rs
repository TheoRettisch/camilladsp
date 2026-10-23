//! Exact direct-PCM ring accounting. Submission and consumption are distinct.

use super::{Shared, Stamp, bump};
use std::sync::{Arc, atomic::Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub sequence: u64,
    pub pipeline: u64,
    pub backend: u64,
    pub reset: u64,
    pub submitted_frames: u64,
    pub frames: u64,
    pub valid: bool,
    pub source_begin_ns: u64,
    pub source_end_ns: u64,
}

#[derive(Debug, Default)]
struct Ring {
    submitted: u64,
    consumed: u64,
    previous_read: Option<u64>,
    source_begin: u64,
    source_end: u64,
    valid: bool,
}

impl Ring {
    fn write(&mut self, frames: usize, remaining: usize) -> Result<(), &'static str> {
        if frames > remaining {
            return Err("PCM returned more frames than requested");
        }
        self.submitted = self
            .submitted
            .checked_add(frames as u64)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or("PCM frame counter exhausted")?;
        Ok(())
    }

    fn observe(
        &mut self,
        size: u64,
        available: i64,
        begin: u64,
        end: u64,
        floor: u64,
    ) -> Result<(), &'static str> {
        if available < 0
            || available as u64 > size
            || begin == 0
            || end < begin
            || self.previous_read.is_some_and(|previous| begin < previous)
        {
            return Err("Invalid PCM availability or observation time");
        }
        let consumed = self
            .submitted
            .checked_sub(size - available as u64)
            .ok_or("PCM contains unaccounted frames")?;
        if consumed < self.consumed {
            return Err("PCM consumption regressed");
        }
        if let Some(previous) = self.previous_read
            && consumed > self.consumed
            && consumed > floor
        {
            self.source_begin = previous;
            self.source_end = end;
            self.valid = true;
        }
        self.consumed = consumed;
        self.previous_read = Some(begin);
        Ok(())
    }

    fn reset_evidence(&mut self) {
        // PCM accounting survives policy reset, including outstanding frames.
        self.previous_read = None;
        self.source_begin = 0;
        self.source_end = 0;
        self.valid = false;
    }
}

/// One playback-thread owner. Every PCM discontinuity invalidates the shared
/// epoch before recovery; processing then establishes a fresh frame origin.
pub struct Playback {
    shared: Arc<Shared>,
    pipeline: u64,
    backend: u64,
    reset: u64,
    floor: u64,
    ring: Ring,
    telemetry: [u64; 31],
    conversion_valid: bool,
    conversion_at: u64,
}

impl Playback {
    pub fn new(shared: Arc<Shared>) -> Self {
        let owner = Self {
            pipeline: shared.epoch.load(Ordering::SeqCst),
            backend: shared.backend.load(Ordering::SeqCst),
            reset: shared.reset.load(Ordering::SeqCst),
            shared,
            floor: 0,
            ring: Ring::default(),
            telemetry: [0; 31],
            conversion_valid: false,
            conversion_at: 0,
        };
        owner.publish();
        owner
    }

    pub fn retire(&mut self) {
        self.backend = bump(self.shared.backend.load(Ordering::SeqCst));
        self.shared.backend.store(self.backend, Ordering::SeqCst);
        self.pipeline = self.shared.epoch.load(Ordering::SeqCst);
        self.reset = self.shared.reset.load(Ordering::SeqCst);
        self.floor = 0;
        self.ring = Ring::default();
        self.telemetry = [0; 31];
        self.conversion_valid = false;
        self.conversion_at = 0;
        self.publish();
        // Coalesced wakeup, with no waiting for capture or playback capacity.
        let _ = self.shared.backend_wakeup.try_send(());
    }

    pub fn pipeline_changed(&self) -> bool {
        self.shared.epoch.load(Ordering::SeqCst) != self.pipeline
    }

    pub fn sync_reset(&mut self) -> bool {
        let Some(guard) = self.shared.guard_snapshot() else {
            return false;
        };
        if guard.pipeline != self.pipeline || guard.backend != self.backend {
            return false;
        }
        if guard.reset != self.reset {
            self.reset = guard.reset;
            self.floor = guard.frame_floor;
            self.ring.reset_evidence();
            self.telemetry = [0; 31];
            self.publish();
        }
        true
    }

    pub fn accepts(&self, stamp: &Stamp) -> bool {
        stamp.is_stream_current()
            && stamp.epoch == self.pipeline
            && stamp.backend == self.backend
            && stamp.frame_begin == self.ring.submitted
    }

    pub fn permits_prepared(&self) -> bool {
        self.ring.submitted == 0
    }

    pub fn converted(&mut self, clipped: usize) {
        let count = self
            .shared
            .conversion_count
            .load(Ordering::SeqCst)
            .checked_add(clipped as u64)
            .filter(|n| *n <= i64::MAX as u64)
            .expect("Protection conversion counter exhausted");
        self.shared.conversion_count.store(count, Ordering::SeqCst);
        self.conversion_at = super::monotonic_ns();
        self.conversion_valid = true;
        self.publish();
    }

    pub fn completed(&mut self, stamp: &Stamp, begin: u64, end: u64) {
        if !stamp.is_current()
            || stamp.frame_end != self.ring.submitted
            || !self.sync_reset()
            || stamp.frame_begin < self.floor
        {
            return;
        }
        self.telemetry[..7].copy_from_slice(&[
            1,
            stamp.frame_begin,
            stamp.frame_end,
            stamp.source_begin_ns,
            stamp.source_end_ns,
            begin,
            end,
        ]);
        for (slot, power) in self.telemetry[7..].iter_mut().zip(stamp.rms_power) {
            *slot = power.to_bits();
        }
        self.publish();
    }

    pub fn written(&mut self, frames: usize, remaining: usize) -> Result<(), &'static str> {
        self.ring.write(frames, remaining)?;
        self.publish();
        Ok(())
    }

    pub fn observe(
        &mut self,
        size: u64,
        available: i64,
        begin: u64,
        end: u64,
    ) -> Result<(), &'static str> {
        if !self.sync_reset() {
            // A concurrently publishing guard is not a PCM discontinuity.
            // Keep exact write accounting and defer the source baseline. The
            // epoch fence rejects this old record until the owner catches up.
            self.ring.reset_evidence();
            self.publish();
            return Ok(());
        }
        self.ring.observe(size, available, begin, end, self.floor)?;
        self.publish();
        Ok(())
    }

    fn publish(&self) {
        let mut record = [0; 43];
        record[..9].copy_from_slice(&[
            self.pipeline,
            self.backend,
            self.reset,
            self.ring.submitted,
            self.ring.consumed,
            self.ring.valid as u64,
            self.ring.source_begin,
            self.ring.source_end,
            self.floor,
        ]);
        record[9..40].copy_from_slice(&self.telemetry);
        record[40] = self.conversion_valid as u64;
        record[41] = self.shared.conversion_count.load(Ordering::SeqCst);
        record[42] = self.conversion_at;
        self.shared.playback_record.publish(record);
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.retire();
    }
}

impl Shared {
    pub fn playback_snapshot(&self) -> Option<Progress> {
        let guard = self.guard_snapshot()?;
        let (sequence, v) = self.playback_record.read()?;
        if sequence == 0 || v[0] != guard.pipeline || v[1] != guard.backend {
            return None;
        }
        let current_reset = v[2] == guard.reset;
        // A paused reset receipt stays readable. Old playback source times are
        // invalidated, never relabelled as evidence of the new reset lifetime.
        let current = self.guard_snapshot()?;
        if (guard.pipeline, guard.backend, guard.reset)
            != (current.pipeline, current.backend, current.reset)
        {
            return None;
        }
        Some(Progress {
            sequence,
            pipeline: v[0],
            backend: v[1],
            reset: guard.reset,
            submitted_frames: v[3],
            frames: v[4],
            valid: current_reset && v[5] != 0 && v[4] > guard.frame_floor,
            source_begin_ns: if current_reset { v[6] } else { 0 },
            source_end_ns: if current_reset { v[7] } else { 0 },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protection::{
        Guard,
        control::{Command, Mode, Operation, Result as ControlResult},
        tests::{chunk, configuration},
    };

    fn command(guard: &Guard, id: u64, reset: bool) -> Command {
        Command {
            id,
            expected: guard.expected(),
            owner: 7,
            parent_reset_generation: u64::from(reset),
            qualification_id: id,
            operation: if reset {
                Operation::Reset
            } else {
                Operation::Context(Mode::Prearm)
            },
        }
    }

    #[test]
    fn paused_reset_receipt_does_not_relabel_backend_progress_or_drop_pcm() {
        let shared = Arc::new(Shared::default());
        let mut guard = Guard::new(shared.clone());
        guard.install(&configuration()).unwrap();
        guard.control_only(command(&guard, 1, false));
        let mut playback = Playback::new(shared.clone());
        for at in [10, 20] {
            let mut audio = chunk();
            guard.process(&mut audio);
            assert!(playback.accepts(audio.protection.as_ref().unwrap()));
            playback.written(256, 256).unwrap();
            playback.observe(1024, 1024, at, at + 1).unwrap();
        }
        let old = shared.playback_snapshot().unwrap();
        assert!(old.valid);
        guard.control_only(command(&guard, 2, true));
        let reset = shared.guard_snapshot().unwrap();
        let status = shared.playback_snapshot().unwrap();
        assert_eq!(
            reset.control.last_command.unwrap().result,
            ControlResult::Applied
        );
        assert_eq!((status.frames, status.submitted_frames), (512, 512));
        assert_eq!((status.backend, status.reset), (old.backend, old.reset + 1));
        assert!(!status.valid);
        assert_eq!((status.source_begin_ns, status.source_end_ns), (0, 0));
        assert_eq!(status.sequence, old.sequence);
        assert!(playback.sync_reset());
        assert_eq!(playback.floor, 512);
        assert_eq!(playback.ring.submitted, 512);
        assert!(!playback.ring.valid);
    }

    #[test]
    fn backend_retirement_fences_queue_and_commands_preserving_trip() {
        let shared = Arc::new(Shared::default());
        let mut guard = Guard::new(shared.clone());
        guard.install(&configuration()).unwrap();
        guard.control_only(command(&guard, 1, false));
        let mut audio = chunk();
        audio.waveforms[1][2] = 1.0;
        guard.process(&mut audio);
        let trip = guard.trip();
        let old_command = command(&guard, 2, true);
        let mut playback = Playback::new(shared.clone());
        playback.written(100, 256).unwrap();
        playback.retire();
        assert!(!audio.protection.as_ref().unwrap().is_stream_current());
        assert!(shared.guard_snapshot().is_none());
        shared.backend_wakeups().try_recv().unwrap();
        guard.backend_wakeup();
        guard.control_only(old_command);
        assert_eq!(
            guard.control.last_command.unwrap().result,
            ControlResult::StateMismatch
        );
        assert_eq!(guard.trip(), trip);
        let guard_status = shared.guard_snapshot().unwrap();
        let status = shared.playback_snapshot().unwrap();
        assert_eq!(
            (guard_status.frames, status.frames, status.submitted_frames),
            (0, 0, 0)
        );
        assert!(!status.valid && !guard_status.processing_valid);
        assert_eq!(guard_status.qualified_frames, 0);
        let mut fresh = chunk();
        guard.process(&mut fresh);
        assert!(playback.accepts(fresh.protection.as_ref().unwrap()));
    }

    #[test]
    fn additional_writes_into_stalled_ring_do_not_renew_consumption() {
        let mut ring = Ring::default();
        ring.write(1000, 1000).unwrap();
        ring.observe(1024, 524, 10, 11, 0).unwrap();
        assert_eq!(ring.consumed, 500);
        assert!(!ring.valid); // first observation is only a baseline
        ring.write(256, 256).unwrap();
        ring.observe(1024, 268, 20, 21, 0).unwrap();
        assert_eq!(ring.consumed, 500);
        assert!(!ring.valid);
        ring.observe(1024, 524, 30, 31, 0).unwrap();
        assert_eq!(ring.consumed, 756);
        assert_eq!((ring.source_begin, ring.source_end), (20, 31));
        ring.observe(1024, 524, 1_000_000_000, 1_000_000_001, 0)
            .unwrap();
        assert_eq!((ring.source_begin, ring.source_end), (20, 31));
    }

    #[test]
    fn partial_and_zero_writes_account_only_actual_returned_frames() {
        let mut ring = Ring::default();
        for (frames, remaining) in [(100, 256), (0, 156), (50, 156), (106, 106)] {
            ring.write(frames, remaining).unwrap();
        }
        assert_eq!(ring.submitted, 256);
        assert!(ring.write(2, 1).is_err());
        assert_eq!(ring.submitted, 256);
    }

    #[test]
    fn invalid_availability_unaccounted_frames_and_regression_fail() {
        let mut ring = Ring::default();
        assert!(ring.observe(1024, 1023, 1, 2, 0).is_err());
        assert!(ring.observe(1024, -1, 1, 2, 0).is_err());
        assert!(ring.observe(1024, 1025, 1, 2, 0).is_err());
        ring.write(256, 256).unwrap();
        ring.observe(1024, 1024, 10, 11, 0).unwrap();
        assert!(ring.observe(1024, 1023, 12, 13, 0).is_err());
    }

    #[test]
    fn reset_preserves_outstanding_pcm_and_requires_progress_past_floor() {
        let mut ring = Ring::default();
        ring.write(512, 512).unwrap();
        ring.observe(1024, 768, 10, 11, 0).unwrap();
        ring.reset_evidence();
        assert_eq!((ring.submitted, ring.consumed), (512, 256));
        ring.observe(1024, 1024, 20, 21, 512).unwrap();
        assert!(!ring.valid);
        ring.write(256, 256).unwrap();
        ring.observe(1024, 1024, 30, 31, 512).unwrap();
        assert!(ring.valid);
        assert_eq!(
            (ring.consumed, ring.source_begin, ring.source_end),
            (768, 20, 31)
        );
    }
}
