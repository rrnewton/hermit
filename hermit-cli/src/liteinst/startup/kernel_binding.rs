#[cfg(test)]
mod tests;
mod transaction;

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::OnceLock;

use reverie::process::Container;
use reverie::process::Namespace;
use reverie::process::RunError;
use transaction::NamespaceSnapshot;
use transaction::Native;
use transaction::Transaction;
use transaction::identity;
use transaction::namespace_fd;

use super::original_interpreter::PreparedInterpreterLaunch;

pub const BINDING_ENV: &str = "HERMIT_LITEINST_INTERPRETER_BINDING_FD";

pub struct EnteredContainer {
    namespace: File,
    identity: (u64, u64),
}

impl EnteredContainer {
    fn after_setup(parent: &File) -> io::Result<Self> {
        let namespace = File::open("/proc/thread-self/ns/mnt")?;
        let current = identity(&namespace_fd(&Native, namespace.as_raw_fd())?);
        if current == identity(&namespace_fd(&Native, parent.as_raw_fd())?) {
            return Err(io::Error::other(
                "container did not create its owned mount namespace",
            ));
        }
        Ok(Self {
            namespace,
            identity: current,
        })
    }

    fn revalidate(&self) -> io::Result<()> {
        let current = File::open("/proc/thread-self/ns/mnt")?;
        if identity(&namespace_fd(&Native, current.as_raw_fd())?) != self.identity
            || identity(&namespace_fd(&Native, self.namespace.as_raw_fd())?) != self.identity
        {
            return Err(io::Error::other("entered container namespace changed"));
        }
        Ok(())
    }
}

pub fn run_owned_container<F, T>(
    container: &mut Container,
    mut body: F,
) -> io::Result<Result<T, RunError>>
where
    F: FnMut(io::Result<EnteredContainer>) -> T,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let parent = File::open("/proc/thread-self/ns/mnt")?;
    container.unshare(Namespace::MOUNT);
    Ok(container.run(|| body(EnteredContainer::after_setup(&parent))))
}

pub(crate) struct BindingOwner {
    launch: PreparedInterpreterLaunch<()>,
    entered: Option<EnteredContainer>,
    transaction: OnceLock<Arc<PreparedBinding>>,
    configuration_owner: Option<Box<dyn Send>>,
}

struct PreparedBinding {
    plan: Transaction,
    snapshot: NamespaceSnapshot,
    alias: transaction::alias::AliasSource,
    _namespace: File,
}

pub(crate) fn configure_launch(
    configure: Option<crate::liteinst::NamespaceConfigurator>,
) -> impl FnOnce(
    &mut BindingOwner,
    &std::process::Command,
    &mut crate::DetConfig,
) -> Result<(), reverie::Error>
+ Send {
    move |owner, command, config| {
        let configure = configure
            .ok_or_else(|| io::Error::other("missing final namespace configuration producer"))?;
        let entered = owner.entered.as_ref().ok_or_else(|| {
            io::Error::other("private LiteInst requires an owned entered container")
        })?;
        entered.revalidate()?;
        transaction::isolate_mounts(&Native, entered.identity)?;
        let plan = Transaction::prepare(&owner.launch, entered, command)?;
        let snapshot = plan.enter_namespace(&Native)?;
        let alias = plan.prepare_alias(&snapshot)?;
        let namespace = File::open("/proc/thread-self/ns/mnt")?;
        if identity(&namespace_fd(&Native, namespace.as_raw_fd())?) != identity(&snapshot.namespace)
        {
            return Err(io::Error::other("final launch namespace changed").into());
        }
        owner
            .transaction
            .set(Arc::new(PreparedBinding {
                plan,
                snapshot,
                alias,
                _namespace: namespace,
            }))
            .map_err(|_| io::Error::other("binding transaction reused"))?;
        owner.configuration_owner = Some(configure(config).map_err(io::Error::other)?);
        Ok(())
    }
}

pub(crate) fn owned_command(
    launch: PreparedInterpreterLaunch<std::process::Command>,
    entered: Option<EnteredContainer>,
) -> reverie_liteinst::PreparedCommand<BindingOwner> {
    let (command, launch) = launch.into_command();
    let owner = BindingOwner {
        launch,
        entered,
        transaction: OnceLock::new(),
        configuration_owner: None,
    };
    reverie_liteinst::PreparedCommand::new(command, owner).with_cleanup(|owner| {
        if let Some(prepared) = owner.transaction.get() {
            prepared.plan.cleanup(&prepared.snapshot, &prepared.alias, &Native)?;
        }
        Ok(())
    }).with_spawn_check(|owner, command| {
        if owner.entered.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private LiteInst requires an owned entered container; refusing preload fallback",
            )
            .into());
        }
        let prepared = owner
            .transaction
            .get()
            .ok_or_else(|| io::Error::other("final namespace was not prepared before GlobalTool"))?
            .clone();
        command.env(BINDING_ENV, prepared.plan.record_fd().to_string());
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || {
                let kernel = transaction::pre_exec::PreExec {
                    kernel: &Native,
                    stderr: libc::STDERR_FILENO,
                };
                prepared.plan.bind(&prepared.snapshot, &prepared.alias, &kernel).inspect_err(|_| {
                    kernel.report(b"liteinst pre_exec binding: transaction failed (original errno follows)\n");
                })
            });
        }
        Ok(())
    })
}
