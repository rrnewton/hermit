/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::env;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
const NUM_ELEMENTS: usize = 20_000_000;

/// Calculate the number of switch points. E.g. the number of times we observed interleaved
/// writes between the threads.
fn count_switch_points(shared_data: &Arc<[u64]>) -> u64 {
    // Calculate the number of switch points. E.g. the number of times we observed interleaved
    // writes between the threads.
    let mut switch_points = 0;
    let mut prev = shared_data[0];
    for i in 1..shared_data.len() {
        if prev != shared_data[i] {
            prev = shared_data[i];
            switch_points += 1;
        }
    }
    switch_points
}

/// In guest mode two threads will try to fill up half of the data array with their thread id as
/// value. The threads grab indices through an atomic int. For sufficiently large arrays we expect
/// the thread ids to show up interleaved.
fn run_test() -> u64 {
    let shared_data: Arc<[u64]> = vec![0; NUM_ELEMENTS].into();
    let shared_idx = Arc::new(AtomicUsize::new(0));

    // `tag` is the distinct per-worker value written into the array. It used to be
    // the thread id, but `ThreadId` has no stable numeric accessor and the test only
    // needs the two workers to write DIFFERENT values -- count_switch_points compares
    // adjacent elements and never inspects the value itself.
    fn worker(idx: Arc<AtomicUsize>, data: Arc<[u64]>, tag: u64) {
        let len = data.len();
        // SAFETY: the two workers only ever write to indices handed out by the shared
        // atomic counter, so no two writes alias. This is the stable spelling of
        // `Arc::get_mut_unchecked`: it rebuilds the same `&mut [u64]`, so indexing
        // keeps its bounds check and the loop keeps the retired-conditional-branch
        // profile the RCB preemption timer measures. Deliberately still a data race --
        // that is what mem_race exists to exercise -- so do NOT "fix" it with atomics.
        let data: &mut [u64] =
            unsafe { std::slice::from_raw_parts_mut(data.as_ptr() as *mut u64, len) };

        // Give each thread half of the fetch_add attempts.
        for _ in 0..(NUM_ELEMENTS / 2) {
            let idx = idx.fetch_add(1, Ordering::SeqCst);
            data[idx] = tag;
        }
    }

    // One-armed variant: spawn one child:
    let handle = {
        let (idx, data) = (shared_idx.clone(), shared_data.clone());
        thread::spawn(move || {
            // This exercises the futex_wait-called-by-kernel behavior, even on "bottom":
            worker(idx, data, 1)
        })
    };
    println!("Parent done spawning child thread and starting own work...");
    worker(shared_idx, shared_data.clone(), 2);

    println!("Parent done with work and joining child thread..");
    handle.join().unwrap();

    let switch_points = count_switch_points(&shared_data);
    let s: String = format!("Switch points: {}\n", switch_points);
    println!("{}", s); // Print a bit more atomically.
    // Only when running deterministically:
    switch_points
}

fn main() {
    if env::var("HERMIT_MODE") == Ok("strict".to_string()) {
        eprintln!("Running sequentialized, deterministically.");
        let switches = run_test();
        assert!(
            switches > 1,
            "Expecting deterministic preemptions when using RCB timers"
        );
    } else {
        eprintln!("Running mem_race, but not expecting determinism.");
        let _switches = run_test();
    }
}
