//! Tiny third-party-runner probe: fast empty tests plus one opt-in failure or hang.

#[cfg(test)]
mod tests {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    fn mode() -> String {
        std::env::var("DAG_RUNNER_PROBE_MODE").unwrap_or_default()
    }

    #[test]
    fn fast_empty_alpha() {}

    #[test]
    fn fast_empty_beta() {}

    #[test]
    fn planted_failure() {
        assert_ne!(mode(), "fail", "deliberately planted failure");
    }

    #[test]
    fn planted_wall_hang() {
        if mode() == "wall" {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn planted_cpu_burn() {
        if mode() == "cpu" {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(60) {
                black_box(start.elapsed());
            }
        }
    }
}
