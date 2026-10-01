// Copyright (c) Meta Platforms, Inc. and affiliates.
//! Separate official owner profile for the no-provider CLI refusal regression.
pub const FD_ENV: &str = "HERMIT_CLI_REFUSAL_CGROUP_FD";
pub const CLI_ENV: &str = "HERMIT_CLI_REFUSAL_PREPARED_CLI";
pub const CASE_ENV: &str = "HERMIT_CLI_REFUSAL_CASE";
pub const WALL_SECONDS: u64 = 12;

pub fn case(package: &str, binary: &str, test: &str) -> Option<&'static str> {
    if package != "hermit" || binary != "hermit::cli_startup_stderr" {
        return None;
    }
    match test {
        "full_stderr_refusal_exits_naturally" => Some("full"),
        "normal_stderr_refusal_exits_naturally" => Some("normal"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_cli_cases_only() {
        for (name, mode) in [
            ("full_stderr_refusal_exits_naturally", "full"),
            ("normal_stderr_refusal_exits_naturally", "normal"),
        ] {
            assert_eq!(
                case("hermit", "hermit::cli_startup_stderr", name),
                Some(mode)
            );
            assert_eq!(case("foreign", "hermit::cli_startup_stderr", name), None);
            assert_eq!(case("hermit", "hermit::record_replay", name), None);
            assert_eq!(
                case(
                    "hermit",
                    "hermit::cli_startup_stderr",
                    &format!("{name}_extra")
                ),
                None
            );
        }
        assert_eq!(
            case("hermit", "hermit::cli_startup_stderr", "pure::oracle"),
            None
        );
    }
}
