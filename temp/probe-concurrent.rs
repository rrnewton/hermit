#!/usr/bin/env -S rust-script --force
//! ```cargo
//! [dependencies]
//! ```
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn(tag: &str, root: &str) -> Child {
    Command::new("cargo")
        .args([
            "nextest", "run", "-p", "hermit-run", "--test", "cli", "-E",
            "test(=cpu_profile_v3_qualifies_the_host_once_and_reuses_the_verdict) | test(=cpu_profile_v3_replay_refuses_a_recording_from_a_different_cpu)",
        ])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {tag}: {e}"))
}

fn main() {
    let root = "/home/newton/work/dev-hermit/worktrees/slots/musetrain-20261101/temp/drain-4709";
    for round in 1..=3 {
        let started = Instant::now();
        let kids: Vec<Child> = (0..2).map(|i| spawn(&format!("r{round}-{i}"), root)).collect();
        for (i, kid) in kids.into_iter().enumerate() {
            let out = kid.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string()
                + &String::from_utf8_lossy(&out.stderr);
            let summary = text
                .lines()
                .filter(|l| l.contains("Summary") || l.contains("FAIL") || l.contains("TIMEOUT"))
                .collect::<Vec<_>>()
                .join(" | ");
            println!(
                "round {round} job {i}: status={} elapsed={:.1}s {}",
                out.status,
                started.elapsed().as_secs_f64(),
                summary
            );
        }
        if started.elapsed() > Duration::from_secs(240) {
            println!("round {round} exceeded 240s budget");
            break;
        }
    }
}
