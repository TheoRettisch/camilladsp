//! Owner-side state grouped by lifetime. Stream interruption, epoch changes and
//! accepted reset deliberately have different preservation rules.

use super::{FRAMES, Reason, Trip, bump, monotonic_ns, policy};

#[derive(Debug, Default)]
pub(super) struct StreamState {
    pub epoch: u64,
    pub backend: u64,
    pub reset: u64,
    pub frames: u64,
    pub frame_floor: u64,
    pub processing_valid: bool,
    pub source_begin_ns: u64,
    pub source_end_ns: u64,
}

impl StreamState {
    pub fn new_pipeline(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.restart_progress();
    }

    pub fn new_backend(&mut self, backend: u64) {
        self.backend = backend;
        self.restart_progress();
    }

    fn restart_progress(&mut self) {
        self.frames = 0;
        self.frame_floor = 0;
        self.clear_evidence();
    }

    fn clear_evidence(&mut self) {
        self.processing_valid = false;
        self.source_begin_ns = 0;
        self.source_end_ns = 0;
    }

    pub fn pause(&mut self) {
        // Retain historical position/timestamps, explicitly invalid for release.
        self.processing_valid = false;
    }

    pub fn accepted_reset(&mut self) {
        self.reset = bump(self.reset);
        self.frame_floor = self.frames;
        self.clear_evidence();
    }
}

#[derive(Debug, Default)]
pub(super) struct DetectorState {
    pub history: [policy::History; 2],
    pub short_power_veto: bool,
}

impl DetectorState {
    pub fn break_continuity(&mut self) {
        for history in &mut self.history {
            history.break_continuity();
        }
        self.short_power_veto = false;
    }

    pub fn accepted_reset(&mut self) {
        // Only accepted reset clears accumulated short/long power.
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
pub(super) struct FaultState {
    pub trip: Option<Trip>,
    pub generation: u64,
}

impl FaultState {
    pub fn latch(&mut self, reason: Reason, channel: Option<usize>, frame: u64) {
        if self.trip.is_none() {
            self.generation = bump(self.generation);
            self.trip = Some(Trip {
                generation: self.generation,
                reason,
                channel,
                frame,
                at_ns: monotonic_ns(),
            });
        }
    }

    pub fn accepted_reset(&mut self) {
        self.trip = None; // The process-lifetime trip generation survives.
    }
}

#[derive(Debug, Default)]
pub(super) struct CandidateObservation {
    pub observable: bool,
    pub last_main_mute: Option<bool>,
}

impl CandidateObservation {
    pub fn new_configuration(&mut self) {
        *self = Self::default();
    }

    pub fn interrupt(&mut self) {
        // Pause, backend retirement and reset retain the last applied mute.
        self.observable = false;
    }
}

/// Local admission is independent of parent Armed context and observability.
/// Active retains the published frame credit, which becomes zero during an
/// ordinary masked fade-out without revoking the already admitted trajectory.
#[derive(Clone, Copy, Debug, Default)]
pub(super) enum Qualification {
    #[default]
    AwaitingAttempt,
    AwaitingBaseline,
    Counting {
        frames: u64,
    },
    Active {
        frames: u64,
    },
}

impl Qualification {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active { .. })
    }

    pub fn attempt_required(self) -> bool {
        matches!(self, Self::AwaitingAttempt)
    }

    pub fn frames(self) -> u64 {
        match self {
            Self::Counting { frames } | Self::Active { frames } => frames,
            Self::AwaitingAttempt | Self::AwaitingBaseline => 0,
        }
    }

    pub fn require_attempt(&mut self) {
        *self = Self::AwaitingAttempt;
    }

    pub fn begin_attempt(&mut self) {
        *self = Self::AwaitingBaseline;
    }

    pub fn restart_window(&mut self) {
        if !self.attempt_required() {
            *self = Self::AwaitingBaseline;
        }
    }

    pub fn masked_candidate(&mut self) {
        if self.is_active() {
            *self = Self::Active { frames: 0 };
        } else {
            self.restart_window();
        }
    }

    /// Called only for observable, untripped candidates under an owned attempt.
    pub fn observe_candidate(&mut self, required_frames: u64) {
        *self = match *self {
            Self::AwaitingBaseline => Self::Counting { frames: 0 },
            Self::Counting { frames } => {
                let frames = frames.checked_add(FRAMES as u64).unwrap();
                if frames >= required_frames {
                    Self::Active { frames }
                } else {
                    Self::Counting { frames }
                }
            }
            other => other,
        };
    }
}

/// Forwarded-output statistics have no authority over detector power history.
#[derive(Debug, Default)]
pub(super) struct OutputActivity {
    pub generation: u64,
    pub silent_frames: u64,
    pub last_active_end_frame: u64,
}

impl OutputActivity {
    pub fn interrupt(&mut self) {
        self.generation = bump(self.generation);
        self.silent_frames = 0;
        self.last_active_end_frame = 0;
    }
}
