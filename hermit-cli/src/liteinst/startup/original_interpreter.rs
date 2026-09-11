use std::fs::File;
use std::io;
use std::os::fd::AsFd;

use super::InterpreterInputs;
use super::private_runtime::PreparedRuntimeLaunch;
use crate::Command;

pub const IMAGE_FD_ENV: &str = "HERMIT_LITEINST_ORIGINAL_INTERPRETER_FD";

pub struct OriginalInterpreterImage {
    inputs: InterpreterInputs,
    sealed: File,
}

impl OriginalInterpreterImage {
    pub fn from_held_inputs(inputs: InterpreterInputs) -> io::Result<Self> {
        inputs.program().revalidate()?;
        inputs.interpreter().revalidate()?;
        let sealed = reverie::process::sealed::create(
            c"hermit-liteinst-original-interpreter",
            inputs.interpreter().bytes(),
            libc::STDERR_FILENO + 1,
        )?;
        inputs.interpreter().revalidate()?;
        inputs.program().revalidate()?;
        Ok(Self {
            inputs,
            sealed: sealed.into(),
        })
    }

    pub fn inputs(&self) -> &InterpreterInputs {
        &self.inputs
    }

    pub fn file(&self) -> &File {
        &self.sealed
    }

    pub fn attach(
        self,
        mut runtime: PreparedRuntimeLaunch,
    ) -> io::Result<PreparedInterpreterLaunch> {
        let inherited_fd = self.inherit(runtime.command_mut())?;
        Ok(PreparedInterpreterLaunch {
            runtime,
            original: self,
            inherited_fd,
        })
    }

    pub(super) fn inherit(&self, command: &mut Command) -> io::Result<i32> {
        if command.get_env(IMAGE_FD_ENV).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "original interpreter descriptor discovery is already selected",
            ));
        }
        let descriptor = command.inherit_fd(self.sealed.as_fd())?;
        command.env(IMAGE_FD_ENV, descriptor.to_string());
        Ok(descriptor)
    }
}

pub struct PreparedInterpreterLaunch<C = Command> {
    runtime: PreparedRuntimeLaunch<C>,
    original: OriginalInterpreterImage,
    inherited_fd: i32,
}

impl<C> PreparedInterpreterLaunch<C> {
    pub(super) fn into_command(self) -> (C, PreparedInterpreterLaunch<()>) {
        let (command, runtime) = self.runtime.into_command();
        (
            command,
            PreparedInterpreterLaunch {
                runtime,
                original: self.original,
                inherited_fd: self.inherited_fd,
            },
        )
    }

    pub fn runtime(&self) -> &PreparedRuntimeLaunch<C> {
        &self.runtime
    }

    pub fn command_mut(&mut self) -> &mut C {
        self.runtime.command_mut()
    }

    pub fn original(&self) -> &OriginalInterpreterImage {
        &self.original
    }

    pub fn inherited_fd(&self) -> i32 {
        self.inherited_fd
    }
}

impl PreparedInterpreterLaunch {
    pub fn try_into_std(self) -> io::Result<PreparedInterpreterLaunch<std::process::Command>> {
        Ok(PreparedInterpreterLaunch {
            runtime: self.runtime.try_into_std()?,
            original: self.original,
            inherited_fd: self.inherited_fd,
        })
    }
}
