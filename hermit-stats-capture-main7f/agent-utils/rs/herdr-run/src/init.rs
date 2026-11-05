//! Writing a project's `.herdr-run.yaml` from the annotated template, and upgrading one.
//!
//! The template carries EVERY knob, each set to the value the tool would use anyway, so a project
//! can adopt the file and change nothing. That is deliberate: the reference for what is
//! configurable is the file the tool writes into the project, not a block of documentation that
//! restates it and then drifts from it.
//!
//! The cost of writing every knob out is that the file keeps the defaults of the release that
//! wrote it. A configured deny list replaces the built-in one, so an entry added to the built-in
//! defaults later does not reach a file written earlier. [`upgrade_config_file`] is the way
//! forward for such a file: it rewrites each setting the file still holds exactly as an earlier
//! release wrote it, and leaves every other byte alone.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::{
    missing_defaults, parse_config_text, read_config_file, Config, MissingDefault, CONFIG_FILENAMES,
};
use crate::error::{HerdrRunError, Result};
use crate::textfmt::{display, os_error_text, quote};

/// The complete annotated `.herdr-run.yaml` written by `herdr-run init`.
pub const CONFIG_TEMPLATE: &str = include_str!("config_template.yaml");

/// Write the annotated configuration template into `directory` and return the path written.
///
/// Refuses to overwrite an existing configuration unless `force`. It also refuses when only the
/// `.yml` spelling exists, because the `.yaml` this would write silently takes precedence over it:
/// a caller who ends up with two files and no warning has been given a puzzle, not a config.
///
/// A directory whose name is not valid UTF-8 is refused, as every other subcommand refuses it: the
/// file written there could never be loaded.
pub fn write_config_template(directory: &Path, force: bool) -> Result<PathBuf> {
    if directory.to_str().is_none() {
        return Err(HerdrRunError::config(format!(
            "current directory is not valid UTF-8: {}",
            quote(&directory.to_string_lossy())
        )));
    }
    let destination = directory.join(CONFIG_FILENAMES[0]);
    if !force {
        for filename in CONFIG_FILENAMES {
            let existing = directory.join(filename);
            if !existing.exists() {
                continue;
            }
            let precedence = if filename == CONFIG_FILENAMES[0] {
                String::new()
            } else {
                format!(
                    ", and the {} this would write takes precedence over it",
                    CONFIG_FILENAMES[0]
                )
            };
            return Err(HerdrRunError::config(format!(
                "{} already exists{precedence}; pass --force to overwrite",
                display(&existing.to_string_lossy())
            )));
        }
    }
    let mut options = OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(&destination).map_err(|error| {
        HerdrRunError::config(format!(
            "cannot write {}: {error}",
            display(&destination.to_string_lossy())
        ))
    })?;
    file.write_all(CONFIG_TEMPLATE.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| {
            HerdrRunError::config(format!(
                "cannot write {}: {error}",
                display(&destination.to_string_lossy())
            ))
        })?;
    Ok(destination)
}

/// One setting as an earlier release wrote it, and the same setting as this release writes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Upgrade {
    /// The setting, as a dotted path (`deny_anywhere`, `value_options.git`).
    pub setting: &'static str,
    /// Whole lines, exactly as an earlier release wrote them.
    pub earlier: &'static str,
    /// The lines this release writes in their place.
    pub current: &'static str,
    /// The entries `current` has that `earlier` does not, in the order it lists them.
    pub added: &'static [&'static str],
}

/// Every setting an earlier release wrote differently from this one, in the order they are
/// applied.
///
/// * `deny_anywhere` without `--exec`: written by `herdr-run init` in 0.2.0 and 0.3.0, and printed
///   in the example configuration of the 0.1.0 and 0.2.0 user guides.
/// * `value_options` with seven git entries: written by `herdr-run init` in 0.2.0 and 0.3.0. The
///   built-in table applies whatever a file says, so this one changes how the file reads, not what
///   it does.
///
/// The other edition holds the same table, and the shared corpus pins what each earlier release's
/// file becomes.
pub const UPGRADES: [Upgrade; 2] = [
    Upgrade {
        setting: "deny_anywhere",
        earlier: "deny_anywhere: [\"--upload-pack\", \"--receive-pack\"]\n",
        current: "deny_anywhere: [\"--upload-pack\", \"--receive-pack\", \"--exec\"]\n",
        added: &["--exec"],
    },
    Upgrade {
        setting: "value_options.git",
        earlier: concat!(
            "value_options:\n",
            "  git: [\"-C\", \"-c\", \"--git-dir\", \"--work-tree\", \"--namespace\", \"--exec-path\", ",
            "\"--config-env\"]\n",
        ),
        current: concat!(
            "value_options:\n",
            "  git: [\"-C\", \"-c\", \"--git-dir\", \"--work-tree\", \"--namespace\", \"--exec-path\", ",
            "\"--config-env\",\n",
            "        \"--super-prefix\", \"--shallow-file\", \"--attr-source\"]\n",
        ),
        added: &["--super-prefix", "--shallow-file", "--attr-source"],
    },
];

/// What [`upgrade_config_file`] did to one file, and what it left for a human.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeResult {
    /// The file upgraded.
    pub path: PathBuf,
    /// Each setting rewritten, in [`UPGRADES`] order.
    pub upgraded: Vec<Upgrade>,
    /// The built-in deny entries the file still leaves out after the upgrade.
    pub missing: Vec<MissingDefault>,
}

/// Rewrite every setting `text` still holds exactly as an earlier release wrote it.
///
/// An [`Upgrade`] applies where its earlier lines appear exactly once, starting at the start of a
/// line; anything else -- a list somebody reformatted or edited, a file with CRLF line ends -- is
/// left exactly as it is. A last line without a line feed is matched as if it had one, and still
/// has none afterwards.
#[must_use]
pub fn upgrade_text(text: &str) -> (String, Vec<Upgrade>) {
    let unterminated = !text.ends_with('\n');
    let mut working = format!("\n{text}{}", if unterminated { "\n" } else { "" });
    let mut applied = Vec::new();
    for upgrade in UPGRADES {
        let needle = format!("\n{}", upgrade.earlier);
        if working.matches(needle.as_str()).count() != 1 {
            continue;
        }
        working = working.replacen(needle.as_str(), &format!("\n{}", upgrade.current), 1);
        applied.push(upgrade);
    }
    let mut upgraded = working[1..].to_owned();
    if unterminated {
        upgraded.pop();
    }
    (upgraded, applied)
}

/// Whether every entry of `old` is in `new`.
fn covers_list(new: &[String], old: &[String]) -> bool {
    old.iter().all(|entry| new.contains(entry))
}

fn covers_map(new: &BTreeMap<String, Vec<String>>, old: &BTreeMap<String, Vec<String>>) -> bool {
    old.iter().all(|(key, entries)| {
        new.get(key)
            .is_some_and(|found| covers_list(found, entries))
    })
}

/// Put the setting `name` of `after` back to its value in `before`, and say whether `after` held
/// every entry `before` did. `false` for a setting no [`Upgrade`] may touch.
fn restore_setting(after: &mut Config, before: &Config, name: &str) -> bool {
    let covered = match name {
        "deny_global" => covers_map(&after.deny_global, &before.deny_global),
        "deny_subcommand" => covers_map(&after.deny_subcommand, &before.deny_subcommand),
        "deny_anywhere" => covers_list(&after.deny_anywhere, &before.deny_anywhere),
        "deny_subcommand_option" => {
            before
                .deny_subcommand_option
                .iter()
                .all(|(program, entries)| {
                    after
                        .deny_subcommand_option
                        .get(program)
                        .is_some_and(|found| covers_map(found, entries))
                })
        }
        "value_options" => covers_map(&after.value_options, &before.value_options),
        _ => return false,
    };
    match name {
        "deny_global" => after.deny_global = before.deny_global.clone(),
        "deny_subcommand" => after.deny_subcommand = before.deny_subcommand.clone(),
        "deny_anywhere" => after.deny_anywhere = before.deny_anywhere.clone(),
        "deny_subcommand_option" => {
            after.deny_subcommand_option = before.deny_subcommand_option.clone();
        }
        _ => after.value_options = before.value_options.clone(),
    }
    covered
}

/// The configuration file in `directory` that `init --upgrade` works on.
fn upgrade_target(directory: &Path) -> Result<PathBuf> {
    for filename in CONFIG_FILENAMES {
        let candidate = directory.join(filename);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(HerdrRunError::config(format!(
        "there is no {} or {} in {} to upgrade; run 'herdr-run init' to write one",
        CONFIG_FILENAMES[0],
        CONFIG_FILENAMES[1],
        display(&directory.to_string_lossy())
    )))
}

/// Upgrade the configuration file in `directory` in place, and say what it did.
///
/// The file is the `.herdr-run.yaml` there, else the `.herdr-run.yml`: the one `init` would refuse
/// to overwrite. It must load. [`upgrade_text`] rewrites what it can; the result is loaded again
/// and must read exactly as the file did except for the settings rewritten, which may only have
/// gained entries -- otherwise nothing is written. The file is written in place, so its mode, its
/// owner, and a symbolic link to it are kept, and it is not written at all when nothing changed.
pub fn upgrade_config_file(directory: &Path) -> Result<UpgradeResult> {
    let Some(directory_text) = directory.to_str() else {
        return Err(HerdrRunError::config(format!(
            "current directory is not valid UTF-8: {}",
            quote(&directory.to_string_lossy())
        )));
    };
    let path = upgrade_target(directory)?;
    let path_text = path.to_string_lossy().into_owned();
    let text = read_config_file(&path)?;
    let before = parse_config_text(&text, path_text.clone(), directory_text.to_owned())?;
    let (upgraded, applied) = upgrade_text(&text);
    if applied.is_empty() {
        return Ok(UpgradeResult {
            path,
            upgraded: applied,
            missing: missing_defaults(&before),
        });
    }
    let after = parse_config_text(&upgraded, path_text.clone(), directory_text.to_owned())?;
    let mut restored = after.clone();
    let mut gained_only = true;
    for upgrade in &applied {
        let name = upgrade
            .setting
            .split_once('.')
            .map_or(upgrade.setting, |(head, _)| head);
        gained_only &= restore_setting(&mut restored, &before, name);
    }
    if restored != before || !gained_only {
        return Err(HerdrRunError::config(format!(
            "{}: cannot upgrade it automatically, because the rewritten file would not read as this one does with entries added; nothing was written. Add the missing built-in entries by hand ('herdr-run status' lists them)",
            display(&path_text)
        )));
    }
    let shown = display(&path_text);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|error| {
            HerdrRunError::config(format!("cannot write {shown}: {}", os_error_text(&error)))
        })?;
    file.write_all(upgraded.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| {
            HerdrRunError::config(format!("cannot write {shown}: {}", os_error_text(&error)))
        })?;
    Ok(UpgradeResult {
        path,
        upgraded: applied,
        missing: missing_defaults(&after),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_config, Config, KNOWN_KEYS};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "herdr-run-init-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
        path
    }

    /// The template must mention every key the parser knows about.
    ///
    /// This is the promise that makes the guide able to point AT the generated file instead of
    /// restating it: a new configuration key that nobody adds to the template turns the file into
    /// a partial answer, and a partial answer is worse than none because it looks complete.
    #[test]
    fn the_template_carries_every_configuration_key() {
        for key in KNOWN_KEYS {
            assert!(
                CONFIG_TEMPLATE.contains(&format!("\n{key}:")),
                "the generated .herdr-run.yaml never sets {key}"
            );
        }
    }

    /// Writing the template and reading it back must reproduce the built-in defaults exactly.
    #[test]
    fn the_written_template_parses_to_the_built_in_defaults() {
        let root = temporary_directory("defaults");
        let path = write_config_template(&root, false).expect("write template");
        assert_eq!(path, root.join(".herdr-run.yaml"));
        let written = load_config(Some(&path), &root).expect("template must parse");
        let expected = Config {
            source_path: written.source_path.clone(),
            project_root: written.project_root.clone(),
            ..Config::default()
        };
        assert_eq!(written, expected);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_existing_configuration_is_not_clobbered_without_force() {
        for filename in CONFIG_FILENAMES {
            let root = temporary_directory("clobber");
            let existing = root.join(filename);
            fs::write(&existing, "workspace: mine\n").unwrap();
            let error = write_config_template(&root, false).expect_err(filename);
            assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG);
            assert!(error.message().contains("pass --force"), "{error}");
            assert_eq!(
                fs::read_to_string(&existing).unwrap(),
                "workspace: mine\n",
                "{filename} was modified by a refused init"
            );
            if filename != ".herdr-run.yaml" {
                assert!(
                    error.message().contains("takes precedence over it"),
                    "an existing {filename} must say the new file would shadow it: {error}"
                );
                assert!(
                    !root.join(".herdr-run.yaml").exists(),
                    "a refused init must not leave a shadowing file behind"
                );
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn force_overwrites_an_existing_configuration() {
        let root = temporary_directory("force");
        let existing = root.join(".herdr-run.yaml");
        fs::write(&existing, "workspace: mine\n").unwrap();
        let path = write_config_template(&root, true).expect("forced write");
        assert_eq!(fs::read_to_string(&path).unwrap(), CONFIG_TEMPLATE);
        fs::remove_dir_all(root).unwrap();
    }

    fn policy_corpus() -> serde_json::Value {
        serde_json::from_str(include_str!("../testdata/policy_cases.json")).expect("corpus")
    }

    /// Both editions rewrite exactly the settings the corpus lists, from and to the same text.
    #[test]
    fn the_upgrade_table_is_the_shared_one() {
        let table: Vec<serde_json::Value> = UPGRADES
            .iter()
            .map(|upgrade| {
                serde_json::json!({
                    "setting": upgrade.setting,
                    "earlier": upgrade.earlier,
                    "current": upgrade.current,
                    "added": upgrade.added,
                })
            })
            .collect();
        assert_eq!(serde_json::Value::Array(table), policy_corpus()["upgrades"]);
    }

    /// A configuration an earlier release wrote: what it admits, the warning, and the upgrade.
    #[test]
    fn golden_earlier_configs() {
        use crate::allowlist::tests::golden_admission;
        use crate::config::{missing_defaults_warning, parse_config_text};

        let corpus = policy_corpus();
        let location = &corpus["earlier_location"];
        let load = |text: &str| {
            parse_config_text(
                text,
                location["source_path"].as_str().unwrap().to_owned(),
                location["project_root"].as_str().unwrap().to_owned(),
            )
            .expect("an earlier configuration must still load")
        };
        let table: BTreeMap<&str, &serde_json::Value> = corpus["upgrades"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| (entry["setting"].as_str().unwrap(), entry))
            .collect();
        for row in corpus["earlier_configs"]
            .as_array()
            .expect("earlier_configs")
        {
            let name = row["name"].as_str().unwrap();
            let text = match row["file"].as_str() {
                None => CONFIG_TEMPLATE.to_owned(),
                Some(file) => fs::read_to_string(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("testdata")
                        .join(file),
                )
                .expect("earlier configuration"),
            };
            let config = load(&text);
            assert_eq!(
                serde_json::json!(missing_defaults_warning(&config)),
                row["warning"],
                "{name}"
            );
            assert_eq!(
                serde_json::json!(config.value_options),
                row["value_options"],
                "{name}"
            );
            for case in row["commands"].as_array().unwrap() {
                let command = case["command"].as_str().unwrap();
                assert_eq!(
                    golden_admission(&config, None, command),
                    case["expect"],
                    "{name}: {command}"
                );
            }

            let (upgraded, applied) = upgrade_text(&text);
            let settings: Vec<&str> = applied.iter().map(|upgrade| upgrade.setting).collect();
            assert_eq!(serde_json::json!(settings), row["upgraded"], "{name}");
            let mut expected = text.clone();
            for setting in row["upgraded"].as_array().unwrap() {
                let entry = table[setting.as_str().unwrap()];
                let earlier = entry["earlier"].as_str().unwrap();
                assert_eq!(expected.matches(earlier).count(), 1, "{name}: {setting}");
                expected = expected.replacen(earlier, entry["current"].as_str().unwrap(), 1);
            }
            assert_eq!(upgraded, expected, "{name}");

            let after = load(&upgraded);
            assert_eq!(
                serde_json::json!(missing_defaults_warning(&after)),
                row["warning_after_upgrade"],
                "{name}"
            );
            for case in row["commands_after_upgrade"].as_array().unwrap() {
                let command = case["command"].as_str().unwrap();
                assert_eq!(
                    golden_admission(&after, None, command),
                    case["expect"],
                    "{name}: {command}"
                );
            }
        }
    }

    fn earlier_configs() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/earlier_configs")
    }

    /// Each rewrite must say exactly what it adds, and take nothing away.
    #[test]
    fn every_upgrade_only_adds_the_entries_it_names() {
        for upgrade in UPGRADES {
            let (top, program) = upgrade
                .setting
                .split_once('.')
                .map_or((upgrade.setting, None), |(top, program)| {
                    (top, Some(program))
                });
            let entries = |text: &str| -> Vec<String> {
                let document = crate::yaml::core_load(text).expect("valid YAML");
                let crate::yaml::Yaml::Map(pairs) = document else {
                    panic!("{text}")
                };
                let mut value = &pairs.iter().find(|(key, _)| key == top).unwrap().1;
                if let Some(program) = program {
                    let crate::yaml::Yaml::Map(inner) = value else {
                        panic!("{text}")
                    };
                    value = &inner.iter().find(|(key, _)| key == program).unwrap().1;
                }
                let crate::yaml::Yaml::Seq(items) = value else {
                    panic!("{text}")
                };
                items
                    .iter()
                    .map(|item| match item {
                        crate::yaml::Yaml::Str(text) => text.clone(),
                        other => panic!("{other:?}"),
                    })
                    .collect()
            };
            let earlier = entries(upgrade.earlier);
            let current = entries(upgrade.current);
            assert_eq!(current[..earlier.len()], earlier[..], "{}", upgrade.setting);
            assert_eq!(
                current[earlier.len()..],
                upgrade
                    .added
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>()[..],
                "{}",
                upgrade.setting
            );
            // What the rewrite produces is what this release's init writes for the same setting.
            assert!(
                CONFIG_TEMPLATE.contains(upgrade.current),
                "{}",
                upgrade.setting
            );
        }
    }

    #[test]
    fn upgrade_brings_every_earlier_file_up_to_date_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let mut names: Vec<_> = fs::read_dir(earlier_configs())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert!(!names.is_empty());
        for name in names {
            let original = fs::read_to_string(earlier_configs().join(&name)).unwrap();
            let root = temporary_directory("upgrade");
            let target = root.join(".herdr-run.yaml");
            fs::write(&target, &original).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();

            let before = load_config(None, &root).unwrap();
            assert!(crate::config::missing_defaults_warning(&before)
                .unwrap()
                .contains("deny_anywhere \"--exec\""));
            let result = upgrade_config_file(&root).expect("upgrade");
            assert_eq!(result.path, target, "{name:?}");
            assert_eq!(result.upgraded[0].setting, "deny_anywhere", "{name:?}");
            assert!(result.missing.is_empty(), "{name:?}");
            let upgraded = fs::read_to_string(&target).unwrap();
            assert_eq!(upgraded, upgrade_text(&original).0, "{name:?}");
            // Written in place: the mode is the file's own, not a fresh file's.
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
                0o640
            );
            let after = load_config(None, &root).unwrap();
            assert_eq!(crate::config::missing_defaults_warning(&after), None);
            assert!(crate::allowlist::admit("git push --exec=/tmp/evil origin", &after).is_err());

            // A second upgrade finds nothing to do and writes nothing.
            let again = upgrade_config_file(&root).unwrap();
            assert!(again.upgraded.is_empty() && again.missing.is_empty());
            assert_eq!(fs::read_to_string(&target).unwrap(), upgraded);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn upgrade_leaves_an_edited_list_alone_and_says_what_it_lacks() {
        let original =
            fs::read_to_string(earlier_configs().join("0.3.0-init-93b44b35.yaml")).unwrap();
        let edited = original
            .replace(
                "deny_anywhere: [\"--upload-pack\", \"--receive-pack\"]",
                "deny_anywhere: [\"--receive-pack\", \"--upload-pack\"]",
            )
            .replace(
                "git: [\"-c\", \"--config-env\", \"--exec-path\", \"--namespace\"]",
                "git: [\"-c\"]",
            );
        let root = temporary_directory("upgrade-edited");
        fs::write(root.join(".herdr-run.yaml"), &edited).unwrap();
        let result = upgrade_config_file(&root).unwrap();
        let settings: Vec<&str> = result
            .upgraded
            .iter()
            .map(|upgrade| upgrade.setting)
            .collect();
        assert_eq!(settings, ["value_options.git"]);
        let left: Vec<(&str, Vec<&str>)> = result
            .missing
            .iter()
            .map(|item| {
                (
                    item.setting.as_str(),
                    item.entries.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            left,
            [
                (
                    "deny_global.git",
                    vec!["--config-env", "--exec-path", "--namespace"]
                ),
                ("deny_anywhere", vec!["--exec"]),
            ]
        );
        let text = fs::read_to_string(root.join(".herdr-run.yaml")).unwrap();
        assert!(text.contains("deny_anywhere: [\"--receive-pack\", \"--upload-pack\"]\n"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrade_text_matches_whole_lines_only() {
        let line = UPGRADES[0].earlier;
        assert!(upgrade_text(&format!("# {line}")).1.is_empty());
        assert!(upgrade_text(&format!("x{line}")).1.is_empty());
        assert!(upgrade_text(&line.replace('\n', "\r\n")).1.is_empty());
        // A last line without its line feed is still that setting, and still has no line feed after.
        let (upgraded, applied) = upgrade_text(&format!("workspace: w\n{}", line.trim_end()));
        assert_eq!(applied, [UPGRADES[0]]);
        assert_eq!(
            upgraded,
            format!("workspace: w\n{}", UPGRADES[0].current.trim_end())
        );
    }

    #[test]
    fn upgrade_works_on_a_yml_and_refuses_when_there_is_no_file() {
        let root = temporary_directory("upgrade-none");
        let error = upgrade_config_file(&root).expect_err("no file");
        assert_eq!(error.exit_code(), crate::error::EXIT_CONFIG);
        assert_eq!(
            error.message(),
            format!(
                "there is no .herdr-run.yaml or .herdr-run.yml in {} to upgrade; run 'herdr-run init' to write one",
                root.display()
            )
        );
        assert!(!root.join(".herdr-run.yaml").exists());
        let yml = root.join(".herdr-run.yml");
        fs::write(&yml, UPGRADES[0].earlier).unwrap();
        assert_eq!(upgrade_config_file(&root).unwrap().path, yml);
        assert_eq!(fs::read_to_string(&yml).unwrap(), UPGRADES[0].current);
        assert!(!root.join(".herdr-run.yaml").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrade_refuses_a_file_that_does_not_load() {
        let root = temporary_directory("upgrade-broken");
        let path = root.join(".herdr-run.yaml");
        let text = format!("{}allowlist: [git]\n", UPGRADES[0].earlier);
        fs::write(&path, &text).unwrap();
        let error = upgrade_config_file(&root).expect_err("does not load");
        assert!(error.message().contains("unknown key"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_dir_all(root).unwrap();
    }

    /// The earlier lines can appear inside another setting; rewriting them there is refused.
    #[test]
    fn upgrade_writes_nothing_when_the_rewrite_would_read_differently() {
        let text = format!("deny_global: {{git: [-c],\n{}}}\n", UPGRADES[0].earlier);
        let document = crate::yaml::core_load(&text).unwrap();
        let loaded = crate::config::parse_config(&document, None, "/".to_owned()).unwrap();
        assert!(loaded.deny_global.contains_key("deny_anywhere"));
        assert_eq!(upgrade_text(&text).1, [UPGRADES[0]]);
        let root = temporary_directory("upgrade-refused");
        let path = root.join(".herdr-run.yaml");
        fs::write(&path, &text).unwrap();
        let error = upgrade_config_file(&root).expect_err("must be refused");
        assert!(
            error.message().contains("cannot upgrade it automatically")
                && error.message().contains("nothing was written"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn upgrade_keeps_a_symbolic_link() {
        let root = temporary_directory("upgrade-link");
        let real = root.join("policy.yaml");
        fs::write(&real, UPGRADES[0].earlier).unwrap();
        std::os::unix::fs::symlink("policy.yaml", root.join(".herdr-run.yaml")).unwrap();
        upgrade_config_file(&root).unwrap();
        assert!(fs::symlink_metadata(root.join(".herdr-run.yaml"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), UPGRADES[0].current);
        fs::remove_dir_all(root).unwrap();
    }

    /// The two things the owner asked the template to say out loud.
    #[test]
    fn the_template_names_the_allow_everything_mode_and_the_human_only_rule() {
        assert!(CONFIG_TEMPLATE.contains(r#"allow: ["*"]"#));
        assert!(CONFIG_TEMPLATE.contains("ALLOW-EVERYTHING MODE"));
        assert!(CONFIG_TEMPLATE.contains("a human-only knob"));
        assert!(CONFIG_TEMPLATE.contains("DO NOT LET AN AGENT EDIT THIS SECTION"));
        assert!(CONFIG_TEMPLATE.contains("worktrees/slotNN/"));
    }
}
