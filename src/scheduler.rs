//! Per-command debounce and dirty-run state machine.

use std::time::{Duration, Instant};

use crate::{RunId, RunTrigger};

/// The externally useful state of one command scheduler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerState {
    Idle,
    Debouncing {
        deadline: Instant,
        trigger: RunTrigger,
    },
    Running {
        run_id: RunId,
        dirty: bool,
    },
}

/// A run which the event loop must attempt to spawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledRun {
    pub id: RunId,
    pub trigger: RunTrigger,
}

/// State machine for one configured command.
///
/// The event loop calls [`Self::change`] for accepted watcher events,
/// [`Self::take_due_run`] as its clock advances, and [`Self::finish`] after a
/// child exits or fails to spawn. Calling `finish` for spawn failures releases
/// the scheduler so a later change can retry the command.
#[derive(Clone, Debug)]
pub struct Scheduler {
    debounce: Duration,
    next_run_id: RunId,
    state: SchedulerState,
}

impl Scheduler {
    /// Creates a scheduler using the current monotonic clock.
    pub fn new(debounce: Duration, initial_run: bool) -> Self {
        Self::new_at(debounce, initial_run, Instant::now())
    }

    /// Creates a scheduler at a caller-supplied time for tests or simulations.
    pub fn new_at(debounce: Duration, initial_run: bool, now: Instant) -> Self {
        let state = if initial_run {
            SchedulerState::Debouncing {
                deadline: now,
                trigger: RunTrigger::Initial,
            }
        } else {
            SchedulerState::Idle
        };
        Self {
            debounce,
            next_run_id: 1,
            state,
        }
    }

    pub fn state(&self) -> &SchedulerState {
        &self.state
    }

    /// Returns the deadline to use in the event loop timeout calculation.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            SchedulerState::Debouncing { deadline, .. } => Some(deadline),
            SchedulerState::Idle | SchedulerState::Running { .. } => None,
        }
    }

    /// Records an accepted filesystem change, including overflow/rescan.
    pub fn change(&mut self, now: Instant) {
        match &mut self.state {
            SchedulerState::Idle => {
                self.state = SchedulerState::Debouncing {
                    deadline: now + self.debounce,
                    trigger: RunTrigger::Filesystem,
                }
            }
            SchedulerState::Debouncing { deadline, trigger } => {
                // A startup event must not postpone the promised initial run.
                if *trigger == RunTrigger::Filesystem {
                    *deadline = now + self.debounce;
                }
            }
            SchedulerState::Running { dirty, .. } => *dirty = true,
        }
    }

    /// Takes one due run. It cannot return another run until this one finishes.
    pub fn take_due_run(&mut self, now: Instant) -> Option<ScheduledRun> {
        let (deadline, trigger) = match &self.state {
            SchedulerState::Debouncing { deadline, trigger } => (*deadline, trigger.clone()),
            SchedulerState::Idle | SchedulerState::Running { .. } => return None,
        };
        if now < deadline {
            return None;
        }

        let id = self.next_run_id;
        self.next_run_id = self.next_run_id.checked_add(1).expect("run id overflow");
        self.state = SchedulerState::Running {
            run_id: id,
            dirty: false,
        };
        Some(ScheduledRun { id, trigger })
    }

    /// Marks a started run as complete.
    ///
    /// Returns false for a stale or duplicate completion. Dirty changes create
    /// exactly one follow-up run due immediately, without another debounce.
    pub fn finish(&mut self, run_id: RunId, now: Instant) -> bool {
        let SchedulerState::Running {
            run_id: active_id,
            dirty,
        } = self.state
        else {
            return false;
        };
        if run_id != active_id {
            return false;
        }

        self.state = if dirty {
            SchedulerState::Debouncing {
                deadline: now,
                trigger: RunTrigger::Filesystem,
            }
        } else {
            SchedulerState::Idle
        };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> Instant {
        Instant::now()
    }

    #[test]
    fn initial_run_is_due_once_immediately() {
        let now = clock();
        let mut scheduler = Scheduler::new_at(Duration::from_millis(150), true, now);
        assert_eq!(
            scheduler.take_due_run(now),
            Some(ScheduledRun {
                id: 1,
                trigger: RunTrigger::Initial
            })
        );
        assert_eq!(scheduler.take_due_run(now), None);
    }

    #[test]
    fn changes_debounce_until_the_last_change_is_quiet() {
        let now = clock();
        let mut scheduler = Scheduler::new_at(Duration::from_millis(150), false, now);
        scheduler.change(now);
        scheduler.change(now + Duration::from_millis(100));
        assert_eq!(
            scheduler.take_due_run(now + Duration::from_millis(249)),
            None
        );
        assert_eq!(
            scheduler.take_due_run(now + Duration::from_millis(250)),
            Some(ScheduledRun {
                id: 1,
                trigger: RunTrigger::Filesystem
            })
        );
    }

    #[test]
    fn change_during_run_makes_one_immediate_followup_without_overlap() {
        let now = clock();
        let mut scheduler = Scheduler::new_at(Duration::from_secs(1), false, now);
        scheduler.change(now);
        let first = scheduler
            .take_due_run(now + Duration::from_secs(1))
            .unwrap();
        scheduler.change(now + Duration::from_secs(2));
        scheduler.change(now + Duration::from_secs(3));
        assert_eq!(scheduler.take_due_run(now + Duration::from_secs(3)), None);
        assert!(scheduler.finish(first.id, now + Duration::from_secs(4)));
        assert_eq!(
            scheduler.take_due_run(now + Duration::from_secs(4)),
            Some(ScheduledRun {
                id: 2,
                trigger: RunTrigger::Filesystem
            })
        );
        assert_eq!(scheduler.take_due_run(now + Duration::from_secs(4)), None);
    }

    #[test]
    fn clean_completion_becomes_idle_and_later_changes_run_normally() {
        let now = clock();
        let mut scheduler = Scheduler::new_at(Duration::ZERO, false, now);
        scheduler.change(now);
        let first = scheduler.take_due_run(now).unwrap();
        assert!(scheduler.finish(first.id, now));
        assert_eq!(scheduler.state(), &SchedulerState::Idle);
        scheduler.change(now + Duration::from_secs(1));
        assert_eq!(
            scheduler
                .take_due_run(now + Duration::from_secs(1))
                .unwrap()
                .id,
            2
        );
    }

    #[test]
    fn spawn_failure_is_a_completion_and_later_change_retries() {
        let now = clock();
        let mut scheduler = Scheduler::new_at(Duration::ZERO, false, now);
        scheduler.change(now);
        let failed_spawn = scheduler.take_due_run(now).unwrap();
        assert!(scheduler.finish(failed_spawn.id, now));
        assert_eq!(scheduler.state(), &SchedulerState::Idle);
        scheduler.change(now + Duration::from_secs(1));
        assert_eq!(
            scheduler
                .take_due_run(now + Duration::from_secs(1))
                .unwrap()
                .id,
            2
        );
    }

    #[test]
    fn stale_completion_cannot_finish_the_current_run() {
        let now = clock();
        let mut scheduler = Scheduler::new_at(Duration::ZERO, false, now);
        scheduler.change(now);
        let run = scheduler.take_due_run(now).unwrap();
        assert!(!scheduler.finish(run.id + 1, now));
        assert!(
            matches!(scheduler.state(), SchedulerState::Running { run_id, .. } if *run_id == run.id)
        );
    }

    #[test]
    fn startup_change_does_not_delay_initial_run() {
        let now = clock();
        let mut scheduler = Scheduler::new_at(Duration::from_secs(1), true, now);
        scheduler.change(now);
        assert_eq!(
            scheduler.take_due_run(now).unwrap().trigger,
            RunTrigger::Initial
        );
    }
}
