use rand::RngExt as _;
use rand::SeedableRng as _;
use rand_pcg::Pcg64Mcg;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::CloneFlags;
use reverie::vdso::VdsoRngSnapshot;

use crate::Config;
use crate::Detcore;
use crate::tool_local::ThreadState;

fn initialized(seed: u64) -> (Detcore, ThreadState<()>) {
    let config = Config {
        rng_seed: Some(seed),
        ..Config::default()
    };
    let tool = <Detcore as Tool>::new(Pid::from_raw(1), &config);
    let state = tool.init_thread_state(Pid::from_raw(1), None);
    (tool, state)
}

fn expected() -> VdsoRngSnapshot {
    VdsoRngSnapshot {
        ready: true,
        generation: 1,
    }
}

#[test]
fn initialized_configured_entropy_is_available_in_generation_one() {
    for seed in [0, 17, u64::MAX] {
        let (tool, state) = initialized(seed);
        assert!(state.prng == Pcg64Mcg::seed_from_u64(seed));
        assert_eq!(tool.vdso_rng_snapshot(&state).unwrap(), expected());
        assert!(state.prng == Pcg64Mcg::seed_from_u64(seed));
    }
}

#[test]
fn repeated_snapshots_do_not_consume_streams_or_charge_events() {
    let (tool, mut state) = initialized(71);
    let mut consumed = [0_u8; 19];
    state.thread_prng().fill(&mut consumed);
    let before = state.clone();
    for _ in 0..128 {
        assert_eq!(tool.vdso_rng_snapshot(&state).unwrap(), expected());
    }
    assert!(state.prng == before.prng);
    assert!(state.chaos_prng == before.chaos_prng);
    assert_eq!(state.committed_clock_value, before.committed_clock_value);
    assert_eq!(
        serde_json::to_string(&state.thread_logical_time).unwrap(),
        serde_json::to_string(&before.thread_logical_time).unwrap()
    );
    assert_eq!(state.end_of_timeslice, before.end_of_timeslice);
    assert_eq!(
        serde_json::to_string(&state.stats).unwrap(),
        serde_json::to_string(&before.stats).unwrap()
    );
    let mut actual = [0_u8; 4097];
    let mut expected_bytes = [0_u8; 4097];
    state.thread_prng().fill(&mut actual);
    before.prng.clone().fill(&mut expected_bytes);
    assert_eq!(actual, expected_bytes);
}

#[test]
fn at_random_shaped_draw_is_consumption_not_a_domain_reset() {
    let (tool, mut state) = initialized(29);
    let mut reference = state.prng.clone();
    let expected_bytes: [u8; 16] = reference.random();
    assert_eq!(tool.vdso_rng_snapshot(&state).unwrap(), expected());
    let bytes: [u8; 16] = state.thread_prng().random();
    assert_eq!(bytes, expected_bytes);
    assert_eq!(tool.vdso_rng_snapshot(&state).unwrap(), expected());
    assert!(state.prng == reference);
}

#[test]
fn actual_child_constructors_and_derivation_preserve_domain_generation() {
    for flags in [
        CloneFlags::empty(),
        CloneFlags::CLONE_VM,
        CloneFlags::CLONE_VM | CloneFlags::CLONE_VFORK,
        CloneFlags::CLONE_VM | CloneFlags::CLONE_THREAD,
    ] {
        let (tool, mut parent) = initialized(43);
        parent.clone_flags = Some(flags);
        let parent_before = parent.prng.clone();
        let mut child = tool.init_thread_state(Pid::from_raw(2), Some((Pid::from_raw(1), &parent)));
        assert!(parent.prng == parent_before);
        assert!(child.prng != parent.prng);
        let child_before = child.prng.clone();
        assert_eq!(tool.vdso_rng_snapshot(&parent).unwrap(), expected());
        assert_eq!(tool.vdso_rng_snapshot(&child).unwrap(), expected());
        assert!(child.prng == child_before);
        child.reseed_child_rngs(&parent, 0x1234_5678);
        let derived = child.prng.clone();
        assert_eq!(tool.vdso_rng_snapshot(&child).unwrap(), expected());
        assert!(child.prng == derived);
        assert!(parent.prng == parent_before);
    }
}
