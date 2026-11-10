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

/// Published control state. The owner's command replay fence is private runtime
/// state and cannot be reconstructed from this snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlSnapshot {
    pub owner: Option<u128>,
    pub revision: u64,
    pub parent_reset_generation: u64,
    pub qualification_id: u64,
    pub mode: Mode,
    pub last_command: Option<Receipt>,
}

impl Control {
    pub(super) fn snapshot(&self) -> ControlSnapshot {
        ControlSnapshot {
            owner: self.owner,
            revision: self.revision,
            parent_reset_generation: self.parent_reset_generation,
            qualification_id: self.qualification_id,
            mode: self.mode,
            last_command: self.last_command,
        }
    }
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
            pipeline: self.stream.epoch,
            backend: self.stream.backend,
            reset: self.stream.reset,
            trip_generation: self.fault.generation,
            revision: self.control.revision,
        }
    }

    pub fn state(&self) -> State {
        if self.fault.trip.is_some() {
            State::Tripped
        } else if self.qualification.is_active() {
            // Ordinary mute preserves an already admitted fade-out trajectory.
            // candidate_observable is false and no new armed receipt is allowed.
            State::Active
        } else if self.configuration.is_none()
            || self.control.owner.is_none()
            || self.qualification.attempt_required()
        {
            State::Inactive
        } else {
            State::Qualifying
        }
    }

    /// No candidate or invented progress. Pause/resume needs a fresh attempt.
    pub fn pause(&mut self) {
        self.sync_backend();
        self.stream.pause();
        self.activity.interrupt();
        self.detector.break_continuity();
        self.qualification.require_attempt();
        self.candidate.interrupt();
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
            pipeline: self.stream.epoch,
            backend: self.stream.backend,
            reset: self.stream.reset,
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
        if command.expected != self.expected() || self.configuration.is_none() {
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
                self.stream.accepted_reset();
                self.shared
                    .reset
                    .store(self.stream.reset, std::sync::atomic::Ordering::SeqCst);
                self.activity.interrupt();
                self.detector.accepted_reset();
                self.fault.accepted_reset();
                self.qualification.begin_attempt();
                self.candidate.interrupt();
                self.control.mode = Mode::Prearm;
            }
            Operation::Context(Mode::Prearm) => {
                if !fresh_attempt
                    && (self.qualification.attempt_required()
                        || command.parent_reset_generation != self.control.parent_reset_generation
                        || self.control.mode == Mode::Armed)
                {
                    return Result::StateMismatch;
                }
                if fresh_attempt {
                    self.qualification.begin_attempt();
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
                    || self.detector.short_power_veto
                    || !self.qualification.is_active()
                    || !self.candidate.observable
                    || self.fault.trip.is_some()
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
