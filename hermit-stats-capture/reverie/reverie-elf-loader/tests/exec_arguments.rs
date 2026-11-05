/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! LB3 controls run native execveat in ordinary children. No loader or Hermit
//! executes in this suite. Every fixture and evidence file stays in Cargo's
//! ELF_LOADER_ARTIFACT_DIR below this worktree's target directory.

use std::ffi::CString;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use reverie_elf_loader::Error;
use reverie_elf_loader::ExecRequest;
use reverie_elf_loader::PATH_MAX;
use reverie_elf_loader::arguments::ArgumentError;
use reverie_elf_loader::arguments::ArgumentPages;
use reverie_elf_loader::arguments::ArgumentPlan;
use reverie_elf_loader::arguments::MAX_ARG_STRLEN;
use reverie_elf_loader::arguments::argument_limit;
use reverie_elf_loader::exec::E2bigClassification;
use reverie_elf_loader::pad_launcher_path;

mod exec_support;

use exec_support::ChildSetup;
use exec_support::ExecComparison;
use exec_support::NativeObservation;
use exec_support::PrepareObservation;
use exec_support::PreparedObservation;
use exec_support::exec_request;
use exec_support::native_filename;
use exec_support::run_comparison as native_exec;

// Forked test children can temporarily inherit another thread's executable
// writer, despite CLOEXEC. Serialize fixture construction with child launches.
static EXEC_FIXTURE_GUARD: Mutex<()> = Mutex::new(());

fn cstring(bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap()
}

fn fixture_dir(test: &str) -> PathBuf {
    // Separate cargo invocations must not rewrite a live child's executable,
    // even when separate PID namespaces reuse this process ID.
    exec_support::fixture_dir(test)
}

fn write_script(
    directory: &Path,
    name: &str,
    interpreter: &str,
    optional: Option<&str>,
) -> PathBuf {
    let path = directory.join(name);
    let line = if let Some(argument) = optional {
        format!("#!{interpreter} {argument}\n")
    } else {
        format!("#!{interpreter}\n")
    };
    assert!(
        line.len() <= 256,
        "fixture shebang unexpectedly exceeds BINPRM_BUF_SIZE"
    );
    fs::write(&path, line).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn open_path(path: &Path, directory: bool) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | if directory { libc::O_DIRECTORY } else { 0 })
        .open(path)
        .unwrap()
}

fn set_cloexec(file: &File, enabled: bool) {
    // SAFETY: F_GETFD/F_SETFD take an integer fd/flags, with no pointers.
    let old = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(old, -1);
    let new = if enabled {
        old | libc::FD_CLOEXEC
    } else {
        old & !libc::FD_CLOEXEC
    };
    // SAFETY: the descriptor is retained by file and new preserves other flags.
    assert_eq!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, new) },
        0
    );
}

fn stack_limit() -> u64 {
    let mut value = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: value is writable for exactly the rlimit structure.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_STACK, &mut value) },
        0
    );
    value.rlim_cur
}

fn assert_success(observation: &ExecComparison) -> &PreparedObservation {
    assert_eq!(observation.outcome, NativeObservation::Exited(0));
    match &observation.preparation {
        PrepareObservation::Prepared(prepared) => {
            assert_eq!(prepared.original_check, 0);
            prepared
        }
        other => panic!("native success did not prepare: {other:?}"),
    }
}

fn assert_named_refusal(observation: &ExecComparison, expected: &str) {
    assert_eq!(observation.outcome, NativeObservation::Exited(0));
    match &observation.preparation {
        PrepareObservation::Refusal(name) => assert_eq!(name, expected),
        other => panic!("native success should refuse {expected}: {other:?}"),
    }
}

fn assert_errno(observation: &ExecComparison, errno: i32) {
    assert_eq!(observation.outcome, NativeObservation::Errno(errno));
    match &observation.preparation {
        PrepareObservation::NativeErrno {
            errno: observed, ..
        } => assert_eq!(*observed, errno),
        other => panic!("native errno {errno} did not agree with preparation: {other:?}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn assert_observed_arguments(observation: &ExecComparison, plan: &ArgumentPlan) {
    let prepared = assert_success(observation);
    assert_eq!(
        prepared.argv,
        plan.argv
            .iter()
            .map(|value| value.as_bytes().to_vec())
            .collect::<Vec<_>>()
    );
    assert_eq!(prepared.execfn, plan.execfn.as_bytes());
    let text = fs::read_to_string(&observation.output).unwrap();
    let value = |key: &str| -> String {
        let mut matching = text.lines().filter_map(|line| {
            let (name, contents) = line.split_once('=')?;
            (name == key).then_some(contents)
        });
        let found = matching
            .next()
            .unwrap_or_else(|| panic!("missing fixture key {key}"));
        assert!(matching.next().is_none(), "duplicate fixture key {key}");
        found.to_owned()
    };
    assert_eq!(value("argc"), plan.argv.len().to_string());
    assert_eq!(value("envc"), plan.envp.len().to_string());
    for (index, argument) in plan.argv.iter().enumerate() {
        assert_eq!(
            value(&format!("argv.{index}.hex")),
            hex(argument.as_bytes())
        );
    }
    for (index, environment) in plan.envp.iter().enumerate() {
        assert_eq!(
            value(&format!("env.{index}.hex")),
            hex(environment.as_bytes())
        );
    }
    assert_eq!(value("execfn.hex"), hex(plan.execfn.as_bytes()));
}

#[test]
fn lb_prepare_child() {
    exec_support::prepare_child_entry();
}

#[test]
fn lb3_filename_and_directory_prefix_boundaries() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-path-length");
    let mut setup = ChildSetup::default();
    let target = directory.join("p");
    fs::copy("/bin/true", &target).unwrap();
    let mut bytes = target.as_os_str().as_bytes().to_vec();
    let position = bytes.iter().rposition(|byte| *byte == b'/').unwrap();
    let padding = PATH_MAX - 1 - bytes.len();
    bytes.splice(position..position, std::iter::repeat_n(b'/', padding));
    assert_eq!(bytes.len(), PATH_MAX - 1);
    let mut request = exec_request(Path::new("/bin/true"), vec![cstring(b"p")]);
    request.path = cstring(&bytes);
    let native = native_exec("path-4095", &request, &setup, &directory);
    assert_success(&native);
    let plan = ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &native_filename(&request),
        stack_limit(),
    )
    .unwrap();
    assert_eq!(plan.execfn.as_bytes_with_nul().len(), PATH_MAX);
    assert_eq!(
        pad_launcher_path(Path::new("./h"), PATH_MAX - 1)
            .unwrap()
            .as_bytes_with_nul()
            .len(),
        PATH_MAX
    );
    bytes.insert(position, b'/');
    request.path = cstring(&bytes);
    assert_eq!(request.path.as_bytes().len(), PATH_MAX);
    assert_errno(
        &native_exec("path-4096", &request, &setup, &directory),
        libc::ENAMETOOLONG,
    );

    let dirfd = open_path(&directory, true);
    request.dirfd = dirfd.as_raw_fd();
    let relative = format!(".{}p", "/".repeat(PATH_MAX - 3));
    assert_eq!(relative.len(), PATH_MAX - 1);
    request.path = cstring(relative.as_bytes());
    assert_named_refusal(
        &native_exec("dirfd-path-4095", &request, &setup, &directory),
        "PaddedPathTooLong",
    );
    let filename = native_filename(&request);
    assert!(filename.as_bytes().len() > PATH_MAX - 1);
    assert!(ArgumentPlan::new(&request.argv, &request.envp, &filename, stack_limit()).is_ok());
    assert!(matches!(
        pad_launcher_path(Path::new("./h"), filename.as_bytes().len()),
        Err(Error::PaddedPathTooLong { length_with_nul, maximum: PATH_MAX })
            if length_with_nul == filename.as_bytes_with_nul().len()
    ));
    request.path = cstring(format!(".{}p", "/".repeat(PATH_MAX - 2)).as_bytes());
    assert_errno(
        &native_exec("dirfd-path-4096", &request, &setup, &directory),
        libc::ENAMETOOLONG,
    );

    request.dirfd = libc::AT_FDCWD;
    request.path = cstring(b"p");
    setup.cwd = Some(directory.clone());
    assert_named_refusal(
        &native_exec("short-F", &request, &setup, &directory),
        "LauncherPathTooLong",
    );
    assert!(matches!(
        pad_launcher_path(Path::new("./h"), native_filename(&request).as_bytes().len()),
        Err(Error::LauncherPathTooLong {
            filename_length: 1,
            launcher_length: 3
        })
    ));
}

#[test]
fn lb3_optional_nested_scripts_and_empty_argv_observed() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-rewritten-argv");
    let mut setup = ChildSetup::default();
    let second = write_script(
        &directory,
        "s2",
        env!("ELF_LOADER_LAYOUT_NONPIE"),
        Some("second"),
    );
    let first = write_script(
        &directory,
        "s1",
        "./s2",
        Some("one argument with spaces\t  "),
    );
    let mut request = exec_request(&first, vec![cstring(b"caller-zero"), cstring(b"tail")]);
    setup.cwd = Some(directory.clone());
    request.envp = vec![cstring(b"CHECK=VALUE")];
    let mut plan = ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &native_filename(&request),
        stack_limit(),
    )
    .unwrap();
    plan.rewrite_script(
        c"./s2",
        Some(c"one argument with spaces"),
        &native_filename(&request),
    )
    .unwrap();
    plan.rewrite_script(
        &cstring(env!("ELF_LOADER_LAYOUT_NONPIE").as_bytes()),
        Some(c"second"),
        c"./s2",
    )
    .unwrap();
    assert_eq!(plan.argv.len(), 6);
    assert_observed_arguments(
        &native_exec("nested-optional", &request, &setup, &directory),
        &plan,
    );
    assert!(second.is_file());

    let target = Path::new(env!("ELF_LOADER_LAYOUT_NONPIE"));
    let request = exec_request(target, Vec::new());
    setup.cwd = None;
    let plan = ArgumentPlan::new(&[], &[], &native_filename(&request), stack_limit()).unwrap();
    assert_eq!(plan.argv, [CString::default()]);
    assert_observed_arguments(
        &native_exec("empty-argv-elf", &request, &setup, &directory),
        &plan,
    );

    let request = exec_request(&first, Vec::new());
    setup.cwd = Some(directory.clone());
    let mut plan = ArgumentPlan::new(&[], &[], &native_filename(&request), stack_limit()).unwrap();
    plan.rewrite_script(
        c"./s2",
        Some(c"one argument with spaces"),
        &native_filename(&request),
    )
    .unwrap();
    plan.rewrite_script(
        &cstring(env!("ELF_LOADER_LAYOUT_NONPIE").as_bytes()),
        Some(c"second"),
        c"./s2",
    )
    .unwrap();
    assert_eq!(plan.argv.len(), 5);
    assert_observed_arguments(
        &native_exec("empty-argv-script", &request, &setup, &directory),
        &plan,
    );
}

#[test]
fn lb3_script_depth_five_six_and_missing_interpreter_order() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-script-depth");
    let mut setup = ChildSetup::default();
    for index in 0..6 {
        let interpreter = if index == 5 {
            "/bin/true".to_owned()
        } else {
            format!("./s{}", index + 1)
        };
        write_script(&directory, &format!("s{index}"), &interpreter, None);
    }
    let mut request = exec_request(&directory.join("s1"), vec![cstring(b"ignored")]);
    setup.cwd = Some(directory.clone());
    let native = native_exec("five-script-rewrites", &request, &setup, &directory);
    assert_eq!(assert_success(&native).scripts, 5);
    request.path = cstring(directory.join("s0").as_os_str().as_bytes());
    assert_errno(
        &native_exec("six-script-rewrites", &request, &setup, &directory),
        libc::ELOOP,
    );
    write_script(&directory, "s5", "./definitely-missing", None);
    assert_errno(
        &native_exec("missing-sixth-interpreter", &request, &setup, &directory),
        libc::ENOENT,
    );
}

#[test]
fn lb3_script_buffer_last_byte_terminator() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-script-buffer-end");
    let setup = ChildSetup::default();
    let path = directory.join("s");
    let interpreter = format!("{}bin/true", "/".repeat(245));
    assert_eq!(interpreter.len(), 253);
    let mut contents = format!("#!{interpreter}").into_bytes();
    assert_eq!(contents.len(), 255);
    fs::write(&path, &contents).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let request = exec_request(&path, vec![cstring(b"zero")]);
    // prepare_binprm zero-fills byte 255 after the 255-byte file read. The
    // native next_terminator scan includes that last buffer byte.
    assert_success(&native_exec(
        "nul-at-byte-255",
        &request,
        &setup,
        &directory,
    ));
    let mut plan = ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &native_filename(&request),
        stack_limit(),
    )
    .unwrap();
    plan.rewrite_script(
        &cstring(interpreter.as_bytes()),
        None,
        &native_filename(&request),
    )
    .unwrap();
    assert!(plan.launcher_budget(&[], &[]).is_ok());
    contents.push(b' ');
    fs::write(&path, &contents).unwrap();
    assert_success(&native_exec(
        "space-at-byte-255",
        &request,
        &setup,
        &directory,
    ));
    *contents.last_mut().unwrap() = b'x';
    fs::write(&path, &contents).unwrap();
    // Without a terminator anywhere in the 256-byte header, parsing must fail
    // before trying to open the now-missing interpreter pathname ending in x.
    assert_errno(
        &native_exec("no-terminator-in-header", &request, &setup, &directory),
        libc::ENOEXEC,
    );
}

fn fill_argument_strings(count: usize, total: usize) -> Vec<CString> {
    assert!(count != 0 && total >= count);
    let quotient = total / count;
    let remainder = total % count;
    (0..count)
        .map(|index| {
            let bytes = quotient + usize::from(index < remainder);
            assert!((1..=MAX_ARG_STRLEN).contains(&bytes));
            cstring(&vec![b'x'; bytes - 1])
        })
        .collect()
}

fn script_rewrites(plan: &mut ArgumentPlan, nested: bool) -> Result<(), ArgumentError> {
    if nested {
        let original_f = plan.execfn.clone();
        plan.rewrite_script(c"./s1", Some(c"first optional argument"), &original_f)?;
        plan.rewrite_script(c"/bin/true", Some(c"second optional argument"), c"./s1")
    } else {
        let original_f = plan.execfn.clone();
        plan.rewrite_script(c"/bin/true", Some(c"optional argument"), &original_f)
    }
}

fn native_argument_model_agrees(
    native: &ExecComparison,
    model: &Result<(), ArgumentError>,
) -> bool {
    match model {
        Ok(()) => native.outcome == NativeObservation::Exited(0),
        Err(ArgumentError::NativeE2big) => native.outcome == NativeObservation::Errno(libc::E2BIG),
        Err(ArgumentError::LauncherArgumentBudget) => false,
    }
}

fn script_budget_control(nested: bool) {
    let mut setup = ChildSetup::default();
    let directory = fixture_dir(if nested {
        "lb3-nested-script-budget"
    } else {
        "lb3-script-budget"
    });
    if nested {
        write_script(
            &directory,
            "s1",
            "/bin/true",
            Some("second optional argument"),
        );
        write_script(&directory, "s0", "./s1", Some("first optional argument"));
    } else {
        write_script(&directory, "s0", "/bin/true", Some("optional argument"));
    }
    let mut request = exec_request(Path::new("./s0"), vec![cstring(b"./s0")]);
    setup.cwd = Some(directory.clone());
    request.envp = vec![cstring(b"E=A"), cstring(b"F=B"), cstring(b"G=C")];
    let stack = stack_limit();
    let base = ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &native_filename(&request),
        stack,
    )
    .unwrap();
    let mut rewritten_base = base.clone();
    script_rewrites(&mut rewritten_base, nested).unwrap();
    let added_string_bytes = rewritten_base.string_bytes - base.string_bytes;
    // 66 argv entries ensure every individual string stays below 128 KiB even
    // at the maximum 6 MiB total band. The original environment also consumes
    // pointers; it must not disappear from either allowance calculation.
    let original_argc = 66;
    let pointer_bytes = 8 * (original_argc + request.envp.len()) as u64;
    let initial_string_bytes = argument_limit(stack) - pointer_bytes - added_string_bytes;
    let tail_bytes = initial_string_bytes - base.string_bytes;
    request.argv.extend(fill_argument_strings(
        original_argc - 1,
        tail_bytes as usize,
    ));
    let mut plan = ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &native_filename(&request),
        stack,
    )
    .unwrap();
    script_rewrites(&mut plan, nested).unwrap();
    assert_eq!(plan.native_pointer_bytes, pointer_bytes);
    assert_eq!(plan.string_bytes, plan.native_string_limit);
    let native = native_exec("exact-native-band", &request, &setup, &directory);
    assert!(native_argument_model_agrees(&native, &Ok(())));
    assert_named_refusal(&native, "LauncherArgumentBudget");
    assert_eq!(
        plan.launcher_budget(&[], &[]),
        Err(ArgumentError::LauncherArgumentBudget)
    );

    // Mutation: using rewritten argc in the native allowance predicts E2BIG
    // for this observed native success. The same outcome comparator rejects it.
    let wrong_pointer_bytes = 8 * (plan.argv.len().max(1) + plan.envp.len()) as u64;
    assert!(wrong_pointer_bytes > plan.native_pointer_bytes);
    let wrong_pointer_model = if plan.string_bytes <= plan.native_limit - wrong_pointer_bytes {
        Ok(())
    } else {
        Err(ArgumentError::NativeE2big)
    };
    assert_eq!(wrong_pointer_model, Err(ArgumentError::NativeE2big));
    assert!(!native_argument_model_agrees(&native, &wrong_pointer_model));
    fs::write(
        directory.join("wrong-pointer-mutation.result"),
        "REJECTED: rewritten argc falsely predicts native E2BIG\n",
    )
    .unwrap();

    let last = request.argv.last_mut().unwrap();
    let mut oversized_tail = last.as_bytes().to_vec();
    oversized_tail.push(b'x');
    *last = cstring(&oversized_tail);
    let mut plan = ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &native_filename(&request),
        stack,
    )
    .unwrap();
    let rewrite = script_rewrites(&mut plan, nested);
    assert_eq!(rewrite, Err(ArgumentError::NativeE2big));
    let native = native_exec("native-band-plus-one", &request, &setup, &directory);
    assert!(native_argument_model_agrees(&native, &rewrite));
    assert_errno(&native, libc::E2BIG);

    // Qualifying companion: free exactly the extra rewritten-pointer bytes.
    let extra_pointer_bytes = 8 * (rewritten_base.argv.len() - base.argv.len()) as u64;
    let final_tail = request.argv.last_mut().unwrap();
    let shorter = final_tail.as_bytes()
        [..final_tail.as_bytes().len() - extra_pointer_bytes as usize - 1]
        .to_vec();
    *final_tail = cstring(&shorter);
    let mut admitted = ArgumentPlan::new(
        &request.argv,
        &request.envp,
        &native_filename(&request),
        stack,
    )
    .unwrap();
    script_rewrites(&mut admitted, nested).unwrap();
    let launcher = admitted.launcher_budget(&[], &[]).unwrap();
    assert_eq!(launcher.string_bytes, launcher.string_limit);
    assert_success(&native_exec(
        "rewritten-launcher-band",
        &request,
        &setup,
        &directory,
    ));
}

#[test]
fn lb3_native_and_launcher_script_argument_budgets() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    script_budget_control(false);
    script_budget_control(true);
}

#[test]
fn lb3_native_single_argument_length_boundary() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-one-argument-length");
    let setup = ChildSetup::default();
    for (payload_length, expected) in [
        (MAX_ARG_STRLEN - 1, NativeObservation::Exited(0)),
        (MAX_ARG_STRLEN, NativeObservation::Errno(libc::E2BIG)),
    ] {
        let request = exec_request(
            Path::new("/bin/true"),
            vec![cstring(b"/bin/true"), cstring(&vec![b'x'; payload_length])],
        );
        let plan = ArgumentPlan::new(
            &request.argv,
            &request.envp,
            &native_filename(&request),
            stack_limit(),
        )
        .map(|_| ());
        let native = native_exec(
            &format!("one-argument-{payload_length}"),
            &request,
            &setup,
            &directory,
        );
        assert_eq!(native.outcome, expected);
        assert!(native_argument_model_agrees(&native, &plan));
        match expected {
            NativeObservation::Exited(0) => {
                assert_success(&native);
            }
            NativeObservation::Errno(errno) => assert_errno(&native, errno),
            other => panic!("unexpected single-string control expectation: {other:?}"),
        }
    }
}

#[test]
fn lb3_native_e2big_with_both_size_models_passing() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-both-models-pass-e2big");
    let stack_limit = 64 * 1024;
    let request = ExecRequest {
        dirfd: libc::AT_FDCWD,
        path: cstring(b"/bin/true"),
        argv: vec![cstring(b"true"), cstring(&vec![b'x'; 96 * 1024])],
        envp: Vec::new(),
        flags: 0,
    };
    let plan = ArgumentPlan::new(&request.argv, &request.envp, &request.path, stack_limit).unwrap();
    assert_eq!(plan.native_limit, 128 * 1024);
    assert!(plan.string_bytes < plan.native_string_limit);
    assert!(plan.launcher_budget(&[], &[]).is_ok());
    assert_eq!(
        ArgumentPages::new(&request.argv, &request.envp, &request.path, stack_limit).unwrap_err(),
        ArgumentError::NativeE2big
    );
    // exec.c's ARG_MAX floor admits these strings. get_arg_page still has to
    // grow the nascent stack: acct_stack_growth in mm/vma.c rejects growth
    // beyond RLIMIT_STACK, and copy_strings returns the native E2BIG.
    let setup = exec_support::ChildSetup {
        stack_limit: Some(stack_limit),
        ..exec_support::ChildSetup::default()
    };
    let native = exec_support::run_native(&request, &setup, &directory);
    assert_eq!(native, exec_support::NativeObservation::Errno(libc::E2BIG));
    assert_eq!(
        exec_support::run_check(&request, &setup, &directory),
        libc::E2BIG
    );
    let preparation = exec_support::run_prepare(&request, &setup, &directory);
    assert_eq!(
        preparation,
        PrepareObservation::NativeErrno {
            errno: libc::E2BIG,
            original_check: true,
            e2big: Some(E2bigClassification::BothModelsPass),
        }
    );
    fs::write(
        directory.join("both-models-pass.result"),
        format!(
            "native={native:?}\npreparation={preparation:?}\nstack_limit={stack_limit}\nargument_plan={plan:?}\n"
        ),
    )
    .unwrap();
}

#[test]
fn lb3_original_check_fits_but_script_needs_another_argument_page() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-script-page-growth");
    let script = write_script(&directory, "s", "/bin/true", None);
    let filename = cstring(format!(".{}s", "/".repeat(197)).as_bytes());
    assert_eq!(filename.as_bytes_with_nul().len(), 200);
    let request = ExecRequest {
        dirfd: libc::AT_FDCWD,
        path: filename.clone(),
        argv: vec![cstring(b"a"), cstring(&vec![b'x'; 65325])],
        envp: Vec::new(),
        flags: 0,
    };
    let mut setup = ChildSetup {
        cwd: Some(directory.clone()),
        stack_limit: Some(65536),
        ..ChildSetup::default()
    };
    let mut byte_plan = ArgumentPlan::new(&request.argv, &[], &filename, 65536).unwrap();
    let mut pages = ArgumentPages::new(&request.argv, &[], &filename, 65536).unwrap();
    assert_eq!(pages.string_bytes(), 65528);
    assert_eq!(pages.pages(), 16);
    assert_eq!(exec_support::run_check(&request, &setup, &directory), 0);
    assert_eq!(
        pages.rewrite_script(Some(c"a"), c"/bin/true", None, &filename),
        Err(ArgumentError::NativeE2big)
    );
    byte_plan
        .rewrite_script(c"/bin/true", None, &filename)
        .unwrap();
    assert!(byte_plan.launcher_budget(&[], &[]).is_ok());
    let native = native_exec("rewrite-needs-page-17", &request, &setup, &directory);
    assert_errno(&native, libc::E2BIG);
    assert_eq!(
        native.preparation,
        PrepareObservation::NativeErrno {
            errno: libc::E2BIG,
            original_check: false,
            e2big: Some(E2bigClassification::NativeBudget),
        }
    );
    // Mutation: keeping only the preexisting byte allowance would predict
    // success; the unchanged native comparator rejects that page omission.
    assert!(!native_argument_model_agrees(&native, &Ok(())));
    fs::write(
        directory.join("omitted-page-accounting.result"),
        "REJECTED: original CHECK fits but script copy needs page 17 before interpreter lookup\n",
    )
    .unwrap();

    // With enough stack, the exact same request and script rewrite succeeds.
    setup.stack_limit = Some(8 * 1024 * 1024);
    assert_success(&native_exec(
        "rewrite-page-growth-admitted",
        &request,
        &setup,
        &directory,
    ));

    // The failing copy precedes interpreter lookup, including ENOENT there.
    write_script(&directory, "s", "./missing-interpreter", None);
    setup.stack_limit = Some(65536);
    assert_eq!(exec_support::run_check(&request, &setup, &directory), 0);
    assert_errno(
        &native_exec(
            "page-growth-before-missing-interpreter",
            &request,
            &setup,
            &directory,
        ),
        libc::E2BIG,
    );
    setup.stack_limit = Some(8 * 1024 * 1024);
    assert_errno(
        &native_exec(
            "missing-interpreter-after-admitted-growth",
            &request,
            &setup,
            &directory,
        ),
        libc::ENOENT,
    );
    assert!(script.is_file());
}

#[test]
fn lb3_cloexec_synthesized_script_path_and_literal_procfd() {
    let _fixture_guard = EXEC_FIXTURE_GUARD.lock().unwrap();
    let directory = fixture_dir("lb3-cloexec-script");
    let mut setup = ChildSetup::default();
    let script = write_script(&directory, "s", "/bin/true", None);
    let file = open_path(&script, false);
    set_cloexec(&file, true);
    let mut request = exec_request(Path::new(""), vec![cstring(b"ignored")]);
    request.dirfd = file.as_raw_fd();
    request.flags = libc::AT_EMPTY_PATH;
    setup.inherited_fds.push(file.as_raw_fd());
    assert_errno(
        &native_exec("cloexec-empty-script", &request, &setup, &directory),
        libc::ENOENT,
    );
    assert_eq!(
        native_filename(&request).as_bytes(),
        format!("/dev/fd/{}", file.as_raw_fd()).as_bytes()
    );

    let mut literal = request.clone();
    literal.dirfd = libc::AT_FDCWD;
    literal.path = cstring(format!("/proc/self/fd/{}", file.as_raw_fd()).as_bytes());
    literal.flags = 0;
    assert_success(&native_exec(
        "literal-procfd-script",
        &literal,
        &setup,
        &directory,
    ));
    set_cloexec(&file, false);
    assert_success(&native_exec(
        "noncloexec-empty-script",
        &request,
        &setup,
        &directory,
    ));

    let elf = open_path(Path::new("/bin/true"), false);
    set_cloexec(&elf, true);
    request.dirfd = elf.as_raw_fd();
    assert_success(&native_exec(
        "cloexec-empty-elf",
        &request,
        &setup,
        &directory,
    ));

    let dirfd = open_path(&directory, true);
    set_cloexec(&dirfd, true);
    request.dirfd = dirfd.as_raw_fd();
    request.path = cstring(b"s");
    request.flags = 0;
    setup.inherited_fds.push(dirfd.as_raw_fd());
    assert_errno(
        &native_exec("cloexec-relative-script", &request, &setup, &directory),
        libc::ENOENT,
    );
    literal.path = cstring(format!("/proc/self/fd/{}/s", dirfd.as_raw_fd()).as_bytes());
    assert_success(&native_exec(
        "literal-procfd-relative-script",
        &literal,
        &setup,
        &directory,
    ));
    set_cloexec(&dirfd, false);
    assert_success(&native_exec(
        "noncloexec-relative-script",
        &request,
        &setup,
        &directory,
    ));
}
