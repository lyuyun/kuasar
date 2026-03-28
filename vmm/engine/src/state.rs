/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxState {
    Creating,
    Running,
    // Paused — removed from Epic 1; no backend supports VM pause/resume yet.
    // Re-introduce together with Vmm::pause() / Vmm::resume() in a future epic.
    Stopped,
    Deleted,
}

impl SandboxState {
    /// Returns Err(InvalidState) if the transition is not allowed.
    pub fn transition(&self, event: StateEvent) -> Result<SandboxState> {
        match (self, event) {
            (SandboxState::Creating, StateEvent::StartSucceeded) => Ok(SandboxState::Running),
            (SandboxState::Creating, StateEvent::StartFailed) => Ok(SandboxState::Stopped),
            (SandboxState::Running, StateEvent::Stop) => Ok(SandboxState::Stopped),
            (SandboxState::Stopped, StateEvent::Delete) => Ok(SandboxState::Deleted),
            // Force-delete from any state
            (_, StateEvent::ForceDelete) => Ok(SandboxState::Deleted),
            (from, event) => Err(Error::InvalidState(format!(
                "cannot {:?} a sandbox in state {:?}",
                event, from
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StateEvent {
    StartSucceeded,
    StartFailed,
    Stop,
    Delete,
    ForceDelete,
    // Pause / Resume — reserved for future epic; requires Vmm::pause() / Vmm::resume().
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creating_to_running() {
        let s = SandboxState::Creating;
        assert_eq!(
            s.transition(StateEvent::StartSucceeded).unwrap(),
            SandboxState::Running
        );
    }

    #[test]
    fn creating_to_stopped_on_failure() {
        let s = SandboxState::Creating;
        assert_eq!(
            s.transition(StateEvent::StartFailed).unwrap(),
            SandboxState::Stopped
        );
    }

    #[test]
    fn running_to_stopped() {
        let s = SandboxState::Running;
        assert_eq!(
            s.transition(StateEvent::Stop).unwrap(),
            SandboxState::Stopped
        );
    }

    #[test]
    fn stopped_to_deleted() {
        let s = SandboxState::Stopped;
        assert_eq!(
            s.transition(StateEvent::Delete).unwrap(),
            SandboxState::Deleted
        );
    }

    #[test]
    fn force_delete_from_any_state() {
        for state in &[
            SandboxState::Creating,
            SandboxState::Running,
            SandboxState::Stopped,
            SandboxState::Deleted,
        ] {
            assert_eq!(
                state.transition(StateEvent::ForceDelete).unwrap(),
                SandboxState::Deleted
            );
        }
    }

    #[test]
    fn invalid_transition_running_to_deleted() {
        let s = SandboxState::Running;
        assert!(matches!(
            s.transition(StateEvent::Delete),
            Err(Error::InvalidState(_))
        ));
    }
}
