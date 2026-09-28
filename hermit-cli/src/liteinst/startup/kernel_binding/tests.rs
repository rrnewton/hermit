use super::*;

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
