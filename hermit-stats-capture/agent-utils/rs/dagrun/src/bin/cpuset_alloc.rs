//! CLI entry point for the `cpuset-alloc` companion command.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(dagrun::cpuset_allocator::run(&args));
}
