use super::*;

struct NativeControlLease {
    owners: Option<(
        PreparedInterpreterLaunch<()>,
        EnteredContainer,
        Arc<PreparedBinding>,
    )>,
}

impl Drop for NativeControlLease {
    fn drop(&mut self) {
        if let Some(owners) = self.owners.take() {
            std::mem::forget(owners);
        }
    }
}

fn native_control_platform() -> io::Result<()> {
    use sha2::Digest;

    if std::fs::read_to_string("/proc/sys/kernel/osrelease")?
        != "7.1.3-0_fbk0_rc18_0_gd373cd4b8dbf\n"
    {
        return Err(io::Error::other(
            "native control deployment kernel mismatch",
        ));
    }
    for (path, expected) in [
        (
            "/usr/src/kernels/7.1.3-0_fbk0_rc18_0_gd373cd4b8dbf/.config",
            "bea86ccc7b8ba0de1be69715e64d5d983c998f4d36f0d9e799f274050a28fe50",
        ),
        (
            "/usr/src/kernels/7.1.3-0_fbk0_rc18_0_gd373cd4b8dbf/arch/x86/include/asm/shstk.h",
            "a68ab5ad608652bb2cb4f7508f6052d9de938039dd6c44f8e415b1e23ca672fe",
        ),
    ] {
        let bytes = std::fs::read(path)?;
        if format!("{:x}", sha2::Sha256::digest(&bytes)) != expected {
            return Err(io::Error::other(format!(
                "native control platform input drift: {path}"
            )));
        }
    }
    Ok(())
}

fn native_control_digest(name: &str) -> io::Result<[u8; 32]> {
    let value = std::env::var(name).map_err(io::Error::other)?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other(format!("invalid digest: {name}")));
    }
    let mut digest = [0; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(io::Error::other)?;
    }
    Ok(digest)
}

fn native_control_in_entered_container(entered: EnteredContainer) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;

    use crate::liteinst::startup::original_interpreter::OriginalInterpreterImage;
    use crate::liteinst::startup::private_runtime;

    let program = std::env::var_os("PL_NATIVE_CONTROL_PROGRAM")
        .ok_or_else(|| io::Error::other("missing native control program"))?;
    let runtime = std::env::var_os("PL_NATIVE_CONTROL_ELF")
        .ok_or_else(|| io::Error::other("missing native control ELF"))?;
    let case = std::env::var("PL_NATIVE_CONTROL_CASE").map_err(io::Error::other)?;
    if case.is_empty()
        || case.len() > 80
        || !case
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(io::Error::other("invalid native control selector"));
    }
    entered.revalidate()?;
    let mut command = crate::Command::new(&program);
    command.arg(&case);
    let inputs = crate::liteinst::startup::prepare_in_current_filesystem(
        &command,
        private_runtime::MAX_IMAGE_BYTES,
    )?;
    if inputs.program().identity().sha256
        != native_control_digest("PL_NATIVE_CONTROL_PROGRAM_SHA256")?
    {
        return Err(io::Error::other("native control program digest mismatch"));
    }
    if inputs.interpreter().identity().sha256
        != native_control_digest("PL_NATIVE_CONTROL_INTERPRETER_SHA256")?
    {
        return Err(io::Error::other(
            "native control interpreter digest mismatch",
        ));
    }
    let arg0 = command.get_arg0().to_owned();
    command.program(inputs.program().lookup_path()).arg0(arg0);
    let image = private_runtime::prepare_in_current_filesystem(
        &command,
        std::path::Path::new(&runtime),
        native_control_digest("PL_NATIVE_CONTROL_ELF_SHA256")?,
        private_runtime::MAX_IMAGE_BYTES,
    )?;
    private_runtime::validate_kernel_entry(image.source().bytes())?;
    let original = OriginalInterpreterImage::from_held_inputs(inputs)?;
    let launch = original.attach(image.attach(command)?)?.try_into_std()?;
    let (mut command, launch) = launch.into_command();
    transaction::isolate_mounts(&Native, entered.identity)?;
    let plan = Transaction::prepare(&launch, &entered, &command)?;
    let snapshot = plan.enter_namespace(&Native)?;
    let namespace = File::open("/proc/thread-self/ns/mnt")?;
    if identity(&namespace_fd(&Native, namespace.as_raw_fd())?) != identity(&snapshot.namespace) {
        return Err(io::Error::other("native control namespace changed"));
    }
    let alias = plan.prepare_alias(&snapshot)?;
    let prepared = Arc::new(PreparedBinding {
        plan,
        snapshot,
        alias,
        _namespace: namespace,
    });
    let mut lease = NativeControlLease {
        owners: Some((launch, entered, prepared.clone())),
    };
    command.env(BINDING_ENV, prepared.plan.record_fd().to_string());
    let binding = prepared.clone();
    unsafe {
        command.pre_exec(move || {
            let kernel = transaction::pre_exec::PreExec {
                kernel: &Native,
                stderr: libc::STDERR_FILENO,
            };
            binding
                .plan
                .bind(&binding.snapshot, &binding.alias, &kernel)
        });
    }
    let spawned = command.spawn();
    let status = match spawned {
        Ok(mut child) => {
            eprintln!("native-control stage=spawned pid={}", child.id());
            loop {
                match child.wait() {
                    Ok(status) => break Ok(status),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        eprintln!(
                            "native-control stage=wait-refused error={error}; retaining owners"
                        );
                        std::mem::forget(child);
                        return Err(error);
                    }
                }
            }
        }
        Err(error) => {
            eprintln!("native-control stage=spawn-refused error={error}");
            Err(error)
        }
    };
    if let Ok(status) = &status {
        eprintln!(
            "native-control stage=reaped raw_status={}",
            status.into_raw()
        );
    }
    let cleanup = prepared
        .plan
        .cleanup(&prepared.snapshot, &prepared.alias, &Native);
    eprintln!("native-control stage=cleanup result={cleanup:?}");
    if cleanup.is_ok() {
        drop(lease.owners.take());
    }
    let status = status?;
    cleanup?;
    if !status.success() {
        return Err(io::Error::other(format!("native control failed: {status}")));
    }
    Ok(())
}

#[test]
fn actual_native_child_control_with_owned_interpreter_binding() {
    native_control_platform().expect("native control prelaunch deployment binding");
    let mut container = Container::new();
    let outcome = run_owned_container(&mut container, |entered| {
        entered
            .and_then(|entered| {
                native_control_platform()?;
                native_control_in_entered_container(entered)
            })
            .map_err(|error| {
                eprintln!("native-control stage=refused error={error}");
                error.to_string()
            })
    });
    let platform_postcheck = native_control_platform();
    let entered = outcome.expect("native control container setup");
    let completed = entered.expect("native control container process");
    completed.expect("native control setup/child/cleanup result; inspect stage receipts");
    platform_postcheck.expect("native control postlaunch deployment binding");
}

#[test]
fn existing_namespace_cannot_mint_entered_capability() {
    let parent = File::open("/proc/thread-self/ns/mnt").unwrap();
    assert!(EnteredContainer::after_setup(&parent).is_err());
}

#[test]
fn private_covering_mount_requires_unique_complete_record() {
    use transaction::private_mount;
    let private = "17 1 8:1 / / rw - ext4 /dev/root rw\n";
    assert!(private_mount(private, 17).is_ok());
    assert!(private_mount(private, 18).is_err());
    assert!(private_mount(&private.repeat(2), 17).is_err());
    for field in ["shared:1", "master:2", "unbindable", "unknown:3"] {
        assert!(
            private_mount(
                &format!("17 1 8:1 / / rw {field} - ext4 /dev/root rw\n"),
                17
            )
            .is_err()
        );
    }
}

#[test]
fn modeled_optional_fields_distinguish_propagation_from_unsupported_metadata() {
    use transaction::private_mount;
    assert!(private_mount("17 1 8:1 / / rw idmapped - ext4 /dev/root rw", 17).is_ok());
    for (field, reason) in [
        ("shared:1", "shared propagation"),
        ("master:2", "slave propagation"),
        ("propagate_from:3", "propagation source"),
        ("unbindable", "unbindable, not private"),
        ("unknown:3", "unsupported mountinfo optional field"),
        ("idmapped:3", "unsupported mountinfo optional field"),
    ] {
        let row = format!("17 1 8:1 / / rw idmapped {field} - ext4 /dev/root rw");
        let error = private_mount(&row, 17).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        let message = error.to_string();
        assert!(message.contains(reason), "{message}");
        assert!(message.contains("covering_mount_id=17"));
        assert!(message.contains("separator=Some(8)"));
        assert!(message.contains(&format!("row_prefix={row:?}")));
    }
}

#[test]
fn modeled_mountinfo_diagnostics_are_bounded_and_escaped() {
    use transaction::private_mount;
    let row = format!(
        "17 1 8:1 / / rw unknown:\u{1b}{} - ext4 /dev/root rw",
        "é".repeat(1024)
    );
    let message = private_mount(&row, 17).unwrap_err().to_string();
    assert!(message.contains("\\u{1b}"));
    assert!(!message.contains('\u{1b}'));
    assert!(!message.contains('\n'));
    assert!(message.len() < 1024);
    let end = if row.is_char_boundary(512) { 512 } else { 511 };
    assert!(message.contains(&format!("omitted_bytes={}", row.len() - end)));
}

#[test]
fn modeled_covering_mount_requires_complete_suffix_and_exact_id() {
    use transaction::private_mount;
    for row in [
        "17 1 8:1 / / rw ext4 /dev/root rw",
        "17 1 8:1 / / - ext4 /dev/root rw",
        "17 1 8:1 / / rw - ext4 /dev/root",
        "17 1 8:1 / / rw - ext4 /dev/root rw extra",
    ] {
        let error = private_mount(row, 17).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("malformed mountinfo record"));
    }
    let rows =
        "117 1 8:1 / / rw shared:1 - ext4 /dev/root rw\n17 1 8:1 / / rw - ext4 /dev/root rw\n";
    assert!(private_mount(rows, 17).is_ok());
    assert!(private_mount(rows, 117).is_err());
}

#[test]
fn ordinary_kernel_execute_permissions_are_required() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("not-executed");
    std::fs::write(&path, b"ordinary permission fixture").unwrap();
    let file = File::open(&path).unwrap();
    for mode in [0o600, 0o4755, 0o2755] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(transaction::executable(&Native, file.as_raw_fd()).is_err());
    }
}
