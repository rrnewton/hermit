#!/usr/bin/env -S rust-script --force
//! Produce a nextest configuration with independent per-test CPU and wall
//! bounds scaled by their machine-specific multipliers.
//!
//! ```cargo
//! [dependencies]
//! toml_edit = "0.22.27"
//! ```

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use toml_edit::Array;
use toml_edit::ArrayOfTables;
use toml_edit::DocumentMut;
use toml_edit::InlineTable;
use toml_edit::Item;
use toml_edit::Table;
use toml_edit::Value;
use toml_edit::value;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[allow(dead_code)]
#[path = "manifest-plan/src/timeouts.rs"]
mod timeouts;

const CPU_WRAPPER_BIN_ENV: &str = "HERMIT_NEXTEST_CPU_WRAPPER_BIN";
const CPU_WRAPPER_NAME: &str = "hermit-per-test-cpu";
const CPU_TERMINATION_GRACE_SECONDS: u64 = 2;

fn usage() -> &'static str {
    "usage: nextest-timeout-config.rs SOURCE MULTIPLIER OUTPUT"
}

fn parse_period_seconds(period: &str) -> Result<u64, String> {
    let digits = period.strip_suffix('s').ok_or_else(|| {
        format!("slow-timeout.period must be a positive whole-second duration, got {period:?}")
    })?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "slow-timeout.period must be a positive whole-second duration, got {period:?}"
        ));
    }
    let seconds = digits
        .parse::<u64>()
        .map_err(|error| format!("invalid slow-timeout.period {period:?}: {error}"))?;
    if seconds == 0 {
        return Err("slow-timeout.period must be greater than zero".into());
    }
    Ok(seconds)
}

fn scale_period_value(period: &mut Value, multiplier: f64) -> Result<u64, String> {
    let source = period
        .as_str()
        .ok_or_else(|| "slow-timeout.period must be a quoted duration string".to_string())?;
    let base_seconds = parse_period_seconds(source)?;
    let scaled = timeouts::scale_timeout_seconds(base_seconds, multiplier, "wall multiplier")?;
    *period = Value::from(format!("{scaled}s"));
    Ok(scaled)
}

fn scale_inline_timeout(table: &mut InlineTable, multiplier: f64) -> Result<Option<u64>, String> {
    let terminate_after = table.get("terminate-after").cloned();
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?;
    let period = scale_period_value(period, multiplier)?;
    termination_bound(period, terminate_after.as_ref())
}

fn scale_table_timeout(table: &mut Table, multiplier: f64) -> Result<Option<u64>, String> {
    let terminate_after = table
        .get("terminate-after")
        .and_then(Item::as_value)
        .cloned();
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?
        .as_value_mut()
        .ok_or_else(|| "slow-timeout.period must be a scalar value".to_string())?;
    let period = scale_period_value(period, multiplier)?;
    termination_bound(period, terminate_after.as_ref())
}

// Nextest inherits the complete slow-timeout value. A present table without
// terminate-after warns indefinitely; it does not inherit that field alone.
fn termination_bound(period: u64, terminate_after: Option<&Value>) -> Result<Option<u64>, String> {
    let Some(value) = terminate_after else {
        return Ok(None);
    };
    let count = value
        .as_integer()
        .filter(|value| *value > 0)
        .ok_or("slow-timeout.terminate-after must be a positive integer")? as u64;
    period
        .checked_mul(count)
        .map(Some)
        .ok_or_else(|| "slow-timeout termination bound overflows seconds".into())
}

fn scale_timeout_item(item: &mut Item, multiplier: f64) -> Result<Option<u64>, String> {
    match item {
        Item::Value(Value::InlineTable(table)) => scale_inline_timeout(table, multiplier),
        Item::Table(table) => scale_table_timeout(table, multiplier),
        _ => Err("slow-timeout must be a TOML table".into()),
    }
}

fn visit_table(
    table: &mut Table,
    multiplier: f64,
    wall_deadlines: &mut Vec<Option<u64>>,
) -> Result<(), String> {
    for (key, item) in table.iter_mut() {
        if key == "slow-timeout" {
            wall_deadlines.push(scale_timeout_item(item, multiplier)?);
        } else {
            visit_item(item, multiplier, wall_deadlines)?;
        }
    }
    Ok(())
}

fn visit_array_of_tables(
    tables: &mut ArrayOfTables,
    multiplier: f64,
    wall_deadlines: &mut Vec<Option<u64>>,
) -> Result<(), String> {
    for table in tables.iter_mut() {
        visit_table(table, multiplier, wall_deadlines)?;
    }
    Ok(())
}

fn visit_item(
    item: &mut Item,
    multiplier: f64,
    wall_deadlines: &mut Vec<Option<u64>>,
) -> Result<(), String> {
    match item {
        Item::Table(table) => visit_table(table, multiplier, wall_deadlines),
        Item::ArrayOfTables(tables) => visit_array_of_tables(tables, multiplier, wall_deadlines),
        Item::None | Item::Value(_) => Ok(()),
    }
}

fn run_wrapper_count(document: &DocumentMut, profile: &str) -> Result<usize, String> {
    let Some(profile_table) = document
        .get("profile")
        .and_then(Item::as_table)
        .and_then(|profiles| profiles.get(profile))
        .and_then(Item::as_table)
    else {
        return Ok(0);
    };
    let Some(scripts) = profile_table.get("scripts") else {
        return Ok(0);
    };
    let scripts = scripts
        .as_array_of_tables()
        .ok_or_else(|| format!("profile.{profile}.scripts must be an array of tables"))?;
    Ok(scripts
        .iter()
        .filter(|script| script.contains_key("run-wrapper"))
        .count())
}

fn add_cpu_wrapper(
    document: &mut DocumentMut,
    wrapper_bin: &Path,
    cpu_budget_usec: u64,
) -> Result<(), String> {
    if !wrapper_bin.is_absolute() {
        return Err("nextest CPU wrapper executable must be absolute".into());
    }
    for profile in ["default", "ci"] {
        let count = run_wrapper_count(document, profile)?;
        if count != 0 {
            return Err(format!(
                "profile.{profile} already defines {count} run-wrapper rule(s); refusing to make per-test CPU measurement partial or ambiguous"
            ));
        }
    }

    let experimental = document
        .entry("experimental")
        .or_insert_with(|| Item::Value(Value::Array(Array::new())))
        .as_array_mut()
        .ok_or_else(|| "experimental must be an array".to_string())?;
    let wrapper_feature_count = experimental
        .iter()
        .filter(|value| value.as_str() == Some("wrapper-scripts"))
        .count();
    if wrapper_feature_count > 1 {
        return Err("experimental contains wrapper-scripts more than once".into());
    }
    if wrapper_feature_count == 0 {
        experimental.push("wrapper-scripts");
    }

    if document
        .get("scripts")
        .and_then(Item::as_table)
        .and_then(|scripts| scripts.get("wrapper"))
        .and_then(Item::as_table)
        .is_some_and(|wrappers| wrappers.contains_key(CPU_WRAPPER_NAME))
    {
        return Err(format!(
            "scripts.wrapper.{CPU_WRAPPER_NAME} is already defined"
        ));
    }
    let wrapper_path = wrapper_bin
        .to_str()
        .ok_or_else(|| "nextest CPU wrapper executable path is not UTF-8".to_string())?;
    let mut command = InlineTable::new();
    // An argv array preserves spaces and shell punctuation in a target path.
    let mut argv = Array::new();
    argv.push(wrapper_path);
    argv.push("--cpu-timeout-usec");
    argv.push(cpu_budget_usec.to_string());
    argv.push("--termination-grace-ms");
    argv.push((CPU_TERMINATION_GRACE_SECONDS * 1000).to_string());
    argv.push("--");
    command.insert("command-line", Value::Array(argv));
    command.insert("relative-to", Value::from("none"));
    let scripts = document
        .entry("scripts")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "scripts must be a table".to_string())?;
    let wrappers = scripts
        .entry("wrapper")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| "scripts.wrapper must be a table".to_string())?;
    let mut wrapper = Table::new();
    wrapper["command"] = value(command);
    wrapper["target-runner"] = value("within-wrapper");
    wrappers.insert(CPU_WRAPPER_NAME, Item::Table(wrapper));

    let scripts = &mut document["profile"]["default"]["scripts"];
    if scripts.is_none() {
        *scripts = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let scripts = scripts
        .as_array_of_tables_mut()
        .ok_or_else(|| "profile.default.scripts must be an array of tables".to_string())?;
    let mut binding = Table::new();
    binding["filter"] = value("all()");
    binding["run-wrapper"] = value(CPU_WRAPPER_NAME);
    scripts.push(binding);

    if run_wrapper_count(document, "default")? != 1 || run_wrapper_count(document, "ci")? != 0 {
        return Err(
            "generated nextest config does not define exactly one inherited run wrapper for default and ci"
                .into(),
        );
    }
    Ok(())
}

fn scaled_config(
    source: &str,
    multiplier: f64,
    cpu_wrapper: Option<(&Path, f64)>,
) -> Result<String, String> {
    timeouts::validate_timeout_multiplier(multiplier, "wall multiplier")?;
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("cannot parse nextest TOML: {error}"))?;
    let mut wall_deadlines = Vec::new();
    visit_table(document.as_table_mut(), multiplier, &mut wall_deadlines)?;
    if wall_deadlines.is_empty() {
        return Err("nextest config contains no slow-timeout.period values".into());
    }
    if let Some((wrapper_bin, cpu_multiplier)) = cpu_wrapper {
        let cpu_seconds = timeouts::scale_timeout_seconds(
            timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
            cpu_multiplier,
            "CPU multiplier",
        )?;
        let cpu_budget_usec = cpu_seconds
            .checked_mul(1_000_000)
            .ok_or("CPU timeout overflows microseconds")?;
        let cpu_and_cleanup = cpu_seconds
            .checked_add(CPU_TERMINATION_GRACE_SECONDS)
            .ok_or("CPU timeout plus cleanup grace overflows seconds")?;
        // Check complete profile/override values, including inherited defaults,
        // without lengthening a wall timeout or treating a warning as a stop.
        if wall_deadlines
            .iter()
            .flatten()
            .any(|wall| *wall <= cpu_and_cleanup)
        {
            return Err(format!(
                "nextest CPU timeout ({cpu_seconds}s) plus cleanup grace \
                 ({CPU_TERMINATION_GRACE_SECONDS}s) must be below every finite scaled wall termination bound"
            ));
        }
        add_cpu_wrapper(&mut document, wrapper_bin, cpu_budget_usec)?;
    }
    Ok(document.to_string())
}

fn cpu_wrapper_from_env() -> Option<std::path::PathBuf> {
    env::var_os(CPU_WRAPPER_BIN_ENV).map(Into::into)
}

fn run() -> Result<(), String> {
    let mut args = env::args_os();
    let _program = args.next();
    let source = args.next().ok_or_else(|| usage().to_string())?;
    let multiplier = args
        .next()
        .ok_or_else(|| usage().to_string())?
        .into_string()
        .map_err(|_| "MULTIPLIER must be UTF-8".to_string())?
        .parse::<f64>()
        .map_err(|error| format!("invalid wall multiplier: {error}"))?;
    let output = args.next().ok_or_else(|| usage().to_string())?;
    if args.next().is_some() {
        return Err(usage().into());
    }
    let source_text = fs::read_to_string(Path::new(&source))
        .map_err(|error| format!("cannot read {}: {error}", Path::new(&source).display()))?;
    let cpu_wrapper = cpu_wrapper_from_env();
    let cpu_multiplier = if cpu_wrapper.is_some() {
        timeouts::timeout_multiplier_from_env(timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV)?
    } else {
        1.0
    };
    let rendered = scaled_config(
        &source_text,
        multiplier,
        cpu_wrapper.as_deref().map(|path| (path, cpu_multiplier)),
    )?;
    fs::write(Path::new(&output), rendered)
        .map_err(|error| format!("cannot write {}: {error}", Path::new(&output).display()))
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nextest-timeout-config: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_every_profile_and_override_with_ceil_rounding() {
        let source = concat!(
            "# retained comment\n",
            "[profile.default]\n",
            "slow-timeout = { period = \"57s\", terminate-after = 1, grace-period = \"2s\" }\n",
            "unrelated = \"kept\"\n",
            "[profile.ci]\n",
            "slow-timeout = { period = \"10s\", terminate-after = 2 }\n",
            "[[profile.ci.overrides]]\n",
            "filter = \"test(example)\"\n",
            "slow-timeout = { period = \"3s\", terminate-after = 1 }\n",
        );
        let rendered = scaled_config(source, 1.25, None).unwrap();
        assert!(rendered.contains("# retained comment"));
        assert!(rendered.contains("unrelated = \"kept\""));
        assert!(rendered.contains("filter = \"test(example)\""));
        assert!(rendered.contains("period = \"72s\""));
        assert!(rendered.contains("period = \"13s\""));
        assert!(rendered.contains("period = \"4s\""));
        assert_eq!(rendered.matches("slow-timeout").count(), 3);
    }

    #[test]
    fn refuses_malformed_or_missing_periods() {
        for period in ["0s", "1.5s", "5m", "-1s", "s"] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = {period:?}, terminate-after = 1 }}\n"
            );
            assert!(
                scaled_config(&source, 1.0, None).is_err(),
                "accepted {period}"
            );
        }
        assert!(
            scaled_config(
                "[profile.default]\nslow-timeout = { terminate-after = 1 }\n",
                1.0,
                None,
            )
            .unwrap_err()
            .contains("missing period")
        );
        assert!(
            scaled_config("[profile.default]\nfail-fast = false\n", 1.0, None)
                .unwrap_err()
                .contains("no slow-timeout.period")
        );
    }

    #[test]
    fn refuses_nonpositive_or_nonfinite_multiplier() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(scaled_config(source, multiplier, None).is_err());
        }
    }

    #[test]
    fn adds_one_default_wrapper_inherited_by_ci_without_changing_wall_scaling() {
        let source = concat!(
            "nextest-version = \"0.9.100\"\n",
            "[profile.default]\n",
            "slow-timeout = { period = \"57s\", terminate-after = 1, grace-period = \"2s\" }\n",
            "[profile.ci]\n",
            "status-level = \"slow\"\n",
        );
        let rendered = scaled_config(source, 1.5, Some((Path::new("/tmp/wrapper"), 1.0))).unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        assert_eq!(run_wrapper_count(&document, "default").unwrap(), 1);
        assert_eq!(run_wrapper_count(&document, "ci").unwrap(), 0);
        assert_eq!(rendered.matches("run-wrapper").count(), 1);
        assert!(rendered.contains("period = \"86s\""));
        assert!(rendered.contains("wrapper-scripts"));
        assert!(rendered.contains("target-runner = \"within-wrapper\""));
    }

    #[test]
    fn refuses_a_second_default_or_ci_run_wrapper() {
        for profile in ["default", "ci"] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = \"57s\" }}\n[[profile.{profile}.scripts]]\nfilter = \"all()\"\nrun-wrapper = \"other\"\n"
            );
            let error =
                scaled_config(&source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0))).unwrap_err();
            assert!(error.contains(&format!("profile.{profile} already defines")));
        }
    }

    #[test]
    fn wrapper_path_is_one_literal_argument() {
        let path = "/tmp/a path/it's-$(literal)-wrapper";
        let rendered = scaled_config(
            "[profile.default]\nslow-timeout = { period = \"57s\" }\n",
            1.0,
            Some((Path::new(path), 1.0)),
        )
        .unwrap();
        let document = rendered.parse::<DocumentMut>().unwrap();
        let argv = document["scripts"]["wrapper"][CPU_WRAPPER_NAME]["command"]["command-line"]
            .as_array()
            .unwrap();
        assert_eq!(argv.len(), 6);
        assert_eq!(argv.get(0).unwrap().as_str(), Some(path));
    }

    #[test]
    fn cpu_budget_is_scaled_independently_and_rounded_up() {
        let source = "[profile.default]\nslow-timeout = { period = \"57s\" }\n";
        for (cpu_multiplier, expected) in [(1.0, "22000000"), (1.25, "28000000"), (0.01, "1000000")]
        {
            let rendered = scaled_config(
                source,
                1.5,
                Some((Path::new("/tmp/wrapper"), cpu_multiplier)),
            )
            .unwrap();
            let document = rendered.parse::<DocumentMut>().unwrap();
            let argv = document["scripts"]["wrapper"][CPU_WRAPPER_NAME]["command"]["command-line"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                argv,
                [
                    "/tmp/wrapper",
                    "--cpu-timeout-usec",
                    expected,
                    "--termination-grace-ms",
                    "2000",
                    "--"
                ]
            );
            assert!(rendered.contains("period = \"86s\""));
        }
    }

    #[test]
    fn cpu_budget_refuses_invalid_overflow_and_wall_collision() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX, 1e12, 2.5] {
            assert!(
                scaled_config(source, 1.0, Some((Path::new("/tmp/wrapper"), multiplier))).is_err(),
                "accepted {multiplier}"
            );
        }
        let source = format!(
            "{source}[[profile.ci.overrides]]\nfilter = \"test(short)\"\nslow-timeout = {{ period = \"12s\", terminate-after = 2 }}\n"
        );
        assert!(
            scaled_config(&source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0)))
                .unwrap_err()
                .contains("cleanup grace")
        );
        for raw in ["", "wat", "NaN", "inf", "0", "-1"] {
            assert!(timeouts::parse_timeout_multiplier(Some(raw), "CPU multiplier").is_err());
        }
    }

    #[test]
    fn respects_warning_counts_and_whole_timeout_inheritance() {
        let source = concat!(
            "[profile.default]\nslow-timeout = { period = \"12s\", terminate-after = 3 }\n",
            "[profile.ci]\nstatus-level = \"slow\"\n",
            "[[profile.ci.overrides]]\nfilter = \"test(short)\"\n",
            "slow-timeout = { period = \"1s\" }\n",
        );
        let rendered = scaled_config(source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0))).unwrap();
        assert!(rendered.contains("period = \"12s\", terminate-after = 3"));
        assert!(rendered.contains("period = \"1s\""));
        for invalid in ["0", "-1", "1.5", "\"2\""] {
            let source = format!(
                "[profile.default]\nslow-timeout = {{ period = \"57s\", terminate-after = {invalid} }}\n"
            );
            assert!(scaled_config(&source, 1.0, Some((Path::new("/tmp/wrapper"), 1.0))).is_err());
        }
    }
}
