//! CLI entry point for dagrun.

use std::process::ExitCode;

use dagrun::{cli, cpuset_allocator};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = if matches!(
        args.first().map(String::as_str),
        Some(dagrun::profile_capture::PROFILE_PIDFD_SIGNAL_COMMAND)
    ) {
        dagrun::profile_capture::run_pidfd_signal_helper(&args[1..])
    } else if matches!(
        args.first().map(String::as_str),
        Some(dagrun::profile_capture::PROFILE_GROUP_SIGNAL_COMMAND)
    ) {
        dagrun::profile_capture::run_group_signal_helper(&args[1..])
    } else if matches!(args.first().map(String::as_str), Some("__hold" | "__probe")) {
        cpuset_allocator::run(&args)
    } else {
        cli::run(&args)
    };
    ExitCode::from(code as u8)
}
