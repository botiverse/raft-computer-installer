//! What `raft-computer-launch.exe` (Windows) does when the Computer service
//! it started exits. Task Scheduler cannot tell exit codes apart, so the
//! launcher, not the task, owns the restart policy: the exit-code table and
//! the failed-start cap come from `contract/supervised-exit-policy.json`,
//! a byte-identical copy of the fixture slock's `exitCodeFor` is tested
//! against, so the two sides cannot drift.
//!
//! A start counts as failed, mechanically, when the service exits (or cannot
//! be spawned) without having written the readiness file for its pid.
//! Duration plays no part. Reaching readiness resets the count.
use serde::Deserialize;

pub const POLICY_JSON: &str = include_str!("../contract/supervised-exit-policy.json");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    format_version: u32,
    launcher: LauncherPolicy,
    breaker: Breaker,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LauncherPolicy {
    pub relaunch_at_once: Vec<i32>,
    pub stay_down: Vec<i32>,
    pub otherwise: String,
    pub backoff_ms: Vec<u64>,
    pub ready_file: String,
    pub breaker_receipt: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Breaker {
    consecutive_failed_starts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub launcher: LauncherPolicy,
    pub failed_start_limit: u32,
}

/// The embedded contract. Panics only if the committed fixture is broken,
/// which the tests below rule out.
pub fn policy() -> Policy {
    let fixture: Fixture = serde_json::from_str(POLICY_JSON).expect("supervised-exit-policy.json");
    assert_eq!(fixture.format_version, 1, "unknown exit policy format");
    assert_eq!(fixture.launcher.otherwise, "crash");
    Policy {
        launcher: fixture.launcher,
        failed_start_limit: fixture.breaker.consecutive_failed_starts,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Relaunch {
        delay_ms: u64,
    },
    /// Exit 0 and stay down (an intentional stop, 77, 78).
    StayDown,
    /// Too many failed starts: write the breaker receipt, delete the task
    /// definition, exit 0.
    Trip,
}

#[derive(Default, Debug)]
pub struct State {
    pub failed_starts: u32,
}

impl State {
    /// `code` is None when the child could not be spawned or reported no code.
    pub fn on_exit(&mut self, policy: &Policy, code: Option<i32>, reached_ready: bool) -> Decision {
        if reached_ready {
            self.failed_starts = 0;
        }
        if let Some(code) = code {
            if policy.launcher.relaunch_at_once.contains(&code) {
                return Decision::Relaunch { delay_ms: 0 };
            }
            if policy.launcher.stay_down.contains(&code) {
                return Decision::StayDown;
            }
        }
        if !reached_ready {
            self.failed_starts += 1;
            if self.failed_starts >= policy.failed_start_limit {
                return Decision::Trip;
            }
        }
        let backoff = &policy.launcher.backoff_ms;
        let index = (self.failed_starts.max(1) as usize - 1).min(backoff.len().saturating_sub(1));
        Decision::Relaunch {
            delay_ms: backoff.get(index).copied().unwrap_or(60_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn the_launcher_reads_exit_codes_exactly_as_slock_maps_them_for_windows_tasks() {
        let fixture: Value = serde_json::from_str(POLICY_JSON).unwrap();
        let map = &fixture["exitCodeFor"]["windows-task"];
        let policy = policy();
        for class in fixture["classes"].as_array().unwrap() {
            let class = class.as_str().unwrap();
            let code = map[class].as_i64().unwrap() as i32;
            let decision = State::default().on_exit(&policy, Some(code), false);
            match class {
                "relaunch" => assert_eq!(decision, Decision::Relaunch { delay_ms: 0 }, "{class}"),
                "crash" => assert!(
                    matches!(decision, Decision::Relaunch { delay_ms } if delay_ms > 0),
                    "{class}"
                ),
                _ => assert_eq!(decision, Decision::StayDown, "{class}"),
            }
        }
        // The supervisor's own reading, as the fixture states it.
        let reads = &fixture["supervisorReads"]["windows-task"]["stayDown"];
        let stay: Vec<i32> = reads
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap() as i32)
            .collect();
        assert_eq!(stay, policy.launcher.stay_down);
    }

    #[test]
    fn five_consecutive_failed_starts_trip_and_readiness_resets_the_count() {
        let policy = policy();
        assert_eq!(policy.failed_start_limit, 5);
        let mut state = State::default();
        let delays: Vec<Decision> = (0..4)
            .map(|_| state.on_exit(&policy, Some(1), false))
            .collect();
        assert_eq!(
            delays,
            [2000, 4000, 8000, 16000].map(|delay_ms| Decision::Relaunch { delay_ms })
        );
        assert_eq!(
            state.on_exit(&policy, None, false),
            Decision::Trip,
            "a spawn failure is a failed start"
        );

        let mut state = State::default();
        for _ in 0..4 {
            state.on_exit(&policy, Some(1), false);
        }
        // A crash after readiness is relaunched and starts the count again.
        assert_eq!(
            state.on_exit(&policy, Some(1), true),
            Decision::Relaunch { delay_ms: 2000 }
        );
        assert_eq!(state.failed_starts, 0);
        // A deliberate relaunch never counts, ready or not.
        for _ in 0..10 {
            assert_eq!(
                state.on_exit(&policy, Some(75), false),
                Decision::Relaunch { delay_ms: 0 }
            );
        }
        assert_eq!(state.failed_starts, 0);
    }

    #[test]
    fn stay_down_codes_stop_even_after_failures() {
        let policy = policy();
        let mut state = State::default();
        state.on_exit(&policy, Some(1), false);
        for code in [0, 77, 78] {
            assert_eq!(
                state.on_exit(&policy, Some(code), false),
                Decision::StayDown
            );
        }
    }
}
