use super::AgentExecutionAdmission;
use crate::agent::AgentControl;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Barrier;

fn control_with_limit(max_threads: usize) -> AgentControl {
    let control = AgentControl::default();
    control.agent_execution_limiter.initialize(max_threads);
    control
}

#[test]
fn execution_guards_count_active_v2_subagent_turns() {
    let control = control_with_limit(/*max_threads*/ 1);
    // Child role configs cannot replace the root-derived session limit.
    control
        .agent_execution_limiter
        .initialize(/*max_threads*/ 2);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));

    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("first active turn should fit");
    let first = control
        .execution_guard(MultiAgentVersion::V2, &source)
        .expect("v2 subagent execution should be counted");
    let Err(err) = control.ensure_execution_capacity(MultiAgentVersion::V2, &source) else {
        panic!("second active turn should exceed the derived non-root cap");
    };
    let CodexErr::AgentLimitReached { max_threads } = err else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(max_threads, 1);

    drop(first);
    control
        .ensure_execution_capacity(MultiAgentVersion::V2, &source)
        .expect("capacity should be released when the running task drops");
}

#[test]
fn execution_guards_ignore_root_and_v1_turns() {
    let control = control_with_limit(/*max_threads*/ 0);

    assert!(
        control
            .execution_guard(MultiAgentVersion::V2, &SessionSource::Cli)
            .is_none()
    );
    assert!(
        control
            .execution_guard(
                MultiAgentVersion::V1,
                &SessionSource::SubAgent(SubAgentSource::Other("worker".to_string())),
            )
            .is_none()
    );
    assert!(matches!(
        control.execution_admission(MultiAgentVersion::V2, &SessionSource::Cli),
        AgentExecutionAdmission::Unrestricted
    ));
    assert!(matches!(
        control.execution_admission(
            MultiAgentVersion::V1,
            &SessionSource::SubAgent(SubAgentSource::Other("worker".to_string())),
        ),
        AgentExecutionAdmission::Unrestricted
    ));
}

#[test]
fn execution_guard_capacity_reservation_is_atomic() {
    let control = Arc::new(control_with_limit(/*max_threads*/ 1));
    let start = Arc::new(Barrier::new(/*n*/ 3));
    let finish = Arc::new(Barrier::new(/*n*/ 3));
    let handles: [_; 2] = std::array::from_fn(|_| {
        let control = Arc::clone(&control);
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        std::thread::spawn(move || {
            let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
            start.wait();
            let guard = control.try_execution_guard(MultiAgentVersion::V2, &source);
            finish.wait();
            guard
        })
    });

    start.wait();
    finish.wait();
    let mut admitted = 0;
    let mut rejected = 0;
    for attempt in handles.map(|handle| handle.join().expect("capacity contender should not panic"))
    {
        match attempt {
            Ok(Some(_guard)) => admitted += 1,
            Ok(None) => panic!("V2 subagent execution should require capacity"),
            Err(CodexErr::AgentLimitReached { max_threads }) => {
                assert_eq!(max_threads, 1);
                rejected += 1;
            }
            Err(error) => panic!("unexpected capacity error: {error}"),
        }
    }
    assert_eq!((admitted, rejected), (1, 1));

    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    assert!(
        control
            .try_execution_guard(MultiAgentVersion::V2, &source)
            .expect("capacity should be released after both contenders exit")
            .is_some()
    );
}

#[tokio::test]
async fn capacity_waiter_observes_release_before_first_poll() {
    let control = control_with_limit(/*max_threads*/ 1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    let guard = match control.execution_admission(MultiAgentVersion::V2, &source) {
        AgentExecutionAdmission::Admitted(guard) => guard,
        AgentExecutionAdmission::Unrestricted | AgentExecutionAdmission::AtCapacity(_) => {
            panic!("first limited turn should be admitted")
        }
    };
    let waiter = match control.execution_admission(MultiAgentVersion::V2, &source) {
        AgentExecutionAdmission::AtCapacity(waiter) => waiter,
        AgentExecutionAdmission::Unrestricted | AgentExecutionAdmission::Admitted(_) => {
            panic!("second limited turn should wait")
        }
    };

    drop(guard);
    tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 1),
        waiter.wait_for_release(),
    )
    .await
    .expect("pre-subscribed waiter should retain a release that happens before polling");
}

#[tokio::test]
async fn capacity_retry_rearms_after_released_capacity_is_stolen() {
    let control = control_with_limit(/*max_threads*/ 1);
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    let first_guard = match control.execution_admission(MultiAgentVersion::V2, &source) {
        AgentExecutionAdmission::Admitted(guard) => guard,
        AgentExecutionAdmission::Unrestricted | AgentExecutionAdmission::AtCapacity(_) => {
            panic!("first limited turn should be admitted")
        }
    };
    let first_waiter = match control.execution_admission(MultiAgentVersion::V2, &source) {
        AgentExecutionAdmission::AtCapacity(waiter) => waiter,
        AgentExecutionAdmission::Unrestricted | AgentExecutionAdmission::Admitted(_) => {
            panic!("retry should receive a capacity waiter")
        }
    };

    drop(first_guard);
    let stealing_guard = match control.execution_admission(MultiAgentVersion::V2, &source) {
        AgentExecutionAdmission::Admitted(guard) => guard,
        AgentExecutionAdmission::Unrestricted | AgentExecutionAdmission::AtCapacity(_) => {
            panic!("another claimant should be able to steal the released capacity")
        }
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 1),
        first_waiter.wait_for_release(),
    )
    .await
    .expect("the first pre-subscribed waiter should still wake");
    let rearmed_waiter = match control.execution_admission(MultiAgentVersion::V2, &source) {
        AgentExecutionAdmission::AtCapacity(waiter) => waiter,
        AgentExecutionAdmission::Unrestricted | AgentExecutionAdmission::Admitted(_) => {
            panic!("retry must rearm after the released capacity is stolen")
        }
    };
    assert!(
        !rearmed_waiter
            .release_rx
            .has_changed()
            .expect("the release channel should remain open")
    );

    drop(stealing_guard);
    tokio::time::timeout(
        std::time::Duration::from_secs(/*secs*/ 1),
        rearmed_waiter.wait_for_release(),
    )
    .await
    .expect("rearmed waiter should observe the thief's release");
}
