use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::liteinst::startup::kernel_binding::identity;
use crate::liteinst::startup::kernel_binding::run_owned_container;

#[test]
fn anonymous_source_actual_transaction_and_checked_cleanup() {
    let fixture = tempfile::tempdir().unwrap();
    let paths = ["program", "interpreter", "artifact"].map(|name| fixture.path().join(name));
    for path in &paths {
        std::fs::write(path, b"ordinary original bytes").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let parent = File::open("/proc/thread-self/ns/mnt").unwrap();
    let parent_id = identity(&namespace_fd(&Native, parent.as_raw_fd()).unwrap());
    let mut container = reverie::process::Container::new();
    container.map_root();
    let result = run_owned_container(&mut container, |entered| {
        let run = || -> io::Result<()> {
            let entered = entered?;
            entered.revalidate()?;
            isolate_mounts(&Native, entered.identity)?;
            let held = paths
                .iter()
                .map(File::open)
                .collect::<io::Result<Vec<_>>>()?;
            let expected = held
                .iter()
                .map(|file| executable(&Native, file.as_raw_fd()))
                .collect::<io::Result<Vec<_>>>()?;
            let runtime =
                reverie::process::sealed::create(c"ordinary-runtime", b"sealed runtime bytes", 3)?;
            let image = reverie::process::sealed::create(
                c"ordinary-interpreter",
                b"ordinary original bytes",
                3,
            )?;
            let record = descriptor(unsafe {
                File::from_raw_fd(call(
                    &Native,
                    libc::SYS_memfd_create,
                    [
                        c"binding-test".as_ptr() as usize,
                        (libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) as usize,
                        0,
                        0,
                        0,
                        0,
                    ],
                )? as i32)
            })?;
            let target = CString::new(paths[1].as_os_str().as_bytes())?;
            let original = descriptor(
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
                    .open(&paths[1])?,
            )?;
            let plan = Transaction {
                record,
                original,
                target: target.clone(),
                interpreter_path: target.clone(),
                kernel_interpreter_path: target,
                program_path: CString::new(paths[0].as_os_str().as_bytes())?,
                source_path: CString::new(paths[2].as_os_str().as_bytes())?,
                runtime_path: CString::new(format!("/proc/self/fd/{}", runtime.as_raw_fd()))?,
                namespace: entered.identity,
                runtime: runtime.as_raw_fd(),
                original_image: image.as_raw_fd(),
                source_fds: [
                    held[0].as_raw_fd(),
                    held[1].as_raw_fd(),
                    held[2].as_raw_fd(),
                ],
                expected: expected
                    .try_into()
                    .map_err(|_| io::Error::other("identity count"))?,
            };
            let snapshot = plan.enter_namespace(&Native)?;
            let mountinfo = std::fs::read("/proc/thread-self/mountinfo")?;
            let alias = plan.prepare_alias(&snapshot)?;
            assert_eq!(mountinfo, std::fs::read("/proc/thread-self/mountinfo")?);
            alias.verify(&Native)?;
            plan.bind(&snapshot, &alias, &Native)?;
            let selected = File::open(&paths[1])?;
            let selected_stat = stat_fd(&Native, selected.as_raw_fd())?;
            let runtime_stat = stat_fd(&Native, runtime.as_raw_fd())?;
            assert_eq!(identity(&selected_stat), identity(&runtime_stat));
            assert_eq!(selected_stat.stx_mnt_id, runtime_stat.stx_mnt_id);
            assert_eq!(std::fs::read(&paths[1])?, b"sealed runtime bytes");
            assert_eq!(
                call(
                    &Native,
                    libc::SYS_fcntl,
                    [
                        selected.as_raw_fd() as usize,
                        libc::F_GET_SEALS as usize,
                        0,
                        0,
                        0,
                        0
                    ]
                )?,
                i64::from(SEALS)
            );
            assert_eq!(
                call(
                    &Native,
                    libc::SYS_fcntl,
                    [
                        plan.record_fd() as usize,
                        libc::F_GET_SEALS as usize,
                        0,
                        0,
                        0,
                        0
                    ]
                )?,
                i64::from(SEALS)
            );
            eprintln!(
                "actual alias attached: runtime={:?}/mnt{}; source root={:?}/mnt{}; alias mount={}",
                identity(&runtime_stat),
                runtime_stat.stx_mnt_id,
                identity(&alias.identity.root),
                alias.identity.root.stx_mnt_id,
                stat_fd(&Native, alias.tree.as_raw_fd())?.stx_mnt_id
            );
            plan.cleanup(&snapshot, &alias, &Native)?;
            assert_eq!(std::fs::read(&paths[1])?, b"ordinary original bytes");
            assert_eq!(mountinfo, std::fs::read("/proc/thread-self/mountinfo")?);
            let missing = stat_at(
                &Native,
                alias.root.as_raw_fd(),
                c"runtime".as_ptr() as usize,
                libc::AT_SYMLINK_NOFOLLOW,
            )
            .err()
            .ok_or_else(|| io::Error::other("source remains linked"))?;
            assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
            plan.cleanup(&snapshot, &alias, &Native)?;
            eprintln!(
                "verified restoration, linked source removed, unchanged namespace mountinfo; idempotent post-reap cleanup"
            );
            Ok(())
        };
        run().map_err(|error| error.to_string())
    });
    let after = File::open("/proc/thread-self/ns/mnt").unwrap();
    assert_eq!(
        identity(&namespace_fd(&Native, after.as_raw_fd()).unwrap()),
        parent_id
    );
    for path in &paths {
        assert_eq!(std::fs::read(path).unwrap(), b"ordinary original bytes");
    }
    assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 3);
    eprintln!("parent files/namespace/inventory unchanged; child result={result:?}");
    result.unwrap().unwrap().unwrap();
}
