/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Interpreter descriptor-view and genuine relative-CWD mount controls.
//! All target syscalls run in the shared parent's monitored ordinary helper.

mod exec_support;

use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use exec_support::ChildSetup;
use exec_support::MountedCwd;
use exec_support::NativeObservation;
use exec_support::PrepareObservation;
use exec_support::RawFault;
use reverie_elf_loader::ExecCheckOutcome;
use reverie_elf_loader::ExecRequest;
use reverie_elf_loader::Limits;
use reverie_elf_loader::LoaderHostFacts;
use reverie_elf_loader::PrepareExecOptions;
use reverie_elf_loader::exec::NativeErrorStage;
use reverie_elf_loader::host::RetainedLookupRoot;
use reverie_elf_loader::prepare_exec;

static SERIAL: Mutex<()> = Mutex::new(());
const ANCESTOR_DIRECTORY: &str = "REVERIE_LB2_ANCESTOR_DIRECTORY";
const ANCESTOR_FORMAT: &str = "REVERIE_LB2_ANCESTOR_FORMAT";
const ANCESTOR_SEARCHABLE: &str = "REVERIE_LB2_ANCESTOR_SEARCHABLE";

#[test]
fn lb_prepare_child() {
    exec_support::prepare_child_entry();
}

fn directory(name: &str) -> PathBuf {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
    exec_support::fixture_dir_in(&base, name)
}

fn request(path: &Path) -> ExecRequest {
    ExecRequest::execve(path, vec![CString::new("guest-argv0").unwrap()], Vec::new()).unwrap()
}

fn executable(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn native_interpreter() -> PathBuf {
    let bytes = fs::read("/bin/true").unwrap();
    let header = interpreter_header(&bytes);
    let offset = u64::from_le_bytes(bytes[header + 8..header + 16].try_into().unwrap()) as usize;
    let length = u64::from_le_bytes(bytes[header + 32..header + 40].try_into().unwrap()) as usize;
    PathBuf::from(std::ffi::OsStr::from_bytes(
        &bytes[offset..offset + length - 1],
    ))
}

fn interpreter_header(bytes: &[u8]) -> usize {
    let start = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let size = usize::from(u16::from_le_bytes(bytes[54..56].try_into().unwrap()));
    (0..usize::from(u16::from_le_bytes(bytes[56..58].try_into().unwrap())))
        .map(|index| start + index * size)
        .find(|offset| u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().unwrap()) == 3)
        .unwrap()
}

fn with_interpreter(directory: &Path, name: &str, interpreter: &Path) -> PathBuf {
    let mut bytes = fs::read("/bin/true").unwrap();
    let header = interpreter_header(&bytes);
    let offset = bytes.len() as u64;
    let name_bytes = CString::new(interpreter.as_os_str().as_bytes()).unwrap();
    let length = name_bytes.as_bytes_with_nul().len() as u64;
    bytes.extend_from_slice(name_bytes.as_bytes_with_nul());
    bytes[header + 8..header + 16].copy_from_slice(&offset.to_le_bytes());
    bytes[header + 32..header + 40].copy_from_slice(&length.to_le_bytes());
    bytes[header + 40..header + 48].copy_from_slice(&length.to_le_bytes());
    let path = directory.join(name);
    executable(&path, &bytes);
    path
}

fn mount_fixture(source: &Path, target: &Path, filesystem: Option<&std::ffi::CStr>, flags: u64) {
    let source = CString::new(source.as_os_str().as_bytes()).unwrap();
    let target = CString::new(target.as_os_str().as_bytes()).unwrap();
    // SAFETY: these owned fixture pathnames are beneath target in a fresh
    // private mount namespace. No host mount or configuration is changed.
    assert_eq!(
        unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                filesystem.map_or(std::ptr::null(), std::ffi::CStr::as_ptr),
                flags,
                std::ptr::null(),
            )
        },
        0,
        "private ancestor fixture mount: {}",
        std::io::Error::last_os_error()
    );
}

fn native_ancestor_exec(request: &ExecRequest, directory: &Path) -> NativeObservation {
    let report_path = directory.join("native-errno");
    let report = File::create(&report_path).unwrap();
    let argv: Vec<_> = request
        .argv
        .iter()
        .map(|argument| argument.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp: Vec<_> = request
        .envp
        .iter()
        .map(|argument| argument.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    // SAFETY: after fork the child performs only execveat/write/_exit with
    // these prebuilt buffers and the already-opened report descriptor.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            libc::syscall(
                libc::SYS_execveat,
                request.dirfd,
                request.path.as_ptr(),
                argv.as_ptr(),
                envp.as_ptr(),
                request.flags,
            );
            let errno = *libc::__errno_location();
            let bytes = errno.to_le_bytes();
            libc::write(report.as_raw_fd(), bytes.as_ptr().cast(), bytes.len());
            libc::_exit(111);
        }
    }
    let status = exec_support::run_monitored_fork(
        pid,
        "native interpreter ancestor companion",
        Duration::from_secs(1),
    );
    let bytes = fs::read(report_path).unwrap();
    if !bytes.is_empty() {
        assert_eq!(bytes.len(), 4);
        assert_eq!(status.code(), Some(111));
        return NativeObservation::Errno(i32::from_le_bytes(bytes.try_into().unwrap()));
    }
    if let Some(code) = status.code() {
        NativeObservation::Exited(code)
    } else {
        NativeObservation::Signaled(status.signal().unwrap())
    }
}

#[test]
fn lb2_interpreter_ancestor_child() {
    let Some(directory) = std::env::var_os(ANCESTOR_DIRECTORY) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let format = std::env::var(ANCESTOR_FORMAT).unwrap();
    let searchable = std::env::var(ANCESTOR_SEARCHABLE).unwrap() == "1";
    mount_fixture(
        Path::new("tmpfs"),
        &directory,
        Some(c"tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
    );
    let blocked = directory.join("blocked");
    let retained_mount = blocked.join("mnt");
    let source = directory.join("source");
    fs::create_dir_all(&retained_mount).unwrap();
    fs::create_dir(&source).unwrap();
    fs::copy(native_interpreter(), source.join("present-elf")).unwrap();
    fs::copy("/bin/true", source.join("present-script")).unwrap();
    mount_fixture(&source, &retained_mount, None, libc::MS_BIND);
    let mut cases = Vec::new();
    for present in [false, true] {
        let interpreter = retained_mount.join(if present {
            if format == "elf" {
                "present-elf"
            } else {
                "present-script"
            }
        } else {
            "missing"
        });
        let target = if format == "elf" {
            with_interpreter(&directory, if present { "Tp" } else { "Tm" }, &interpreter)
        } else {
            assert_eq!(format, "script");
            let target = directory.join(if present { "Sp" } else { "Sm" });
            let contents = format!("#!{}\n", interpreter.display());
            assert!(
                contents.len() < 256,
                "complete shebang interpreter pathname"
            );
            executable(&target, contents.as_bytes());
            target
        };
        cases.push((present, request(&target)));
    }
    // Root in this private user namespace must not bypass ancestor DAC or
    // regain that bypass when the native companion execs. This affects only
    // the helper; both CHECK, preparation and native exec use the same state.
    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: i32,
    }
    let header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let capabilities = [[0u32; 3]; 2];
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_SECUREBITS, 1, 0, 0, 0), 0); // NOROOT.
        assert_eq!(
            libc::syscall(libc::SYS_capset, &header, capabilities.as_ptr()),
            0
        );
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
    }
    let host = exec_support::modeled_host(None);
    let mut roots: Vec<_> = host
        .retained_lookup_roots()
        .unwrap()
        .iter()
        .map(|root| RetainedLookupRoot {
            mount_point: root.mount_point.clone(),
            directory: root.directory.try_clone().unwrap(),
        })
        .collect();
    for path in [&directory, &retained_mount] {
        roots.push(RetainedLookupRoot {
            mount_point: path.as_os_str().as_bytes().to_vec(),
            directory: OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
                .open(path)
                .unwrap(),
        });
    }
    // SAFETY: retain both actual namespace mounts while all ancestors are
    // searchable. Their exact bindings and namespace remain frozen below;
    // security/binfmt/watch receipts remain explicit inactive models.
    let host = unsafe { host.with_lookup_roots(roots).unwrap() };
    let loader_host = LoaderHostFacts::current().unwrap();
    let options = PrepareExecOptions {
        launcher_link: Path::new("/lb"),
        host: &host,
        limits: Limits::current().unwrap(),
        loader_host: &loader_host,
        inherited_virtual_proc_state: false,
        interpreter_writer_fds: &[],
    };
    if !searchable {
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
    }
    for (present, request) in cases {
        let argv = [request.argv[0].as_ptr(), std::ptr::null()];
        let envp = [std::ptr::null::<libc::c_char>()];
        // CHECK skips format handling and therefore has not looked up I.
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_execveat,
                    request.dirfd,
                    request.path.as_ptr(),
                    argv.as_ptr(),
                    envp.as_ptr(),
                    reverie_elf_loader::exec::AT_EXECVE_CHECK,
                )
            },
            0
        );
        let native = native_ancestor_exec(&request, &directory);
        let expected = if !searchable {
            NativeObservation::Errno(libc::EACCES)
        } else if !present {
            NativeObservation::Errno(libc::ENOENT)
        } else {
            NativeObservation::Exited(0)
        };
        assert_eq!(
            native, expected,
            "{format}: searchable={searchable}, present={present}"
        );
        // SAFETY: one ordinary preparation task has exclusive FD mutation;
        // namespace, credentials and all fixture files stay fixed during it.
        let outcome = unsafe { prepare_exec(&request, &options) };
        match expected {
            NativeObservation::Errno(errno) => match outcome {
                ExecCheckOutcome::NativeErrno(error) => {
                    assert_eq!(error.errno, errno);
                    assert_eq!(error.stage, NativeErrorStage::InterpreterOpen);
                }
                other => panic!("ordered original-path interpreter errno required: {other:?}"),
            },
            NativeObservation::Exited(0) => match outcome {
                ExecCheckOutcome::Prepared(prepared) => prepared.verify_objects().unwrap(),
                other => panic!("searchable ancestor and present interpreter: {other:?}"),
            },
            other => panic!("unexpected native ancestor result: {other:?}"),
        }
    }
    println!(
        "PASS {format}: searchable={searchable}; exact native/preparation ancestor ordering, missing and present interpreter companions"
    );
}

fn compare_interpreter_ancestors(format: &str) {
    for searchable in [false, true] {
        let directory = directory("lb2-ancestor");
        let output = directory.join("stdout");
        let errors = directory.join("stderr");
        let mut command = Command::new("unshare");
        command
            .args(["--user", "--map-root-user", "--mount", "--"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lb2_interpreter_ancestor_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ANCESTOR_DIRECTORY, &directory)
            .env(ANCESTOR_FORMAT, format)
            .env(ANCESTOR_SEARCHABLE, if searchable { "1" } else { "0" })
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(&output).unwrap()))
            .stderr(Stdio::from(File::create(&errors).unwrap()));
        let status = exec_support::run_monitored(command, "interpreter ancestor namespace");
        assert!(
            status.success(),
            "{format}, searchable={searchable}: {status}\n{}\n{}",
            fs::read_to_string(output).unwrap(),
            fs::read_to_string(errors).unwrap()
        );
    }
}

#[test]
fn lb2_pt_interp_preserves_inaccessible_and_searchable_ancestor_errors() {
    let _serial = SERIAL.lock().unwrap();
    compare_interpreter_ancestors("elf");
}

#[test]
fn lb2_shebang_preserves_inaccessible_and_searchable_ancestor_errors() {
    let _serial = SERIAL.lock().unwrap();
    compare_interpreter_ancestors("script");
}

#[test]
fn lb2_interpreter_symlink_and_repeated_slash_aliases_preserve_descriptor_errors() {
    let _serial = SERIAL.lock().unwrap();
    let directory = directory("lb-interpreter-alias");
    fs::create_dir(directory.join("cwd")).unwrap();
    std::os::unix::fs::symlink("/proc/self/fd", directory.join("cwd/alias")).unwrap();
    let setup = ChildSetup {
        cwd: Some(directory.join("cwd")),
        closed_fds: vec![3],
        ..ChildSetup::default()
    };
    for (index, interpreter, refusal) in [
        ("alias/3/child", "LookupMountUnverified"),
        ("alias//3///child", "LookupMountUnverified"),
        ("//proc/self//fd///3/child", "LookupMountUnverified"),
        ("/proc//self//fd//3/child", "LookupMountUnverified"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (path, refusal))| (index, path, refusal))
    {
        let target = with_interpreter(&directory, &format!("T{index}"), Path::new(interpreter));
        let script = directory.join(format!("S{index}"));
        executable(&script, format!("#!{interpreter}\n").as_bytes());
        for path in [target, script] {
            let request = request(&path);
            assert_eq!(exec_support::run_check(&request, &setup, &directory), 0);
            assert_eq!(
                exec_support::run_native(&request, &setup, &directory),
                NativeObservation::Errno(libc::ENOENT)
            );
            assert_eq!(
                exec_support::run_prepare(&request, &setup, &directory),
                PrepareObservation::Refusal(refusal.into()),
                "private reservations must never produce a native ENOTDIR"
            );
        }
    }

    // Ordinary filesystem symlinks and redundant slashes remain usable.
    let interpreter = native_interpreter();
    fs::copy(&interpreter, directory.join("local-I")).unwrap();
    std::os::unix::fs::symlink("../local-I", directory.join("cwd/ordinary-link")).unwrap();
    for (index, interpreter) in [
        PathBuf::from("ordinary-link"),
        PathBuf::from(format!("////{}", interpreter.display())),
    ]
    .into_iter()
    .enumerate()
    {
        let target = with_interpreter(&directory, &format!("control{index}"), &interpreter);
        let request = request(&target);
        assert_eq!(
            exec_support::run_native(&request, &setup, &directory),
            NativeObservation::Exited(0)
        );
        assert!(matches!(
            exec_support::run_prepare(&request, &setup, &directory),
            PrepareObservation::Prepared(_)
        ));
    }
}

#[test]
fn lb2_generic_proc_aliases_are_refused_before_original_or_interpreter_lookup() {
    let _serial = SERIAL.lock().unwrap();
    let directory = directory("lb-generic-proc-alias");
    std::os::unix::fs::symlink("/proc/self", directory.join("proc-alias")).unwrap();
    let setup = ChildSetup {
        cwd: Some(directory.clone()),
        ..ChildSetup::default()
    };
    // status is an ordinary proc regular file, rather than a magic link. A
    // NO_MAGICLINKS-only probe would already traverse its unqualified handler.
    for (index, path) in [
        PathBuf::from("/proc/self/status"),
        PathBuf::from("//proc//self///status"),
        PathBuf::from("proc-alias/status"),
        PathBuf::from("proc-alias//status"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut paths = vec![path.clone()];
        paths.push(with_interpreter(&directory, &format!("T{index}"), &path));
        let script = directory.join(format!("S{index}"));
        executable(&script, format!("#!{}\n", path.display()).as_bytes());
        paths.push(script);
        for executable in paths {
            let request = request(&executable);
            assert_eq!(
                exec_support::run_native(&request, &setup, &directory),
                NativeObservation::Errno(libc::EACCES)
            );
            assert_eq!(
                exec_support::run_prepare(&request, &setup, &directory),
                PrepareObservation::Refusal("LookupMountUnverified".into())
            );
        }
    }
}

#[test]
fn lb1_raw_filename_copy_unavailable_refuses_before_lookup() {
    let _serial = SERIAL.lock().unwrap();
    let directory = directory("lb-raw-filename-copy");
    let setup = ChildSetup {
        raw_fault: RawFault::WriteOnlyPath,
        ..ChildSetup::default()
    };
    for (path, check, native) in [
        ("/bin/true", 0, NativeObservation::Exited(0)),
        (
            "/proc/self/status",
            libc::EACCES,
            NativeObservation::Errno(libc::EACCES),
        ),
        (
            "//proc//self///status",
            libc::EACCES,
            NativeObservation::Errno(libc::EACCES),
        ),
    ] {
        let request = request(Path::new(path));
        // On x86-64 getname can read a PROT_WRITE mapping while remote GUP
        // cannot. The helper independently requires process_vm_readv EFAULT.
        assert_eq!(exec_support::run_check(&request, &setup, &directory), check);
        assert_eq!(
            exec_support::run_native(&request, &setup, &directory),
            native
        );
        assert_eq!(
            exec_support::run_prepare(&request, &setup, &directory),
            PrepareObservation::Refusal("PreparationIo".into())
        );
    }
}

fn compare_cwd(directory: &Path, target: &Path, source: &Path) {
    for detached in [false, true] {
        let mount_point = directory.join(if detached { "detached" } else { "listed" });
        fs::create_dir(&mount_point).unwrap();
        let setup = ChildSetup {
            mounted_cwd: Some(MountedCwd {
                mount_point,
                source: source.into(),
                interpreter_name: "interpreter".into(),
                detached,
            }),
            ..ChildSetup::default()
        };
        let request = request(target);
        assert!(request.path.as_bytes().starts_with(b"/"));
        assert_eq!(exec_support::run_check(&request, &setup, directory), 0);
        assert_eq!(
            exec_support::run_native(&request, &setup, directory),
            NativeObservation::Exited(0),
            "native interpreter lookup uses the retained CWD even after detach"
        );
        let prepared = exec_support::run_prepare(&request, &setup, directory);
        if detached {
            assert_eq!(
                prepared,
                PrepareObservation::Refusal("LookupMountUnverified".into())
            );
        } else {
            assert!(
                matches!(prepared, PrepareObservation::Prepared(_)),
                "{prepared:?}"
            );
        }
    }
}

#[test]
fn lb5_relative_pt_interp_qualifies_listed_and_detached_cwd_mounts() {
    let _serial = SERIAL.lock().unwrap();
    let directory = directory("lb-relative-elf");
    let target = with_interpreter(&directory, "T", Path::new("interpreter"));
    compare_cwd(&directory, &target, &native_interpreter());
}

#[test]
fn lb5_relative_script_interpreter_qualifies_listed_and_detached_cwd_mounts() {
    let _serial = SERIAL.lock().unwrap();
    let directory = directory("lb-relative-script");
    let script = directory.join("S");
    executable(&script, b"#!interpreter\n");
    compare_cwd(&directory, &script, Path::new("/bin/true"));

    // A later rewrite must qualify the CWD as well as the first rewrite.
    let nested = directory.join("nested");
    fs::create_dir(&nested).unwrap();
    let outer = nested.join("S");
    executable(&outer, format!("#!{}\n", script.display()).as_bytes());
    compare_cwd(&nested, &outer, Path::new("/bin/true"));
}
