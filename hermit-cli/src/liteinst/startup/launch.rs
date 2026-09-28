use std::io;
use std::path::Path;
use std::path::PathBuf;

use goblin::elf::Elf;

use super::PinnedImage;
use super::original_interpreter::OriginalInterpreterImage;
use super::original_interpreter::PreparedInterpreterLaunch;
use super::private_runtime;
use super::private_runtime::PrivateRuntimeImage;
use crate::Command;

pub(crate) enum RuntimeLaunch {
    Preload(Box<Command>, PathBuf),
    Private(Box<PreparedInterpreterLaunch<std::process::Command>>),
}

pub(crate) fn prepare(command: Command, path: &Path) -> io::Result<RuntimeLaunch> {
    let path = super::lookup_path(&command, path)?;
    let source = PinnedImage::open_with_permissions(
        path.to_path_buf(),
        private_runtime::MAX_IMAGE_BYTES,
        false,
    )?;
    crate::liteinst_artifact::validate_snapshot_identity(
        &path,
        source.bytes(),
        env!("HERMIT_REVERIE_PIN"),
        option_env!("HERMIT_LITEINST_SOURCE_SHA256").unwrap_or("unknown"),
        option_env!("HERMIT_LITEINST_DIAGNOSTIC_BUILD") == Some("1"),
        option_env!("HERMIT_LITEINST_RESOLVED_REVERIE_REV").unwrap_or("unknown"),
    )?;
    source.revalidate()?;
    select(command, source)
}

fn select(mut command: Command, source: PinnedImage) -> io::Result<RuntimeLaunch> {
    let elf = Elf::parse(source.bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let private = elf.entry != 0
        || elf.syms.iter().any(|symbol| {
            matches!(
                elf.strtab.get_at(symbol.st_name),
                Some(private_runtime::KERNEL_SYMBOL | private_runtime::CRT_SYMBOL)
            )
        });
    if !private {
        return Ok(RuntimeLaunch::Preload(
            Box::new(command),
            source.lookup_path().to_owned(),
        ));
    }
    private_runtime::validate_kernel_entry(source.bytes())?;
    let inputs = super::prepare_in_current_filesystem(&command, private_runtime::MAX_IMAGE_BYTES)?;
    let arg0 = command.get_arg0().to_owned();
    command.program(inputs.program().lookup_path()).arg0(arg0);
    let original = OriginalInterpreterImage::from_held_inputs(inputs)?;
    let runtime = PrivateRuntimeImage::from_source(source)?.attach(command)?;
    let launch = original.attach(runtime)?.try_into_std()?;
    Ok(RuntimeLaunch::Private(Box::new(launch)))
}

pub(crate) fn owned_command(
    launch: PreparedInterpreterLaunch<std::process::Command>,
    entered: Option<super::kernel_binding::EnteredContainer>,
) -> reverie_liteinst::PreparedCommand<super::kernel_binding::BindingOwner> {
    super::kernel_binding::owned_command(launch, entered)
}

#[cfg(test)]
mod tests;
