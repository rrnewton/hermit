#!/usr/bin/env -S rust-script --force
//! Produce a nextest configuration with every per-test wall bound scaled by
//! the machine-specific wall multiplier.
//!
//! ```cargo
//! [dependencies]
//! toml_edit = "0.22.27"
//! ```

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use toml_edit::ArrayOfTables;
use toml_edit::DocumentMut;
use toml_edit::InlineTable;
use toml_edit::Item;
use toml_edit::Table;
use toml_edit::Value;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[allow(dead_code)]
#[path = "manifest-plan/src/timeouts.rs"]
mod timeouts;

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

fn scale_period_value(period: &mut Value, multiplier: f64) -> Result<(), String> {
    let source = period
        .as_str()
        .ok_or_else(|| "slow-timeout.period must be a quoted duration string".to_string())?;
    let base_seconds = parse_period_seconds(source)?;
    let scaled = timeouts::scale_timeout_seconds(base_seconds, multiplier, "wall multiplier")?;
    *period = Value::from(format!("{scaled}s"));
    Ok(())
}

fn scale_inline_timeout(table: &mut InlineTable, multiplier: f64) -> Result<(), String> {
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?;
    scale_period_value(period, multiplier)
}

fn scale_table_timeout(table: &mut Table, multiplier: f64) -> Result<(), String> {
    let period = table
        .get_mut("period")
        .ok_or_else(|| "slow-timeout table is missing period".to_string())?
        .as_value_mut()
        .ok_or_else(|| "slow-timeout.period must be a scalar value".to_string())?;
    scale_period_value(period, multiplier)
}

fn scale_timeout_item(item: &mut Item, multiplier: f64) -> Result<(), String> {
    match item {
        Item::Value(Value::InlineTable(table)) => scale_inline_timeout(table, multiplier),
        Item::Table(table) => scale_table_timeout(table, multiplier),
        _ => Err("slow-timeout must be a TOML table".into()),
    }
}

fn visit_table(table: &mut Table, multiplier: f64, scaled: &mut usize) -> Result<(), String> {
    for (key, item) in table.iter_mut() {
        if key == "slow-timeout" {
            scale_timeout_item(item, multiplier)?;
            *scaled += 1;
        } else {
            visit_item(item, multiplier, scaled)?;
        }
    }
    Ok(())
}

fn visit_array_of_tables(
    tables: &mut ArrayOfTables,
    multiplier: f64,
    scaled: &mut usize,
) -> Result<(), String> {
    for table in tables.iter_mut() {
        visit_table(table, multiplier, scaled)?;
    }
    Ok(())
}

fn visit_item(item: &mut Item, multiplier: f64, scaled: &mut usize) -> Result<(), String> {
    match item {
        Item::Table(table) => visit_table(table, multiplier, scaled),
        Item::ArrayOfTables(tables) => visit_array_of_tables(tables, multiplier, scaled),
        Item::None | Item::Value(_) => Ok(()),
    }
}

fn scaled_config(source: &str, multiplier: f64) -> Result<String, String> {
    timeouts::validate_timeout_multiplier(multiplier, "wall multiplier")?;
    let mut document = source
        .parse::<DocumentMut>()
        .map_err(|error| format!("cannot parse nextest TOML: {error}"))?;
    let mut scaled = 0;
    visit_table(document.as_table_mut(), multiplier, &mut scaled)?;
    if scaled == 0 {
        return Err("nextest config contains no slow-timeout.period values".into());
    }
    Ok(document.to_string())
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
    let rendered = scaled_config(&source_text, multiplier)?;
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
        let rendered = scaled_config(source, 1.25).unwrap();
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
            assert!(scaled_config(&source, 1.0).is_err(), "accepted {period}");
        }
        assert!(
            scaled_config(
                "[profile.default]\nslow-timeout = { terminate-after = 1 }\n",
                1.0
            )
            .unwrap_err()
            .contains("missing period")
        );
        assert!(
            scaled_config("[profile.default]\nfail-fast = false\n", 1.0)
                .unwrap_err()
                .contains("no slow-timeout.period")
        );
    }

    #[test]
    fn refuses_nonpositive_or_nonfinite_multiplier() {
        let source =
            "[profile.default]\nslow-timeout = { period = \"57s\", terminate-after = 1 }\n";
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(scaled_config(source, multiplier).is_err());
        }
    }
}
