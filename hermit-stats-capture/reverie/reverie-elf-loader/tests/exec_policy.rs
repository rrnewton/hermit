/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! LB5/LB6 native companions for explicitly MODELED host-policy inputs.
//!
//! Every pair executes its exact request in an ordinary bounded native child.
//! A separate ordinary child applies the modeled policy to the inactive public
//! preparation API. Model receipts do not establish this machine's privileged
//! LSM/BPF/integrity, binfmt, mount, capability or watch policy for activation.
//! Authority failures precede the first target lookup; content-dependent binfmt
//! handler failures occur only after reading the classified pinned object.

mod exec_support;

use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use exec_support::ChildSetup;
use exec_support::NativeObservation;
use reverie_elf_loader::ExecCheckOutcome;
use reverie_elf_loader::ExecRefusal;
use reverie_elf_loader::ExecRequest;
use reverie_elf_loader::Limits;
use reverie_elf_loader::LoaderHostFacts;
use reverie_elf_loader::PrepareExecOptions;
use reverie_elf_loader::host::BinfmtAuthority;
use reverie_elf_loader::host::BinfmtEntry;
use reverie_elf_loader::host::BinfmtFlags;
use reverie_elf_loader::host::BinfmtMatch;
use reverie_elf_loader::host::BinfmtRegistry;
use reverie_elf_loader::host::BpfAttestation;
use reverie_elf_loader::host::BpfEvidence;
use reverie_elf_loader::host::EvidenceOrigin;
use reverie_elf_loader::host::ExecContextEvidence;
use reverie_elf_loader::host::ExecSecurityHook;
use reverie_elf_loader::host::FrozenHostAttestation;
use reverie_elf_loader::host::HostEvidence;
use reverie_elf_loader::host::HostPolicyRefusal;
use reverie_elf_loader::host::HostQualification;
use reverie_elf_loader::host::InheritedLookupFd;
use reverie_elf_loader::host::IntegrityEvidence;
use reverie_elf_loader::host::MountEvidence;
use reverie_elf_loader::host::NamespaceBinfmtEvidence;
use reverie_elf_loader::host::PreContentWatchEvidence;
use reverie_elf_loader::prepare_exec;

const POLICY_SPEC: &str = "REVERIE_LB_MODELED_POLICY_SPEC";
static NEXT_CASE: AtomicU64 = AtomicU64::new(0);
const HOOKS: [ExecSecurityHook; 8] = [
    ExecSecurityHook::FileOpen,
    ExecSecurityHook::FilePermission,
    ExecSecurityHook::BprmCredsForExec,
    ExecSecurityHook::BprmCheck,
    ExecSecurityHook::BprmCredsFromFile,
    ExecSecurityHook::BprmCommittingCreds,
    ExecSecurityHook::BprmCommittedCreds,
    ExecSecurityHook::MmapFile,
];

#[test]
fn lb_prepare_child() {
    exec_support::prepare_child_entry();
}

fn request(path: impl AsRef<Path>) -> ExecRequest {
    ExecRequest::execve(
        path.as_ref(),
        vec![
            CString::new("policy-native-argv0").unwrap(),
            CString::new("same argument").unwrap(),
        ],
        vec![CString::new("LB_POLICY_COMPANION=exact-original-env").unwrap()],
    )
    .unwrap()
}

fn fixture_dir() -> PathBuf {
    exec_support::fixture_dir("lb5-lb6-policy-native-companions")
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend((bytes.len() as u64).to_le_bytes());
    output.extend(bytes);
}

struct Decoder<'a>(&'a [u8]);

impl Decoder<'_> {
    fn i32(&mut self) -> i32 {
        let value = i32::from_le_bytes(self.0[..4].try_into().unwrap());
        self.0 = &self.0[4..];
        value
    }

    fn size(&mut self) -> usize {
        let value = u64::from_le_bytes(self.0[..8].try_into().unwrap()) as usize;
        self.0 = &self.0[8..];
        value
    }

    fn bytes(&mut self) -> Vec<u8> {
        let size = self.size();
        let value = self.0[..size].to_vec();
        self.0 = &self.0[size..];
        value
    }

    fn strings(&mut self) -> Vec<CString> {
        let count = self.size();
        (0..count)
            .map(|_| CString::new(self.bytes()).unwrap())
            .collect()
    }
}

fn model_observation(case: &str, request: &ExecRequest, directory: &Path) -> String {
    let sequence = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
    let stem = format!("model-policy-{}-{sequence}-{case}", std::process::id());
    let spec_path = directory.join(format!("{stem}.spec"));
    let result_path = directory.join(format!("{stem}.result"));
    let output_path = directory.join(format!("{stem}.output"));
    let mut spec = Vec::new();
    put_bytes(&mut spec, case.as_bytes());
    put_bytes(&mut spec, result_path.as_os_str().as_bytes());
    spec.extend(request.dirfd.to_le_bytes());
    spec.extend(request.flags.to_le_bytes());
    let original_fd_flags = if request.dirfd == libc::AT_FDCWD {
        -1
    } else {
        // SAFETY: this test owns the inherited lookup descriptor throughout.
        let flags = unsafe { libc::fcntl(request.dirfd, libc::F_GETFD) };
        assert!(flags >= 0);
        flags
    };
    spec.extend(original_fd_flags.to_le_bytes());
    put_bytes(&mut spec, request.path.to_bytes());
    for strings in [&request.argv, &request.envp] {
        spec.extend((strings.len() as u64).to_le_bytes());
        for string in strings {
            put_bytes(&mut spec, string.to_bytes());
        }
    }
    fs::write(&spec_path, spec).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "lb_policy_prepare_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(POLICY_SPEC, spec_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&output_path).unwrap()))
        .stderr(Stdio::from(
            File::create(output_path.with_extension("stderr")).unwrap(),
        ));
    if request.dirfd != libc::AT_FDCWD {
        let fd = request.dirfd;
        // SAFETY: this fixture owns fd throughout spawning; pre_exec only
        // clears CLOEXEC with async-signal-safe descriptor operations.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let status =
        exec_support::run_monitored(command, &format!("modeled policy {case}, {output_path:?}"));
    assert!(status.success(), "{case}: {status}, {output_path:?}");
    fs::read_to_string(result_path).unwrap()
}

#[derive(Clone, Copy)]
enum Expected {
    BeforeLookup(&'static str),
    PinnedStage(&'static str),
    Prepared(usize),
}

fn pair(case: &str, request: &ExecRequest, expected: Expected) -> String {
    let directory = fixture_dir();
    assert_eq!(
        exec_support::run_native(request, &ChildSetup::default(), &directory),
        NativeObservation::Exited(0),
        "real ordinary native companion for {case}"
    );
    let modeled = model_observation(case, request, &directory);
    let first = modeled.lines().next().unwrap();
    match expected {
        Expected::BeforeLookup(name) => {
            assert_eq!(first, format!("before-lookup {name}"), "{case}: {modeled}")
        }
        Expected::PinnedStage(name) => {
            assert_eq!(first, format!("pinned-stage {name}"), "{case}: {modeled}")
        }
        Expected::Prepared(scripts) => assert_eq!(
            first,
            format!("prepared scripts={scripts} original-check=0"),
            "{case}: {modeled}"
        ),
    }
    modeled
}

fn report(name: &str, details: &str) {
    fs::write(
        fixture_dir().join(format!("{name}.result")),
        format!("PASS ordinary native / MODELED inactive preparation companions\n{details}\nMISSING ACTIVATION GATES: privileged live LSM/BPF/appraisal, watch, binfmt/userns, detached mounts, namespace-init/idmap and capability transition fixtures. No modeled receipt qualifies this machine for activation.\n"),
    ).unwrap();
}

fn host_from_model(
    evidence: HostEvidence,
    attestation: FrozenHostAttestation,
) -> Result<HostQualification, HostPolicyRefusal> {
    // Reconstructing changed policy evidence must retain its real bootstrap
    // descriptors. A fresh fixture has the same genuine proc task/namespace
    // identities; only the declared modeled policy is changed by this test.
    let baseline = exec_support::modeled_host(None);
    let endpoints = baseline
        .retained_proc_endpoints()
        .expect("modeled fixture retains qualified proc endpoints");
    // SAFETY: this function is confined to a labeled inactive test model. Its
    // receipt is never represented as complete genuine live host authority.
    let host = unsafe { HostQualification::from_frozen_evidence(evidence, attestation) }?
        .with_proc_endpoints(endpoints)?;
    let roots = baseline
        .retained_lookup_roots()
        .expect("modeled fixture retains qualified admitted lookup roots");
    // SAFETY: same labeled inactive fixture with the same real mount roots.
    unsafe { host.with_retained_lookup_roots(roots) }
}

fn escaped_mount_point(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| match *byte {
            b' ' => "\\040".to_owned(),
            b'\t' => "\\011".to_owned(),
            b'\n' => "\\012".to_owned(),
            b'\\' => "\\134".to_owned(),
            other => char::from(other).to_string(),
        })
        .collect()
}

fn change_mounts(evidence: &mut HostEvidence, filesystem: Option<&str>, detached_fd: Option<i32>) {
    let mut text = String::new();
    for mount in evidence.mounts.records() {
        text.push_str(&format!(
            "{} {} 0:1 / {} rw - {} modeled rw\n",
            mount.mount_id,
            mount.parent_id,
            escaped_mount_point(&mount.mount_point),
            mount.filesystem
        ));
    }
    let extra = evidence
        .mounts
        .records()
        .iter()
        .map(|mount| mount.mount_id)
        .max()
        .unwrap()
        + 1;
    if let Some(filesystem) = filesystem {
        text.push_str(&format!(
            "{extra} 0 0:2 / /MODELED-elsewhere rw - {filesystem} modeled rw\n"
        ));
    }
    let inherited = detached_fd.map_or_else(Vec::new, |fd| {
        vec![InheritedLookupFd {
            fd,
            mount_id: extra,
        }]
    });
    let proc = evidence.mounts.proc_namespaces().to_vec();
    evidence.mounts = MountEvidence::parse_complete(text.as_bytes(), inherited)
        .unwrap()
        .with_proc_namespaces(proc);
}

fn current_registry(evidence: &mut HostEvidence) -> &mut BinfmtRegistry {
    match &mut evidence.binfmt {
        BinfmtAuthority::NearestAncestor { ancestry, .. } => ancestry[0].registry.as_mut().unwrap(),
        BinfmtAuthority::Unverified { .. } => unreachable!(),
    }
}

fn handler(name: &str, flags: &str, rule: BinfmtMatch) -> BinfmtEntry {
    BinfmtEntry {
        name: format!("MODELED-{name}"),
        enabled: true,
        rule,
        interpreter: b"/MODELED-never-invoked-binfmt-handler".to_vec(),
        flags: BinfmtFlags::parse(flags).unwrap(),
    }
}

fn elf_handler(name: &str, flags: &str) -> BinfmtEntry {
    handler(
        name,
        flags,
        BinfmtMatch::Magic {
            offset: 0,
            magic: b"\x7fELF".to_vec(),
            mask: None,
        },
    )
}

fn install_bpf(evidence: &mut HostEvidence, attestation: &FrozenHostAttestation) {
    evidence
        .security
        .active_lsms
        .as_mut()
        .unwrap()
        .push("bpf".into());
    let mut receipt = exec_support::model_receipt();
    receipt.scope = "MODEL ONLY: all eight BPF exec/open/read/mmap hooks, actual caller namespaces and cgroup subject, native FMODE_EXEC versus launcher pinned READ and security_file_permission equivalence".into();
    receipt.lifetime = "MODEL ONLY: no attachment, map, cgroup, credential, policy or executable-content changes during inactive preparation".into();
    evidence.security.bpf = BpfEvidence::Attested(BpfAttestation {
        receipt,
        context_digest: attestation.context_digest,
        covered_hooks: HOOKS.to_vec(),
    });
}

fn qualification(
    case: &str,
    request: &ExecRequest,
) -> Result<HostQualification, HostPolicyRefusal> {
    if case == "live-default-qualification" {
        return HostQualification::collect_current();
    }
    let base = exec_support::modeled_host(None);
    let mut evidence = base.evidence().clone();
    let mut attestation = base.attestation().clone();
    attestation.origin = EvidenceOrigin::Modeled {
        fixture: format!(
            "LB5/LB6 controlled {case} policy with an ordinary native request companion"
        ),
    };
    attestation.receipt.scope = format!(
        "MODEL ONLY: {case} frozen host evidence, including native exec versus pinned ordinary READ subjects and all exec/open/file_permission/mmap hooks; never genuine live policy authority"
    );
    if let Some(filesystem) = case
        .strip_prefix("mount-safe-")
        .or_else(|| case.strip_prefix("mount-unsafe-"))
    {
        change_mounts(&mut evidence, Some(filesystem), None);
    } else if let Some(module) = case.strip_prefix("lsm-unknown-") {
        evidence
            .security
            .active_lsms
            .as_mut()
            .unwrap()
            .push(module.into());
    } else if let Some(hook) = case.strip_prefix("bpf-missing-hook-") {
        install_bpf(&mut evidence, &attestation);
        if let BpfEvidence::Attested(proof) = &mut evidence.security.bpf {
            proof.covered_hooks.remove(hook.parse::<usize>().unwrap());
        }
    } else {
        match case {
            "baseline" => {}
            "detached-dirfd-model" => change_mounts(&mut evidence, None, Some(request.dirfd)),
            "lsm-unreadable" => evidence.security.active_lsms = None,
            "lsm-empty" => evidence.security.active_lsms = Some(Vec::new()),
            "lsm-duplicate" => evidence
                .security
                .active_lsms
                .as_mut()
                .unwrap()
                .push("capability".into()),
            "integrity-unreadable"
            | "integrity-bprm-appraisal"
            | "integrity-ima-unattested"
            | "integrity-measurement-only"
            | "integrity-measurement-missing-receipt" => {
                evidence
                    .security
                    .active_lsms
                    .as_mut()
                    .unwrap()
                    .push("ima".into());
                evidence.security.integrity = match case {
                    "integrity-unreadable" => IntegrityEvidence::Unknown {
                        reason: "MODELED unreadable privileged policy".into(),
                    },
                    "integrity-bprm-appraisal" => IntegrityEvidence::EnforcingAppraisal {
                        policy: "MODELED BPRM-only appraisal; checked content subsequently changes"
                            .into(),
                    },
                    "integrity-ima-unattested" => IntegrityEvidence::Inactive,
                    _ => {
                        let mut receipt = exec_support::model_receipt();
                        if case == "integrity-measurement-missing-receipt" {
                            receipt.lifetime.clear();
                        }
                        IntegrityEvidence::MeasurementOnly { receipt }
                    }
                };
            }
            "bpf-unattested" | "bpf-active-inactive-evidence" => {
                evidence
                    .security
                    .active_lsms
                    .as_mut()
                    .unwrap()
                    .push("bpf".into());
                evidence.security.bpf = if case == "bpf-unattested" {
                    BpfEvidence::Unattested
                } else {
                    BpfEvidence::Inactive
                };
            }
            "bpf-complete-model"
            | "bpf-context-digest-mismatch"
            | "bpf-receipt-missing"
            | "bpf-policy-digest-changed"
            | "bpf-generation-changed"
            | "bpf-hook-scope-changed" => {
                install_bpf(&mut evidence, &attestation);
                if case.ends_with("changed") {
                    let approved = host_from_model(evidence.clone(), attestation)?;
                    if let BpfEvidence::Attested(proof) = &mut evidence.security.bpf {
                        match case {
                            "bpf-policy-digest-changed" => proof.receipt.policy_digest[0] ^= 1,
                            "bpf-generation-changed" => proof.receipt.generation += 1,
                            _ => {
                                proof.covered_hooks.pop();
                            }
                        }
                    }
                    approved.revalidate_evidence(&evidence)?;
                    return Ok(approved);
                }
                if let BpfEvidence::Attested(proof) = &mut evidence.security.bpf {
                    if case == "bpf-context-digest-mismatch" {
                        proof.context_digest[0] ^= 1;
                    }
                    if case == "bpf-receipt-missing" {
                        proof.receipt.approval_id.clear();
                    }
                }
            }
            "binfmt-unreadable" => {
                evidence.binfmt = BinfmtAuthority::Unverified {
                    reason: "MODELED hidden or unreadable authority".into(),
                }
            }
            "binfmt-visible-mount-mismatch" | "binfmt-visible-digest-mismatch" => {
                if let BinfmtAuthority::NearestAncestor {
                    visible_registry, ..
                } = &mut evidence.binfmt
                {
                    if case == "binfmt-visible-mount-mismatch" {
                        visible_registry.mount_id += 1;
                    } else {
                        visible_registry.policy_digest[0] ^= 1;
                    }
                }
            }
            "binfmt-hidden-parent-model" | "binfmt-incomplete-ancestry" => {
                if let BinfmtAuthority::NearestAncestor { ancestry, .. } = &mut evidence.binfmt {
                    ancestry[0].parent = Some(ancestry[0].user_namespace + 1);
                    if case == "binfmt-hidden-parent-model" {
                        ancestry[0].registry = None;
                    }
                }
            }
            "binfmt-nearest-empty-child"
            | "binfmt-nearest-disabled-child"
            | "binfmt-nearest-absent-child"
            | "binfmt-nearest-disabled-parent" => {
                if let BinfmtAuthority::NearestAncestor {
                    ancestry,
                    visible_registry,
                } = &mut evidence.binfmt
                {
                    let parent_namespace = ancestry[0].user_namespace.checked_add(1).unwrap();
                    ancestry[0].parent = Some(parent_namespace);
                    let mut parent = ancestry[0].registry.as_ref().unwrap().clone();
                    parent.identity.owner_user_namespace = parent_namespace;
                    parent.identity.mount_id += 1;
                    parent.identity.policy_digest[0] ^= 1;
                    parent.entries.push(elf_handler("nearest-parent", "COF"));
                    if case == "binfmt-nearest-disabled-child" {
                        ancestry[0].registry.as_mut().unwrap().enabled = false;
                    }
                    if case == "binfmt-nearest-absent-child"
                        || case == "binfmt-nearest-disabled-parent"
                    {
                        ancestry[0].registry = None;
                        *visible_registry = parent.identity.clone();
                    }
                    if case == "binfmt-nearest-disabled-parent" {
                        parent.enabled = false;
                    }
                    ancestry.push(NamespaceBinfmtEvidence {
                        user_namespace: parent_namespace,
                        parent: None,
                        registry: Some(parent),
                    });
                }
            }
            "binfmt-global-disabled"
            | "binfmt-entry-disabled"
            | "binfmt-elf-C"
            | "binfmt-elf-O"
            | "binfmt-elf-F"
            | "binfmt-script-final-elf-COF" => {
                let flags = case.strip_prefix("binfmt-elf-").unwrap_or("COF");
                let mut entry = elf_handler("final-ELF", flags);
                if case == "binfmt-entry-disabled" {
                    entry.enabled = false;
                }
                let registry = current_registry(&mut evidence);
                if case == "binfmt-global-disabled" {
                    registry.enabled = false;
                }
                registry.entries.push(entry);
            }
            "binfmt-elf-mask-offset"
            | "binfmt-mask-nonmatch"
            | "binfmt-script-magic-mask-offset" => {
                let rule = if case == "binfmt-script-magic-mask-offset" {
                    BinfmtMatch::Magic {
                        offset: 1,
                        magic: vec![0x20],
                        mask: Some(vec![0xf0]),
                    }
                } else {
                    BinfmtMatch::Magic {
                        offset: 1,
                        magic: vec![
                            if case == "binfmt-mask-nonmatch" {
                                0x50
                            } else {
                                0x40
                            };
                            3
                        ],
                        mask: Some(vec![0xf0; 3]),
                    }
                };
                current_registry(&mut evidence)
                    .entries
                    .push(handler("mask-offset", "COF", rule));
            }
            "binfmt-script-original-extension"
            | "binfmt-script-nested-extension-C"
            | "binfmt-script-nested-extension-O"
            | "binfmt-script-nested-extension-F"
            | "binfmt-script-nested-extension-COF"
            | "binfmt-extension-nonmatch" => {
                let flags = case
                    .strip_prefix("binfmt-script-nested-extension-")
                    .unwrap_or("COF");
                let extension = match case {
                    "binfmt-script-original-extension" => b"orig".to_vec(),
                    "binfmt-extension-nonmatch" => b"never-present".to_vec(),
                    _ => b"handler".to_vec(),
                };
                current_registry(&mut evidence).entries.push(handler(
                    "current-bprm-extension",
                    flags,
                    BinfmtMatch::Extension(extension),
                ));
            }
            "binfmt-registry-generation-changed" => {
                let approved = host_from_model(evidence.clone(), attestation)?;
                current_registry(&mut evidence).identity.generation += 1;
                approved.revalidate_evidence(&evidence)?;
                return Ok(approved);
            }
            "watches-unverified" => evidence.watches = PreContentWatchEvidence::Unverified,
            "watches-native-equivalent" => {
                evidence.watches = PreContentWatchEvidence::NativeEquivalentForLifetime(
                    exec_support::model_receipt(),
                )
            }
            "watches-missing-receipt" => {
                let mut receipt = exec_support::model_receipt();
                receipt.policy_digest = [0; 32];
                evidence.watches = PreContentWatchEvidence::NativeEquivalentForLifetime(receipt);
            }
            "watches-changed" => {
                let approved = host_from_model(evidence.clone(), attestation)?;
                evidence.watches = PreContentWatchEvidence::Unverified;
                approved.revalidate_evidence(&evidence)?;
                return Ok(approved);
            }
            "context-unreadable" => {
                evidence.context = ExecContextEvidence::Unreadable {
                    reason: "MODELED unavailable namespace credentials".into(),
                }
            }
            "context-pid1"
            | "context-uid-mismatch"
            | "context-gid-mismatch"
            | "context-incomplete-namespace"
            | "context-root-capability-clearing"
            | "context-root-capability-clearing-NNP"
            | "context-root-noroot-model"
            | "context-root-caps-already-permitted-model" => {
                if let ExecContextEvidence::Current(context) = &mut evidence.context {
                    match case {
                        "context-pid1" => context.namespace_pid = 1,
                        "context-uid-mismatch" => {
                            context.effective_uid = context.real_uid.checked_add(1).unwrap()
                        }
                        "context-gid-mismatch" => {
                            context.effective_gid = context.real_gid.checked_add(1).unwrap()
                        }
                        "context-incomplete-namespace" => context.mount_namespace = 0,
                        _ => {
                            context.real_uid = 0;
                            context.effective_uid = 0;
                            context.securebits = u32::from(case == "context-root-noroot-model");
                            context.inheritable_capabilities = 0b1000;
                            context.bounding_capabilities = 0b0101;
                            context.permitted_capabilities =
                                if case == "context-root-caps-already-permitted-model" {
                                    0b1101
                                } else {
                                    0b0001
                                };
                            context.no_new_privs = case == "context-root-capability-clearing-NNP";
                        }
                    }
                }
            }
            "receipt-missing-lifetime" => attestation.receipt.lifetime.clear(),
            other => panic!("unknown MODELED policy case: {other}"),
        }
    }
    host_from_model(evidence, attestation)
}

#[test]
fn lb_policy_prepare_child() {
    let Some(spec_path) = std::env::var_os(POLICY_SPEC) else {
        return;
    };
    let spec = fs::read(spec_path).unwrap();
    let mut decoder = Decoder(&spec);
    let case = String::from_utf8(decoder.bytes()).unwrap();
    let report_path = PathBuf::from(std::ffi::OsString::from_vec(decoder.bytes()));
    let dirfd = decoder.i32();
    let flags = decoder.i32();
    let original_fd_flags = decoder.i32();
    if dirfd != libc::AT_FDCWD {
        // SAFETY: this existing fixture descriptor was inherited by the fresh
        // child; restore exactly the flags of the native comparison's caller.
        assert_eq!(
            unsafe { libc::fcntl(dirfd, libc::F_SETFD, original_fd_flags) },
            0
        );
    }
    let request = ExecRequest {
        dirfd,
        flags,
        path: CString::new(decoder.bytes()).unwrap(),
        argv: decoder.strings(),
        envp: decoder.strings(),
    };
    assert!(decoder.0.is_empty());
    let output = match qualification(&case, &request) {
        Err(policy) => {
            let refusal = ExecRefusal::Host(policy);
            format!("before-lookup {}\n{refusal:?}\n", refusal.name())
        }
        Ok(host) => {
            let loader_host = LoaderHostFacts::current().unwrap();
            let options = PrepareExecOptions {
                launcher_link: Path::new("/lb"),
                host: &host,
                limits: Limits::current().unwrap(),
                loader_host: &loader_host,
                inherited_virtual_proc_state: false,
                interpreter_writer_fds: &[],
            };
            // SAFETY: only this isolated ordinary test child allocates FDs for
            // the call; owned request memory and fixtures remain immutable.
            // The host input is expressly MODELED, never live activation.
            match unsafe { prepare_exec(&request, &options) } {
                ExecCheckOutcome::Prepared(start) => {
                    start.verify_objects().unwrap();
                    assert_eq!(start.evidence.original_check, Some(0));
                    format!(
                        "prepared scripts={} original-check=0\n",
                        start.scripts.len()
                    )
                }
                ExecCheckOutcome::Refuse(refusal) => {
                    assert!(
                        matches!(&refusal, ExecRefusal::Host(_)),
                        "unexpected non-policy refusal: {refusal:?}"
                    );
                    format!("pinned-stage {}\n{refusal:?}\n", refusal.name())
                }
                ExecCheckOutcome::NativeErrno(error) => panic!(
                    "native success companion unexpectedly failed modeled preparation: {error:?}"
                ),
            }
        }
    };
    fs::write(report_path, output).unwrap();
}

#[test]
fn lb5_complete_mount_allowlist_and_detached_dirfd_native_companions() {
    let original = request("/bin/true");
    for filesystem in ["ext4", "xfs", "btrfs", "tmpfs", "squashfs", "erofs"] {
        pair(
            &format!("mount-safe-{filesystem}"),
            &original,
            Expected::Prepared(0),
        );
    }
    for filesystem in [
        "coda",
        "afs",
        "overlay",
        "fuse",
        "fuse.squashfuse_ll",
        "nfs",
        "cifs",
        "autofs",
        "unknown",
        "proc",
        "sysfs",
        "devtmpfs",
        "cgroup2",
    ] {
        pair(
            &format!("mount-unsafe-{filesystem}"),
            &original,
            Expected::BeforeLookup("UnsafeLookupMount"),
        );
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open("/bin")
        .unwrap();
    let relative = ExecRequest::execveat(
        directory.as_raw_fd(),
        "true",
        original.argv,
        original.envp,
        0,
    )
    .unwrap();
    pair(
        "detached-dirfd-model",
        &relative,
        Expected::BeforeLookup("LookupMountUnverified"),
    );
    report(
        "lb5-mounts",
        "20 exact-request native-success companions: six admitted filesystem models, thirteen unsafe complete-namespace models (including an unsafe mount elsewhere), and one modeled unlisted/detached binding for a real relative dirfd. A genuinely detached-mount fixture remains a missing privileged gate.",
    );
}

#[test]
fn lb6_lsm_integrity_and_bpf_native_companions() {
    let original = request("/bin/true");
    for case in [
        "lsm-unreadable",
        "lsm-empty",
        "lsm-duplicate",
        "lsm-unknown-selinux",
        "lsm-unknown-apparmor",
        "lsm-unknown-smack",
        "lsm-unknown-tomoyo",
        "lsm-unknown-ipe",
        "lsm-unknown-future_lsm",
        "receipt-missing-lifetime",
    ] {
        pair(
            case,
            &original,
            Expected::BeforeLookup("SecurityPolicyUnverified"),
        );
    }
    for case in [
        "integrity-unreadable",
        "integrity-bprm-appraisal",
        "integrity-ima-unattested",
        "integrity-measurement-missing-receipt",
    ] {
        pair(
            case,
            &original,
            Expected::BeforeLookup("IntegrityAppraisalUnsupported"),
        );
    }
    pair(
        "integrity-measurement-only",
        &original,
        Expected::Prepared(0),
    );
    for case in [
        "bpf-unattested",
        "bpf-active-inactive-evidence",
        "bpf-context-digest-mismatch",
        "bpf-receipt-missing",
        "bpf-policy-digest-changed",
        "bpf-generation-changed",
        "bpf-hook-scope-changed",
    ] {
        pair(case, &original, Expected::BeforeLookup("BpfLsmUnattested"));
    }
    for hook in 0..HOOKS.len() {
        pair(
            &format!("bpf-missing-hook-{hook}"),
            &original,
            Expected::BeforeLookup("BpfLsmUnattested"),
        );
    }
    pair("bpf-complete-model", &original, Expected::Prepared(0));
    report(
        "lb6-security",
        "31 exact-request native-success companions: unreadable/unknown/duplicate LSM authority, BPRM-only or unknown appraisal, measurement-only model, unattested BPF, all eight individual omitted hook scopes (including descriptor-based file_permission), context/receipt mismatch, and approved policy digest/generation/hook changes. Only explicitly labeled modeled attestations admit inactive preparation; no privileged security fixture is claimed.",
    );
}

#[test]
fn lb5_binfmt_authority_nearest_status_and_matching_native_companions() {
    let original = request("/bin/true");
    for case in [
        "binfmt-unreadable",
        "binfmt-visible-mount-mismatch",
        "binfmt-visible-digest-mismatch",
        "binfmt-hidden-parent-model",
        "binfmt-incomplete-ancestry",
        "binfmt-registry-generation-changed",
    ] {
        pair(
            case,
            &original,
            Expected::BeforeLookup("BinfmtRegistryUnverified"),
        );
    }
    for case in [
        "binfmt-nearest-empty-child",
        "binfmt-nearest-disabled-child",
        "binfmt-nearest-disabled-parent",
        "binfmt-global-disabled",
        "binfmt-entry-disabled",
        "binfmt-mask-nonmatch",
        "binfmt-extension-nonmatch",
    ] {
        pair(case, &original, Expected::Prepared(0));
    }
    for case in [
        "binfmt-nearest-absent-child",
        "binfmt-elf-C",
        "binfmt-elf-O",
        "binfmt-elf-F",
        "binfmt-elf-mask-offset",
    ] {
        let refusal = pair(
            case,
            &original,
            Expected::PinnedStage("BinfmtHandlerUnsupported"),
        );
        if case == "binfmt-elf-C" {
            assert!(refusal.contains("credentials: true") && refusal.contains("open_binary: true"));
        }
        if case == "binfmt-elf-O" {
            assert!(
                refusal.contains("credentials: false") && refusal.contains("open_binary: true")
            );
        }
        if case == "binfmt-elf-F" {
            assert!(refusal.contains("fixed_interpreter: true"));
        }
    }
    report(
        "lb5-binfmt-authority",
        "18 exact-request native-success companions: unreadable/hidden/mismatched authority, nearest instantiated empty or disabled child, absent child selecting parent, disabled global/entry status, ELF magic/mask/offset, separate C/O/F flags and nonmatching controls. Content-dependent refusals happen at the classified pinned header stage, not during authority collection.",
    );
}

#[test]
fn lb5_binfmt_each_script_stage_native_companions() {
    let directory = fixture_dir();
    let middle = directory.join("nested.handler");
    let outer = directory.join("original.orig");
    fs::write(&middle, b"#!/bin/true\n").unwrap();
    fs::set_permissions(&middle, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(&outer, format!("#!{}\n", middle.display())).unwrap();
    fs::set_permissions(&outer, fs::Permissions::from_mode(0o755)).unwrap();
    let original = request(&outer);
    pair("baseline", &original, Expected::Prepared(2));
    pair("binfmt-global-disabled", &original, Expected::Prepared(2));
    pair("binfmt-entry-disabled", &original, Expected::Prepared(2));
    pair(
        "binfmt-extension-nonmatch",
        &original,
        Expected::Prepared(2),
    );
    for case in [
        "binfmt-script-original-extension",
        "binfmt-script-magic-mask-offset",
        "binfmt-script-nested-extension-C",
        "binfmt-script-nested-extension-O",
        "binfmt-script-nested-extension-F",
        "binfmt-script-nested-extension-COF",
        "binfmt-script-final-elf-COF",
    ] {
        let refusal = pair(
            case,
            &original,
            Expected::PinnedStage("BinfmtHandlerUnsupported"),
        );
        if case == "binfmt-script-final-elf-COF" {
            assert!(refusal.contains("MODELED-final-ELF"));
        }
        if case.ends_with("COF") {
            assert!(
                refusal.contains("credentials: true")
                    && refusal.contains("open_binary: true")
                    && refusal.contains("fixed_interpreter: true")
            );
        }
    }
    report(
        "lb5-binfmt-scripts",
        "11 exact-request native-success companions on a real two-script chain: original F extension and #! magic/mask/offset, nested current bprm interp extension with C/O/F/COF, final ELF T magic, disabled status and nonmatching controls. Native scripts succeed, while matching model handlers are named refusals at the correct pinned content stage. No binfmt handler is installed on this machine.",
    );
}

#[test]
fn lb6_watches_namespace_ids_root_capability_and_nnp_native_companions() {
    let original = request("/bin/true");
    for case in [
        "watches-unverified",
        "watches-missing-receipt",
        "watches-changed",
    ] {
        pair(
            case,
            &original,
            Expected::BeforeLookup("PreContentWatchUnverified"),
        );
    }
    pair(
        "watches-native-equivalent",
        &original,
        Expected::Prepared(0),
    );
    pair(
        "context-pid1",
        &original,
        Expected::BeforeLookup("NamespaceInitUnsupported"),
    );
    for case in ["context-unreadable", "context-incomplete-namespace"] {
        pair(
            case,
            &original,
            Expected::BeforeLookup("SecurityPolicyUnverified"),
        );
    }
    for case in ["context-uid-mismatch", "context-gid-mismatch"] {
        pair(
            case,
            &original,
            Expected::BeforeLookup("SecureExecUnsupported"),
        );
    }
    for case in [
        "context-root-capability-clearing",
        "context-root-capability-clearing-NNP",
    ] {
        pair(
            case,
            &original,
            Expected::BeforeLookup("CapabilityPersonalityClearing"),
        );
    }
    for case in [
        "context-root-noroot-model",
        "context-root-caps-already-permitted-model",
    ] {
        pair(case, &original, Expected::Prepared(0));
    }
    report(
        "lb6-context-watches",
        "13 exact-request native-success companions: unverified/native-equivalent/missing/changed watch evidence, namespace PID1, unreadable namespace, UID/GID mismatches, root capability personality clearing including NNP clipping, and NOROOT/already-permitted model controls. Genuine watches, namespace-init and capability transitions are missing privileged activation gates.",
    );
}

#[test]
fn lb5_lb6_live_default_refusal_has_native_success_companion() {
    let original = request("/bin/true");
    let directory = fixture_dir();
    assert_eq!(
        exec_support::run_native(&original, &ChildSetup::default(), &directory),
        NativeObservation::Exited(0)
    );
    let refusal = model_observation("live-default-qualification", &original, &directory);
    assert!(refusal.starts_with("before-lookup "), "{refusal}");
    assert!(
        !refusal.contains("MODELED"),
        "live collector must not invent approval: {refusal}"
    );
    report(
        "lb5-lb6-live-default",
        &format!(
            "Real ordinary native request succeeds; real default HostQualification::collect_current refuses before target lookup: {refusal}"
        ),
    );
}
