//! Processing-owner lifecycle. These are internal commands and snapshots, not
//! the version-1 wire protocol: process/backend identity and parent admission
//! must be bound by that integration before any production activation.

use super::{Guard, bump};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum State {
    Inactive,
    Qualifying,
    Active,
    Tripped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Mode {
    Prearm,
    Armed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    pub pipeline: u64,
    pub backend: u64,
    pub reset: u64,
    pub trip_generation: u64,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Context(Mode),
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: u64,
    pub expected: Expected,
    pub owner: u128,
    pub parent_reset_generation: u64,
    pub qualification_id: u64,
    pub operation: Operation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum Result {
    Applied = 1,
    Invalid,
    StateMismatch,
    NotQualified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub command_id: u64,
    pub result: Result,
    pub pipeline: u64,
    pub backend: u64,
    pub reset: u64,
    pub revision: u64,
    pub parent_reset_generation: u64,
    pub qualification_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
    pub owner: Option<u128>,
    pub revision: u64,
    pub parent_reset_generation: u64,
    pub qualification_id: u64,
    pub mode: Mode,
    pub last_command: Option<Receipt>,
    pub(super) command_high_water: u64,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            owner: None,
            revision: 0,
            parent_reset_generation: 0,
            qualification_id: 0,
            mode: Mode::Prearm,
            last_command: None,
            command_high_water: 0,
        }
    }
}

impl Guard {
    pub fn expected(&self) -> Expected {
        Expected {
            pipeline: self.epoch,
            backend: self.backend,
            reset: self.reset,
            trip_generation: self.trip_generation,
            revision: self.control.revision,
        }
    }

    pub fn state(&self) -> State {
        if self.trip.is_some() {
            State::Tripped
        } else if self.active && !self.activation_inhibited() {
            // Ordinary mute preserves an already admitted fade-out trajectory.
            // candidate_observable is false and no new armed receipt is allowed.
            State::Active
        } else if self.parameters.is_none() || self.control.owner.is_none() || self.attempt_required
        {
            State::Inactive
        } else {
            State::Qualifying
        }
    }

    pub(super) fn invalidate_qualification(&mut self) {
        self.active = false;
        self.qualified_frames = 0;
        self.qualification_baseline = false;
        self.attempt_required = true;
    }

    /// No candidate or invented progress. Pause/resume needs a fresh attempt.
    pub fn pause(&mut self) {
        self.sync_backend();
        self.invalidate_activity();
        self.invalidate_qualification();
        self.candidate_observable = false;
        self.processing_valid = false;
        self.publish();
    }

    /// Explicit wakeup service on the same owner, including capture pauses.
    pub fn control_only(&mut self, command: Command) {
        self.sync_backend();
        self.apply_control(command, false);
        self.publish();
    }

    pub(super) fn apply_control(&mut self, command: Command, has_candidate: bool) {
        let result = self.try_control(command, has_candidate);
        self.control.last_command = Some(Receipt {
            command_id: command.id,
            result,
            pipeline: self.epoch,
            backend: self.backend,
            reset: self.reset,
            revision: self.control.revision,
            parent_reset_generation: self.control.parent_reset_generation,
            qualification_id: self.control.qualification_id,
        });
    }

    fn try_control(&mut self, command: Command, has_candidate: bool) -> Result {
        let same_owner = self.control.owner == Some(command.owner);
        if command.owner == 0
            || command.id == 0
            || [
                command.id,
                command.parent_reset_generation,
                command.qualification_id,
            ]
            .iter()
            .any(|n| *n > i64::MAX as u64)
        {
            return Result::Invalid;
        }
        if same_owner && command.id <= self.control.command_high_water {
            return Result::StateMismatch;
        }
        // A terminally rejected current-owner command is consumed too.
        if same_owner {
            self.control.command_high_water = command.id;
        }
        if command.expected != self.expected() || self.parameters.is_none() {
            return Result::StateMismatch;
        }
        if same_owner
            && (command.qualification_id < self.control.qualification_id
                || command.parent_reset_generation < self.control.parent_reset_generation)
        {
            return Result::StateMismatch;
        }
        let fresh_attempt = !same_owner || command.qualification_id > self.control.qualification_id;
        match command.operation {
            Operation::Reset => {
                if !same_owner
                    || !fresh_attempt
                    || command.parent_reset_generation <= self.control.parent_reset_generation
                {
                    return Result::StateMismatch;
                }
                self.reset = bump(self.reset);
                self.shared
                    .reset
                    .store(self.reset, std::sync::atomic::Ordering::SeqCst);
                self.frame_floor = self.frames;
                self.invalidate_activity();
                self.processing_valid = false;
                self.source_begin_ns = 0;
                self.source_end_ns = 0;
                self.trip = None; // The process-lifetime trip generation survives.
                self.invalidate_qualification();
                self.candidate_observable = false;
                self.attempt_required = false;
                self.control.mode = Mode::Prearm;
            }
            Operation::Context(Mode::Prearm) => {
                if !fresh_attempt
                    && (self.attempt_required
                        || command.parent_reset_generation != self.control.parent_reset_generation
                        || self.control.mode == Mode::Armed)
                {
                    return Result::StateMismatch;
                }
                if fresh_attempt {
                    self.invalidate_qualification();
                    self.attempt_required = false;
                }
                self.control.mode = Mode::Prearm;
            }
            Operation::Context(Mode::Armed) => {
                if !same_owner
                    || fresh_attempt
                    || command.parent_reset_generation != self.control.parent_reset_generation
                {
                    return Result::StateMismatch;
                }
                if !has_candidate
                    || self.activation_inhibited()
                    || !self.active
                    || !self.candidate_observable
                    || self.attempt_required
                    || self.trip.is_some()
                {
                    return Result::NotQualified;
                }
                self.control.mode = Mode::Armed;
            }
        }
        self.control.owner = Some(command.owner);
        self.control.command_high_water = command.id;
        self.control.revision = bump(self.control.revision);
        self.control.parent_reset_generation = command.parent_reset_generation;
        self.control.qualification_id = command.qualification_id;
        Result::Applied
    }
}
