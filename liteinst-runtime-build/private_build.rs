use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::artifact;
use crate::liteinst_artifact;

fn run(root: &Path, name: &str, command: &mut Command) -> io::Result<String> {
    for key in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "LIBRARY_PATH",
        "CPATH",
        "C_INCLUDE_PATH",
        "CPLUS_INCLUDE_PATH",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(key);
    }
    fs::write(
        root.join(format!("{name}.command")),
        format!("{command:?}\n"),
    )?;
    let output = command.output()?;
    fs::write(root.join(format!("{name}.stdout")), &output.stdout)?;
    fs::write(root.join(format!("{name}.stderr")), &output.stderr)?;
    fs::write(
        root.join(format!("{name}.status")),
        format!("{}\n", output.status),
    )?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "private build {name} failed; retained at {}",
            root.display()
        )));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}

fn includes(command: &mut Command, source: &Path) {
    for directory in [
        "bootstrap/src",
        "bootstrap/abi-01",
        "bootstrap/inputs/acquisition",
        "bootstrap/inputs/entry",
        "bootstrap/inputs/builder",
        "crt",
    ] {
        command.arg("-I").arg(source.join(directory));
    }
}

pub fn build(
    hermit: &Path,
    manifest: &Path,
    config: &Option<PathBuf>,
    package: &str,
    profile: &str,
    root: &Path,
) -> io::Result<PathBuf> {
    fs::create_dir(root)?;
    let inputs = liteinst_artifact::private::environment_inputs()?;
    let native = root.join("native");
    fs::create_dir(&native)?;
    for (name, input) in &inputs.files {
        println!("cargo:rerun-if-changed={}", input.display());
        fs::copy(input, native.join(name))?;
        if liteinst_artifact::digest(&fs::read(native.join(name))?)
            != inputs.identity["files"][name]["sha256"]
        {
            return Err(io::Error::other(
                "copied native input differs from verified bytes",
            ));
        }
    }
    for (role, directory) in &inputs.headers {
        println!("cargo:rerun-if-changed={}", directory.display());
        let target = native.join("headers").join(role);
        for relative in inputs.identity["headers"][role].as_object().unwrap().keys() {
            let input = directory.join(relative);
            println!("cargo:rerun-if-changed={}", input.display());
            let output = target.join(relative);
            fs::create_dir_all(output.parent().unwrap())?;
            fs::copy(input, output)?;
        }
        if liteinst_artifact::private::header_tree(&target)? != inputs.identity["headers"][role] {
            return Err(io::Error::other(
                "copied native headers differ from verified bytes",
            ));
        }
    }
    fs::write(
        root.join("native-inputs.json"),
        serde_json::to_vec_pretty(&inputs.identity)?,
    )?;
    let compiler = native.join("cc");
    let source = hermit.join("liteinst-runtime-build/private-native");
    let mut command = crate::cargo("rustc", config);
    command
        .args(["--offline", "--locked", "--manifest-path"])
        .arg(manifest)
        .args([
            "--lib",
            "--no-default-features",
            "--features",
            "private-crt",
            "--crate-type",
            "staticlib",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--profile",
            profile,
            "--message-format=json-render-diagnostics",
            "--target-dir",
        ])
        .arg(root.join("rust-target"))
        .env(
            "RUSTFLAGS",
            "-C target-feature=+crt-static -C relocation-model=pic",
        );
    let messages = run(root, "archive", &mut command)?;
    let archive = artifact::selected_staticlib(&messages, package).map_err(io::Error::other)?;
    let mut objects = Vec::new();
    let units = [
        ("bootstrap/inputs/acquisition", "raw.c"),
        ("bootstrap/inputs/acquisition", "maps.c"),
        ("bootstrap/inputs/acquisition", "image.c"),
        ("bootstrap/inputs/acquisition", "acquire.c"),
        ("bootstrap/inputs/acquisition", "producer.c"),
        ("bootstrap/inputs/entry", "request.c"),
        ("bootstrap/inputs/entry", "entry.S"),
        ("bootstrap/inputs/builder", "private_startup.c"),
        ("bootstrap/src", "map.c"),
        ("bootstrap/src", "stack_input.c"),
        ("bootstrap/src", "binding.c"),
        ("bootstrap/src", "restore_binding.c"),
        ("bootstrap/src", "initial_brk.c"),
        ("bootstrap/src", "prepare.c"),
        ("bootstrap/src", "continue.c"),
        ("lifecycle", "private_tls.c"),
        ("lifecycle", "gnu_provider.c"),
        ("lifecycle", "private_tls.S"),
        ("lifecycle", "initial_transfer.c"),
        ("lifecycle", "initial_transfer.S"),
        ("lifecycle", "context_identity.S"),
        ("crt", "entry.S"),
        ("crt", "enter.S"),
        ("crt", "crt_context.c"),
        ("crt", "bridge.c"),
    ];
    for (index, (directory, unit)) in units.iter().enumerate() {
        let name = format!("unit-{index}");
        let object = root.join(format!("{name}.o"));
        let mut command = Command::new(&compiler);
        command.arg(format!("-B{}/", native.display())).args([
            "-O2",
            "-g",
            "-fPIE",
            "-fvisibility=hidden",
            "-fno-stack-protector",
            "-Wall",
            "-Wextra",
            "-Werror",
        ]);
        if *directory != "crt" {
            command.args([
                "-std=c11",
                "-Wconversion",
                "-Wshadow",
                "-fstack-usage",
                "-ffreestanding",
                "-fno-builtin",
                "-fno-tree-loop-distribute-patterns",
                "-fno-asynchronous-unwind-tables",
                "-fno-unwind-tables",
                "-mno-red-zone",
                "-mgeneral-regs-only",
                "-ffunction-sections",
                "-fdata-sections",
            ]);
        }
        includes(&mut command, &source);
        command.arg("-nostdinc");
        for role in ["gcc", "system"] {
            command
                .arg("-isystem")
                .arg(native.join("headers").join(role));
        }
        command
            .arg("-MD")
            .arg("-MF")
            .arg(root.join(format!("{name}.d")))
            .arg("-c")
            .arg(source.join(directory).join(unit))
            .arg("-o")
            .arg(&object);
        run(root, &name, &mut command)?;
        objects.push(object);
    }
    let output = root.join(liteinst_artifact::private::RUNTIME_NAME);
    let mut command = Command::new(&compiler);
    command
        .arg(format!("-B{}/", native.display()))
        .args([
            "-fuse-ld=bfd",
            "-fno-use-linker-plugin",
            "-nostartfiles",
            "-static-pie",
            "-Wl,-e,pe_kernel_entry",
            "-Wl,--no-gc-sections",
            "-Wl,-z,defs,-z,text,-z,noexecstack",
            "-Wl,-t,-t",
        ])
        .arg(format!("-Wl,-Map,{}", root.join("private.map").display()))
        .arg(native.join("private-rcrt1.o"))
        .arg(native.join("crti.o"))
        .arg(native.join("crtbeginS.o"))
        .args(&objects)
        .arg("-Wl,--whole-archive")
        .arg(&archive)
        .arg("-Wl,--no-whole-archive")
        .arg(format!("-L{}", native.display()))
        .args([
            "-Wl,-Bstatic",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-Wl,--start-group",
            "-lm",
            "-lmvec",
            "-Wl,--end-group",
            "-ldl",
            "-lc",
            "-lgcc_eh",
            "-lgcc",
            "-lc",
        ])
        .arg(native.join("crtendS.o"))
        .arg(native.join("crtn.o"))
        .arg("-o")
        .arg(&output);
    run(root, "link", &mut command)?;
    let bytes = fs::read(&output)?;
    liteinst_artifact::validate_private_runtime(&bytes)?;
    if inputs.identity != liteinst_artifact::private::environment_inputs()?.identity {
        return Err(io::Error::other(
            "private native inputs changed during build",
        ));
    }
    fs::write(
        root.join("artifact.sha256"),
        format!("{}\n", liteinst_artifact::digest(&bytes)),
    )?;
    Ok(output)
}
