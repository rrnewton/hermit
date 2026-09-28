use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
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
    Preload(Box<PreparedPreloadLaunch>),
    Private(Box<PreparedInterpreterLaunch<std::process::Command>>),
}

pub(crate) struct PreparedPreloadLaunch<C = Command> {
    command: C,
    source: PinnedImage,
    sealed: File,
    inherited_fd: i32,
}

impl PreparedPreloadLaunch<Command> {
    fn from_source(mut command: Command, source: PinnedImage) -> io::Result<Self> {
        if command
            .get_env(crate::liteinst_bootstrap::RUNTIME_IMAGE_FD_ENV)
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "preload runtime descriptor discovery is already selected",
            ));
        }
        let sealed = reverie::process::sealed::create(
            c"hermit-liteinst-preload",
            source.bytes(),
            libc::STDERR_FILENO + 1,
        )?;
        source.revalidate()?;
        let sealed = File::from(sealed);
        let inherited_fd = command.inherit_fd(sealed.as_fd())?;
        command.env(
            crate::liteinst_bootstrap::RUNTIME_IMAGE_FD_ENV,
            inherited_fd.to_string(),
        );
        Ok(Self {
            command,
            source,
            sealed,
            inherited_fd,
        })
    }
}

impl<C> PreparedPreloadLaunch<C> {
    pub(crate) fn source(&self) -> &PinnedImage {
        &self.source
    }

    #[cfg(test)]
    pub(crate) fn file(&self) -> &File {
        &self.sealed
    }

    #[cfg(test)]
    pub(crate) fn inherited_fd(&self) -> i32 {
        self.inherited_fd
    }

    fn preload_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.inherited_fd))
    }

    fn into_command(self) -> (C, PreparedPreloadLaunch<()>) {
        (
            self.command,
            PreparedPreloadLaunch {
                command: (),
                source: self.source,
                sealed: self.sealed,
                inherited_fd: self.inherited_fd,
            },
        )
    }
}

fn prepare_preload_command(
    mut launch: PreparedPreloadLaunch,
) -> io::Result<(std::process::Command, PreparedPreloadLaunch<()>)> {
    // Keep the descriptor spelling. Canonicalizing a sealed anonymous file
    // produces a "(deleted)" pathname that the guest's dynamic loader cannot open.
    let mut ld_preload = launch.preload_path().into_os_string();
    if let Some(existing) = launch
        .command
        .get_captured_envs()
        .remove(OsStr::new("LD_PRELOAD"))
        .filter(|value| !value.is_empty())
    {
        ld_preload.push(OsStr::new(":"));
        ld_preload.push(existing);
    }
    launch.command.env("LD_PRELOAD", ld_preload);
    let arg0 = launch.command.get_arg0().to_owned();
    let program = launch.command.find_program()?;
    launch.command.program(program).arg0(arg0);
    let (command, owner) = launch.into_command();
    Ok((command.try_into_std()?, owner))
}

pub(crate) fn owned_preload_command(
    launch: PreparedPreloadLaunch,
) -> io::Result<reverie_liteinst::PreparedCommand<PreparedPreloadLaunch<()>>> {
    let (command, owner) = prepare_preload_command(launch)?;
    Ok(reverie_liteinst::PreparedCommand::new(command, owner))
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
        return PreparedPreloadLaunch::from_source(command, source)
            .map(|launch| RuntimeLaunch::Preload(Box::new(launch)));
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
