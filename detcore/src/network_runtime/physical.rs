//! Custody-purpose identities, deliberately independent of scheduler signal pidfds.
use std::collections::BTreeMap;

use crate::network_replay::NetworkStreamOwner;
use crate::types::DetTid;
use crate::types::MmId;

#[derive(Debug)]
struct Task<T> {
    mm: MmId,
    process: i32,
    thread: i32,
    handle: T,
}
#[derive(Debug)]
pub(super) struct CustodyTasks<T> {
    tasks: BTreeMap<DetTid, Task<T>>,
}
impl<T> Default for CustodyTasks<T> {
    fn default() -> Self {
        Self {
            tasks: BTreeMap::new(),
        }
    }
}
impl<T> CustodyTasks<T> {
    /// The opener executes only after identity checks and before publication.
    /// Its failure leaves any previously registered incarnation untouched.
    pub(super) fn register(
        &mut self,
        owner: NetworkStreamOwner,
        process: i32,
        thread: i32,
        open: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<()> {
        if process <= 0 || thread <= 0 || thread != owner.thread.as_raw() as i32 {
            return Err(std::io::Error::other(
                "ptrace custody task identity mismatch",
            ));
        }
        if let Some(old) = self.tasks.get(&owner.thread) {
            if old.mm == owner.mm {
                return if old.process == process && old.thread == thread {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "custody identity changed within one MM",
                    ))
                };
            }
        }
        let handle = open()?;
        self.tasks.insert(
            owner.thread,
            Task {
                mm: owner.mm,
                process,
                thread,
                handle,
            },
        );
        Ok(())
    }
    pub(super) fn get(&self, owner: NetworkStreamOwner) -> std::io::Result<&T> {
        self.tasks
            .get(&owner.thread)
            .filter(|task| task.mm == owner.mm)
            .map(|task| &task.handle)
            .ok_or_else(|| std::io::Error::other("unregistered custody task/MM"))
    }
    pub(super) fn forget(&mut self, owner: NetworkStreamOwner) {
        if self
            .tasks
            .get(&owner.thread)
            .is_some_and(|task| task.mm == owner.mm)
        {
            self.tasks.remove(&owner.thread);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn owner(thread: i32) -> NetworkStreamOwner {
        let thread = DetTid::from_raw(thread);
        NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        }
    }
    #[test]
    fn wrong_task_and_failed_open_never_replace_authority() {
        let mut tasks = CustodyTasks::default();
        let old = owner(7);
        assert!(tasks.register(old, 7, 8, || Ok(99)).is_err());
        tasks.register(old, 7, 7, || Ok(1)).unwrap();
        tasks
            .register(old, 7, 7, || {
                panic!("idempotent registration reopened task")
            })
            .unwrap();
        assert!(tasks.register(old, 8, 7, || Ok(2)).is_err());
        let new = NetworkStreamOwner {
            mm: old.mm.for_exec(old.thread),
            ..old
        };
        assert!(
            tasks
                .register(new, 7, 7, || Err(std::io::Error::other("pidfd failure")))
                .is_err()
        );
        assert_eq!(*tasks.get(old).unwrap(), 1);
        assert!(tasks.get(new).is_err());
        tasks.register(new, 7, 7, || Ok(3)).unwrap();
        tasks.forget(old);
        assert_eq!(*tasks.get(new).unwrap(), 3);
        tasks.forget(new);
        assert!(tasks.get(new).is_err());
    }
}
