//! OFF controls for the public legacy launcher API. These never execute a
//! child or claim allocation, continuation, stack-placement or parity evidence.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use crate::artifact::Result;

#[derive(Debug, Eq, PartialEq)]
struct CommandInputs {
    program: OsString,
    arguments: Vec<OsString>,
    directory: Option<PathBuf>,
    environment: Vec<(OsString, Option<OsString>)>,
}

fn inputs(command: &Command) -> CommandInputs {
    CommandInputs {
        program: command.get_program().to_owned(),
        arguments: command.get_args().map(ToOwned::to_owned).collect(),
        directory: command.get_current_dir().map(ToOwned::to_owned),
        environment: command
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(ToOwned::to_owned)))
            .collect(),
    }
}

/// Require both unsupported legacy selectors to leave all caller inputs intact.
pub fn require_legacy_refusal(label: &str, alt_stack: Option<bool>) -> Result<()> {
    for (tool, selector) in [
        (reverie_liteinst::PreloadTool::Strace, "strace"),
        (reverie_liteinst::PreloadTool::Compatibility, "compat"),
    ] {
        let mut command = Command::new("legacy-refusal-control-never-executed");
        command
            .arg(label)
            .current_dir("retained-caller-directory")
            .env("LD_PRELOAD", "retained-caller-preload")
            .env("REVERIE_LITEINST_TOOL", "retained-caller-selector")
            .env("REVERIE_LITEINST_SITE_PATCHING", "0")
            .env_remove("RETAINED_CALLER_REMOVAL");
        if let Some(enabled) = alt_stack {
            command.env(
                "REVERIE_LITEINST_ALT_STACK",
                if enabled { "1" } else { "0" },
            );
        }
        let before = inputs(&command);
        let error = reverie_liteinst::configure_command(&mut command, tool)
            .err()
            .ok_or_else(|| format!("{label}: OFF {selector} launcher unexpectedly accepted"))?;
        if error.kind() != std::io::ErrorKind::Unsupported {
            return Err(format!(
                "{label}: {selector} returned an unrelated error: {error}"
            ));
        }
        let expected = format!(
            "legacy {selector} runtime was built without LiteInst; enable the `liteinst` Cargo feature or use the in-guest FullTool installation route"
        );
        if error.to_string() != expected {
            return Err(format!("{label}: {selector} refusal differs: {error}"));
        }
        if inputs(&command) != before {
            return Err(format!(
                "{label}: refused {selector} request mutated caller inputs"
            ));
        }
    }
    Ok(())
}
