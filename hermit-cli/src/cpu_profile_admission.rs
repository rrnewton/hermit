/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Whether a run may use `--cpu-profile x86-64-v3`, and the host facts it
//! binds when it may (<https://github.com/rrnewton/hermit/issues/4243>).
//!
//! Every class the startup qualification cannot cover is refused by name; the
//! run never falls back to v2.

use detcore_model::cpu_profile::CpuProfile;
use detcore_model::cpu_profile::HostXstate;

use crate::xstate_probe;

/// Why a v3 run is refused, as one sentence after "--cpu-profile x86-64-v3
/// refused: ".
pub type AdmissionRefusal = String;

/// A refused x86-64-v3 recording or replay, carried as an error so that it
/// crosses the container boundary as a policy refusal (exit 122).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuProfileRefusal(pub AdmissionRefusal);

impl std::fmt::Display for CpuProfileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "--cpu-profile x86-64-v3 refused: {}", self.0)
    }
}

impl std::error::Error for CpuProfileRefusal {}

/// The admission request of a recording or replay, which always runs on the
/// ptrace backend with virtual CPUID.
pub fn record_or_replay_request(gdbserver: bool) -> AdmissionRequest<'static> {
    AdmissionRequest {
        backend: "ptrace",
        ptrace: true,
        virtualize_cpuid: true,
        strace_only: false,
        gdbserver,
        namespace_only: false,
    }
}

/// The first host fact in which `replay` differs from `recorded`, by name, or
/// `None` when they are equal.
pub fn first_difference(recorded: &HostXstate, replay: &HostXstate) -> Option<&'static str> {
    if recorded.vendor != replay.vendor {
        Some("the CPU vendor")
    } else if recorded.signature != replay.signature {
        Some("the CPU family, model or stepping")
    } else if recorded.xcr0 != replay.xcr0 {
        Some("XCR0")
    } else if recorded.xsave_leaves != replay.xsave_leaves {
        Some("the XSAVE layout (CPUID leaf 0xD)")
    } else if (
        recorded.xcomp_supp,
        recorded.xcomp_perm,
        recorded.xcomp_guest_perm,
    ) != (
        replay.xcomp_supp,
        replay.xcomp_perm,
        replay.xcomp_guest_perm,
    ) {
        Some("the ARCH_GET_XCOMP masks")
    } else if recorded.xfeature_max != replay.xfeature_max {
        Some("XFEATURE_MAX")
    } else if (recorded.probe_cases, recorded.probe_forced_exit)
        != (replay.probe_cases, replay.probe_forced_exit)
    {
        Some("the qualification it passed")
    } else {
        None
    }
}

/// What the run asks for, as far as admission is concerned.
#[derive(Clone, Copy, Debug)]
pub struct AdmissionRequest<'a> {
    /// The backend's command-line name, and whether it is the ptrace backend.
    pub backend: &'a str,
    pub ptrace: bool,
    pub virtualize_cpuid: bool,
    pub strace_only: bool,
    pub gdbserver: bool,
    /// `--namespace-only`, which runs the guest with no Detcore at all.
    pub namespace_only: bool,
}

/// The refusals that need no host probe, in the order they are checked.
pub fn check_request(request: &AdmissionRequest<'_>) -> Result<(), AdmissionRefusal> {
    if !request.ptrace {
        let why = match request.backend {
            "kvm" => {
                "the KVM backend switches XCR0 between the guest's and the host's on every VM \
                 exit, a transition the startup qualification does not cover"
            }
            "dbt" => {
                "Detcore answers DBT's CPUID, but the DBT runtime's configuration carries no \
                 CPU profile, so it answers only the x86-64-v2 table"
            }
            "sabre" | "liteinst" | "in-guest-trap" => {
                "this backend does not trap CPUID, so the guest would see the host's"
            }
            _ => "only the ptrace backend is qualified",
        };
        return Err(format!("--backend={}: {why}", request.backend));
    }
    if request.namespace_only {
        return Err(
            "--namespace-only runs the guest without Detcore, so nothing answers its CPUID".into(),
        );
    }
    if !request.virtualize_cpuid || request.strace_only {
        return Err(
            "the profile is a virtual CPUID table, and this run does not virtualize CPUID".into(),
        );
    }
    if request.gdbserver {
        return Err(
            "--gdbserver writes the guest's FP registers, and the kernel's regset write forces \
             XSAVE header bits the qualification binds"
                .into(),
        );
    }
    Ok(())
}

/// Linux's version as (major, minor), from a `uname -r` release string.
pub fn kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(['.', '-']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Linux 5.16 is the first with the `ARCH_*_XCOMP_*` queries the profile
/// answers from the host; before it they return EINVAL.
pub fn check_kernel(release: &str) -> Result<(), AdmissionRefusal> {
    match kernel_version(release) {
        Some(version) if version >= (5, 16) => Ok(()),
        Some((major, minor)) => Err(format!(
            "Linux {major}.{minor} predates the 5.16 xstate queries the profile answers from"
        )),
        None => Err(format!("cannot read the kernel version from {release:?}")),
    }
}

/// A host CPUID reader, so tests can supply a host.
pub trait HostCpuid {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4];
}

/// The CPUID instruction of the host this process runs on.
pub struct NativeCpuid;

impl HostCpuid for NativeCpuid {
    fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
        let r = std::arch::x86_64::__cpuid_count(leaf, subleaf);
        [r.eax, r.ebx, r.ecx, r.edx]
    }
}

const REGISTER_NAMES: [&str; 4] = ["EAX", "EBX", "ECX", "EDX"];

/// The host checks that need only CPUID, in the order they are checked:
/// every advertised v3 feature bit natively, OSXSAVE, not a hybrid part, and
/// not an AMD part without XSAVEERPTR (where Linux runs `fnclex; emms; fildl`
/// before every FPU restore, a path no host here can exercise).
pub fn check_host_cpuid(
    host: &dyn HostCpuid,
    advertised: &[(u32, u32, usize, u32)],
) -> Result<(), AdmissionRefusal> {
    let max_basic = host.cpuid(0, 0)[0];
    let max_extended = host.cpuid(0x8000_0000, 0)[0];
    let mut missing = Vec::new();
    for &(leaf, subleaf, register, bits) in advertised {
        let supported = if leaf >= 0x8000_0000 {
            leaf <= max_extended
        } else {
            leaf <= max_basic
        };
        let native = if supported {
            host.cpuid(leaf, subleaf)[register]
        } else {
            0
        };
        let absent = bits & !native;
        for bit in 0..32 {
            if absent & (1 << bit) != 0 {
                missing.push(format!(
                    "leaf {leaf:#x}.{subleaf} {} bit {bit}",
                    REGISTER_NAMES[register]
                ));
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "the host lacks feature bits the profile advertises: {}",
            missing.join(", ")
        ));
    }
    if host.cpuid(1, 0)[2] & (1 << 27) == 0 {
        return Err("the host kernel has not enabled XSAVE (OSXSAVE is clear)".into());
    }
    if max_basic >= 7 && host.cpuid(7, 0)[3] & (1 << 15) != 0 {
        return Err(
            "this is a hybrid part (leaf 7 EDX bit 15), whose core types the qualification \
             would each have to sample"
                .into(),
        );
    }
    let vendor = host.cpuid(0, 0);
    let amd = vendor[1] == 0x6874_7541;
    let xsaveerptr = max_extended >= 0x8000_0008 && host.cpuid(0x8000_0008, 0)[1] & (1 << 2) != 0;
    if amd && !xsaveerptr {
        return Err(
            "this AMD part lacks XSAVEERPTR, so Linux runs `fnclex; emms; fildl` before every \
             FPU restore, which the qualification cannot exercise"
                .into(),
        );
    }
    Ok(())
}

/// The user-dynamic xstate component (AMX TILEDATA): Linux enables it in XCR0
/// while withholding process permission, so a permission request changes
/// process state and the three XCOMP answers differ.
const XFEATURE_TILEDATA: u64 = 1 << 18;

const ARCH_GET_XCOMP_SUPP: libc::c_int = 0x1021;
const ARCH_GET_XCOMP_PERM: libc::c_int = 0x1022;
const ARCH_REQ_XCOMP_PERM: libc::c_int = 0x1023;
const ARCH_GET_XCOMP_GUEST_PERM: libc::c_int = 0x1024;

fn arch_prctl_mask(cmd: libc::c_int) -> Result<u64, AdmissionRefusal> {
    let mut mask = 0u64;
    // SAFETY: the GET commands write one u64 through the pointer.
    let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, cmd, &mut mask as *mut u64) };
    if rc != 0 {
        return Err(format!(
            "arch_prctl({cmd:#x}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(mask)
}

/// Linux's XFEATURE_MAX as ARCH_REQ_XCOMP_PERM shows it: the first component
/// index it answers EINVAL for. Every index below must answer EOPNOTSUPP; an
/// index that is granted would mean a dynamic feature, which is refused
/// before this runs.
fn xfeature_max() -> Result<u32, AdmissionRefusal> {
    for index in 0..64u64 {
        // SAFETY: the request takes the component index by value.
        let rc = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_REQ_XCOMP_PERM, index) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        match (rc, errno) {
            (-1, Some(libc::EINVAL)) => return Ok(index as u32),
            (-1, Some(libc::EOPNOTSUPP)) => {}
            _ => {
                return Err(format!(
                    "ARCH_REQ_XCOMP_PERM for component {index} answered {rc} ({errno:?}), \
                     not EOPNOTSUPP or EINVAL"
                ));
            }
        }
    }
    Err("ARCH_REQ_XCOMP_PERM never answered EINVAL for indices 0-63".into())
}

/// The host facts a qualified v3 run binds.
fn capture(probe: &xstate_probe::ProbeReport) -> Result<HostXstate, AdmissionRefusal> {
    let host = host_facts(probe.cases as u32, probe.forced_exit)?;
    if host.xcr0 != probe.xcr0 {
        return Err(format!(
            "XCR0 changed during admission ({:#x}, then {:#x})",
            probe.xcr0, host.xcr0
        ));
    }
    Ok(host)
}

/// This host's facts, with the qualification fields given rather than probed.
/// The caller has checked OSXSAVE.
fn host_facts(probe_cases: u32, probe_forced_exit: bool) -> Result<HostXstate, AdmissionRefusal> {
    let host = NativeCpuid;
    let leaf0 = host.cpuid(0, 0);
    // SAFETY: XGETBV with ECX 0 is available because OSXSAVE was checked.
    let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
    let mut xsave_leaves = Vec::new();
    for subleaf in std::iter::once(0).chain(2..64) {
        if subleaf == 0 || xcr0 & (1 << subleaf) != 0 {
            let [eax, ebx, ecx, edx] = host.cpuid(0xd, subleaf);
            xsave_leaves.push([subleaf, eax, ebx, ecx, edx]);
        }
    }
    let xcomp_supp = arch_prctl_mask(ARCH_GET_XCOMP_SUPP)?;
    refuse_dynamic_features(xcr0, xcomp_supp)?;
    Ok(HostXstate {
        vendor: [leaf0[1], leaf0[3], leaf0[2]],
        signature: host.cpuid(1, 0)[0],
        xcr0,
        xsave_leaves,
        xcomp_supp,
        xcomp_perm: arch_prctl_mask(ARCH_GET_XCOMP_PERM)?,
        xcomp_guest_perm: arch_prctl_mask(ARCH_GET_XCOMP_GUEST_PERM)?,
        xfeature_max: xfeature_max()?,
        probe_cases,
        probe_forced_exit,
    })
}

/// Refuses a v3 replay before the qualification probe runs when the recording
/// carries no host facts, when this host differs in one of the facts it bound
/// (the qualification itself aside), or when its CPUID table digest is not the
/// one this replayer computes from them. The probe costs seconds of CPU, and a
/// replay on a different CPU, or of an altered recording, needs none of it to
/// be refused. Returns the recorded facts.
pub fn precheck_replay(
    profile: CpuProfile,
    recorded: Option<&HostXstate>,
    recorded_digest: Option<&str>,
) -> Result<HostXstate, CpuProfileRefusal> {
    let refuse = |why: String| CpuProfileRefusal(why);
    let recorded = recorded.cloned().ok_or_else(|| {
        refuse("the recording names the profile but carries no host facts".into())
    })?;
    check_host_cpuid(&NativeCpuid, &detcore::v3_feature_words()).map_err(refuse)?;
    let host = host_facts(recorded.probe_cases, recorded.probe_forced_exit).map_err(refuse)?;
    if let Some(fact) = first_difference(&recorded, &host) {
        return Err(refuse(format!(
            "this host differs from the recording's in {fact}, so the guest would see a \
             different CPU"
        )));
    }
    let config = detcore::Config {
        cpu_profile: profile,
        cpu_profile_host: Some(recorded.clone()),
        ..Default::default()
    };
    let digest = crate::record::hex_digest(&detcore::virtual_cpuid_digest(&config));
    if recorded_digest != Some(digest.as_str()) {
        return Err(refuse(format!(
            "the recording's CPUID table digest {recorded_digest:?} is not this replayer's \
             {digest}"
        )));
    }
    Ok(recorded)
}

/// This process's successful qualification. A recording or replay qualifies
/// in the CLI process, before its container child is forked, so the child
/// reuses this result instead of probing again from inside the container,
/// whose CPU affinity can be one busy CPU.
static ADMITTED: std::sync::OnceLock<(xstate_probe::ProbeReport, HostXstate)> =
    std::sync::OnceLock::new();

/// Admits a v3 run on this host, or names why not. On success the probe report
/// and the host facts to bind are returned, and this process's ptrace
/// floating-point register writes are refused from then on. The request's own
/// refusals are checked every time; the host qualification runs once per
/// process.
pub fn admit(
    request: &AdmissionRequest<'_>,
) -> Result<(xstate_probe::ProbeReport, HostXstate), AdmissionRefusal> {
    admit_with(request, || qualify_host_with(cached_probe))
}

/// [`admit`] with its host qualification as a parameter, so that a test can
/// admit without probing.
fn admit_with(
    request: &AdmissionRequest<'_>,
    qualify: impl FnOnce() -> Result<(xstate_probe::ProbeReport, HostXstate), AdmissionRefusal>,
) -> Result<(xstate_probe::ProbeReport, HostXstate), AdmissionRefusal> {
    check_request(request)?;
    if let Some(admitted) = ADMITTED.get() {
        return Ok(admitted.clone());
    }
    let qualified = qualify()?;
    // The qualification covers the XSTATE_BV a guest's own instructions
    // produce. A tracer write of its floating-point registers (PTRACE_SETFPREGS
    // or PTRACE_SETREGSET NT_X86_XSTATE) sets that header from the tracer's
    // buffer instead, which nothing checked, so it fails with EPERM. The flag
    // is process-wide and set before the container child is forked, so the
    // tracer inherits it. It is set before the admission becomes visible in
    // [`ADMITTED`], so an admitted launch always runs with it.
    safeptrace::forbid_fpu_register_writes();
    Ok(ADMITTED.get_or_init(|| qualified).clone())
}

/// Refuses a launch of an x86-64-v3 configuration that this process's
/// admission did not produce. Hermit's launch functions call this before any
/// backend starts, so a library caller cannot run a v3 guest on host facts of
/// its own, on another backend, or without the floating-point register write
/// ban: the facts must be exactly the ones [`admit`] bound after the host
/// qualified, and [`admit`] is the only way to bind them.
pub fn refuse_unadmitted_launch(
    config: &detcore::Config,
    ptrace: bool,
) -> Result<(), CpuProfileRefusal> {
    if config.cpu_profile.is_default() {
        return Ok(());
    }
    let refuse = |why: &str| Err(CpuProfileRefusal(why.to_owned()));
    if !ptrace {
        return refuse("only the ptrace backend is qualified");
    }
    if !config.virtualize_cpuid || config.gdbserver {
        return refuse("the launch does not virtualize CPUID, or serves a gdbserver");
    }
    match (&config.cpu_profile_host, ADMITTED.get()) {
        (None, _) => return refuse("the configuration carries no qualified host facts"),
        (Some(_), None) => return refuse("this process did not qualify the host"),
        (Some(facts), Some((_, admitted))) if facts != admitted => {
            return refuse(
                "the configuration's host facts are not the ones this process qualified",
            );
        }
        _ => {}
    }
    if !safeptrace::fpu_register_writes_forbidden() {
        return refuse("ptrace floating-point register writes are not forbidden");
    }
    Ok(())
}

/// The host half of [`admit`], run once per process: the kernel, the
/// every-bit CPUID check, the dynamic-feature refusal, the probe and the host
/// facts. `hermit host-capabilities --cpu-profile x86-64-v3` runs it alone.
/// It always measures and never reads or writes the cache [`admit`] uses.
pub fn qualify_host() -> Result<(xstate_probe::ProbeReport, HostXstate), AdmissionRefusal> {
    qualify_host_with(|_| xstate_probe::qualify(xstate_probe::ProbeOptions::default()))
}

/// The name of the XINUSE probe's file in the host qualification cache.
const PROBE_CACHE_NAME: &str = "cpu-profile-x86-64-v3-probe";

fn probe_cache_path() -> Option<std::path::PathBuf> {
    crate::host_qualification::default_path(PROBE_CACHE_NAME)
}

/// The probe's inputs besides the host: its options. XCR0 is checked on the
/// report itself ([`cached_report_matches`]).
fn probe_cache_inputs() -> String {
    format!("options={:?}", xstate_probe::ProbeOptions::default())
}

/// The XINUSE probe for [`admit`]: its passing report cached for this host
/// (`crate::host_qualification`), or else a fresh probe, announced on stderr
/// because it takes seconds, and cached if it passes. The probe certifies the
/// CPU's and kernel's XSAVE behavior, which no sandbox changes, so the host
/// key suffices; the checks that depend on the caller's sandbox run on every
/// run (<https://github.com/rrnewton/hermit/issues/4515>). Under a hypervisor
/// nothing is cached: a live migration keeps the boot, the kernel and CPUID
/// but moves the guest to a host the forced-exit arm never certified.
fn cached_probe(xcr0: u64) -> Result<xstate_probe::ProbeReport, AdmissionRefusal> {
    let hypervisor = std::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0;
    cached_probe_at(
        cache_path_for(hypervisor).as_deref(),
        &crate::host_qualification::HostKey::current(),
        xcr0,
        &mut |line: &str| eprint!("{line}"),
        || xstate_probe::qualify(xstate_probe::ProbeOptions::default()),
    )
}

/// The probe's cache file, or `None` under a hypervisor, where nothing is
/// cached ([`cached_probe`]).
fn cache_path_for(hypervisor: bool) -> Option<std::path::PathBuf> {
    if hypervisor {
        return None;
    }
    probe_cache_path()
}

/// Whether a cached report is one this host's probe would produce: it binds
/// `cases` and `forced_exit` into a recording (`HostXstate::probe_cases` and
/// `probe_forced_exit`), so they are derived again here, from XCR0 and from
/// the cache's use only off a hypervisor, and a report that disagrees is a
/// miss. Its repetitions and pair count must be the default probe's too.
fn cached_report_matches(report: &xstate_probe::ProbeReport, xcr0: u64) -> bool {
    let options = xstate_probe::ProbeOptions::default();
    report.xcr0 == xcr0
        && report.cases == xstate_probe::expected_case_count(xcr0)
        && !report.forced_exit
        && report.repetitions == options.repetitions
        && report.certified_pairs == report.cells as u64 * u64::from(report.repetitions)
}

/// [`cached_probe`] with its cache path (`None`: no cache), host key, warning
/// writer and probe as parameters, for the tests.
fn cached_probe_at(
    path: Option<&std::path::Path>,
    host: &crate::host_qualification::HostKey,
    xcr0: u64,
    warn: &mut dyn FnMut(&str),
    probe: impl FnOnce() -> Result<xstate_probe::ProbeReport, xstate_probe::ProbeRefusal>,
) -> Result<xstate_probe::ProbeReport, AdmissionRefusal> {
    let Some(path) = path else {
        warn(
            "WARNING: qualifying this host for --cpu-profile x86-64-v3, which takes seconds; \
             with no cache to use it is measured again on every run.\n",
        );
        return probe();
    };
    let inputs = probe_cache_inputs();
    let cached = || {
        crate::host_qualification::load::<xstate_probe::ProbeReport>(path, host, &inputs)
            .filter(|report| cached_report_matches(report, xcr0))
    };
    if let Some(report) = cached() {
        return Ok(report);
    }
    // Checked again under the lock: another process that missed at the same
    // time may have measured and stored a pass while this one waited.
    crate::host_qualification::locked(path, || {
        if let Some(report) = cached() {
            return Ok(report);
        }
        warn(&if crate::host_qualification::writable(path) {
            format!(
                "WARNING: qualifying this host for --cpu-profile x86-64-v3, which takes \
                 seconds; a pass is cached in {} until the kernel, CPU, microcode, boot or \
                 Hermit binary changes.\n",
                path.display()
            )
        } else {
            format!(
                "WARNING: qualifying this host for --cpu-profile x86-64-v3, which takes \
                 seconds; {} cannot be written, so it is measured again on every run.\n",
                path.display()
            )
        });
        let report = probe()?;
        crate::host_qualification::store(path, host, &inputs, &report);
        Ok(report)
    })
}

/// [`qualify_host`] with the XINUSE probe as a parameter, given XCR0: the
/// kernel, the every-bit CPUID check and the dynamic-feature refusal run every
/// time, and the host facts are captured from this host every time.
fn qualify_host_with(
    probe: impl FnOnce(u64) -> Result<xstate_probe::ProbeReport, AdmissionRefusal>,
) -> Result<(xstate_probe::ProbeReport, HostXstate), AdmissionRefusal> {
    let release = nix::sys::utsname::uname()
        .map_err(|e| format!("uname failed: {e}"))?
        .release()
        .to_string_lossy()
        .into_owned();
    check_kernel(&release)?;
    check_host_cpuid(&NativeCpuid, &detcore::v3_feature_words())?;
    // SAFETY: OSXSAVE was checked above.
    let xcr0 = unsafe { std::arch::x86_64::_xgetbv(0) };
    refuse_dynamic_features(xcr0, 0)?;
    let probe = probe(xcr0)?;
    let host = capture(&probe)?;
    Ok((probe, host))
}

/// Refuses a host with a user-dynamic xstate feature (AMX TILEDATA, bit 18) in
/// XCR0 or in ARCH_GET_XCOMP_SUPP: a guest's permission request for it changes
/// process state, which the profile cannot bind.
fn refuse_dynamic_features(xcr0: u64, xcomp_supp: u64) -> Result<(), AdmissionRefusal> {
    if (xcr0 | xcomp_supp) & XFEATURE_TILEDATA != 0 {
        return Err(
            "the host has a user-dynamic xstate feature (AMX TILEDATA), whose permission \
             requests change process state"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    /// A report the default probe could produce for `xcr0`.
    fn probe_report(xcr0: u64) -> xstate_probe::ProbeReport {
        let cases = xstate_probe::expected_case_count(xcr0);
        let repetitions = xstate_probe::ProbeOptions::default().repetitions;
        xstate_probe::ProbeReport {
            cpu: 3,
            xcr0,
            cases,
            cells: cases * 12,
            repetitions,
            certified_pairs: (cases * 12) as u64 * u64::from(repetitions),
            forced_exit: false,
            reload_bound: 0,
            cpuid_min_cycles: 0,
            calibration: "cached".into(),
            retried_references: 0,
            retried_reloads: 0,
            retried_exits: 0,
            counter_reopens: 0,
        }
    }

    fn cache_host() -> crate::host_qualification::HostKey {
        crate::host_qualification::HostKey {
            kernel_release: "7.1.3".into(),
            boot_id: "boot-a".into(),
            cpu: "AuthenticAMD family 0x1a model 0x11 stepping 0x0".into(),
            microcode: "0xb101054".into(),
            hermit_binary: "64768:123:456:789.0".into(),
        }
    }

    /// Runs [`cached_probe_at`] once and returns its result, whether the probe
    /// ran and what it warned.
    fn probe_through_cache(
        path: Option<&std::path::Path>,
        xcr0: u64,
        answer: Result<xstate_probe::ProbeReport, xstate_probe::ProbeRefusal>,
    ) -> (
        Result<xstate_probe::ProbeReport, AdmissionRefusal>,
        bool,
        Vec<String>,
    ) {
        let mut ran = false;
        let mut warnings = Vec::new();
        let result = cached_probe_at(
            path,
            &cache_host(),
            xcr0,
            &mut |line: &str| warnings.push(line.to_owned()),
            || {
                ran = true;
                answer
            },
        );
        (result, ran, warnings)
    }

    /// The first v3 admission on a host measures, warning once on stderr, and
    /// caches the passing report; later ones reuse it without measuring or
    /// warning (<https://github.com/rrnewton/hermit/issues/4515>).
    #[test]
    fn a_passing_probe_is_measured_once_and_announced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hermit").join("probe.json");
        let (result, ran, warnings) =
            probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
        assert_eq!(result, Ok(probe_report(0x2e7)));
        assert!(ran);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("WARNING: "), "{warnings:?}");
        assert!(
            warnings[0].contains(&path.display().to_string()),
            "{warnings:?}"
        );
        assert_eq!(warnings[0].matches('\n').count(), 1, "{warnings:?}");
        let (result, ran, warnings) =
            probe_through_cache(Some(&path), 0x2e7, Err("must not probe".into()));
        assert_eq!(result, Ok(probe_report(0x2e7)));
        assert!(!ran);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// A refusal is not cached, and a different XCR0 measures again.
    #[test]
    fn a_refusal_or_a_new_xcr0_measures_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe.json");
        let (result, ran, _) = probe_through_cache(Some(&path), 0x2e7, Err("refused".into()));
        assert_eq!(result, Err("refused".into()));
        assert!(ran);
        let (result, ran, _) = probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
        assert!(ran && result.is_ok());
        let (result, ran, _) = probe_through_cache(Some(&path), 0x207, Ok(probe_report(0x207)));
        assert_eq!(result, Ok(probe_report(0x207)));
        assert!(ran, "XCR0 is part of the key");
    }

    /// A cached report whose recorded fields this host's probe would not
    /// produce is a miss: the case count and the forced-exit arm are bound
    /// into recordings, so an edited or foreign file never supplies them.
    #[test]
    fn a_cached_report_that_disagrees_with_this_host_is_a_miss() {
        type Edit = fn(&mut xstate_probe::ProbeReport);
        let edits: [(&str, Edit); 5] = [
            ("cases", |r| r.cases = 1),
            ("forced exit", |r| r.forced_exit = true),
            ("repetitions", |r| r.repetitions += 1),
            ("certified pairs", |r| r.certified_pairs += 1),
            ("xcr0", |r| r.xcr0 ^= 1 << 9),
        ];
        for (field, edit) in edits {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("probe.json");
            let mut edited = probe_report(0x2e7);
            edit(&mut edited);
            crate::host_qualification::store(&path, &cache_host(), &probe_cache_inputs(), &edited);
            let (result, ran, _) = probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
            assert!(ran, "{field}: a disagreeing report was reused");
            assert_eq!(result, Ok(probe_report(0x2e7)), "{field}");
        }
    }

    /// Under a hypervisor nothing is cached; elsewhere the cache file is used.
    #[test]
    fn nothing_is_cached_under_a_hypervisor() {
        assert_eq!(cache_path_for(true), None);
        assert_eq!(cache_path_for(false), probe_cache_path());
    }

    /// When the cache cannot be written the warning says it is measured again,
    /// not that it is cached.
    #[test]
    fn an_unwritable_cache_is_announced_as_such() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("hermit");
        std::fs::write(&blocker, b"a file where the directory would go").unwrap();
        let path = blocker.join("probe.json");
        let (result, ran, warnings) =
            probe_through_cache(Some(&path), 0x2e7, Ok(probe_report(0x2e7)));
        assert!(ran && result.is_ok());
        assert!(warnings[0].contains("cannot be written"), "{warnings:?}");
        assert!(!warnings[0].contains("is cached"), "{warnings:?}");
    }

    /// Without a cache directory every admission measures, and says so.
    #[test]
    fn without_a_cache_directory_every_admission_measures() {
        for _ in 0..2 {
            let (result, ran, warnings) = probe_through_cache(None, 0x2e7, Ok(probe_report(0x2e7)));
            assert!(ran && result.is_ok());
            assert!(
                warnings[0].contains("measured again on every run"),
                "{warnings:?}"
            );
        }
    }

    /// A host with AMX TILEDATA in XCR0 or in the supported mask is refused by
    /// name; one without it is not.
    #[test]
    fn a_dynamic_xstate_feature_is_refused() {
        assert_eq!(super::refuse_dynamic_features(0x2e7, 0x2e7), Ok(()));
        for (xcr0, supp) in [(0x602e7, 0x2e7), (0x2e7, 0x602e7), (0x402e7, 0x402e7)] {
            assert!(
                super::refuse_dynamic_features(xcr0, supp)
                    .unwrap_err()
                    .contains("AMX TILEDATA"),
                "{xcr0:#x} {supp:#x}"
            );
        }
    }

    use super::*;

    fn request(backend: &str) -> AdmissionRequest<'_> {
        AdmissionRequest {
            backend,
            ptrace: backend == "ptrace",
            virtualize_cpuid: true,
            strace_only: false,
            gdbserver: false,
            namespace_only: false,
        }
    }

    #[test]
    fn only_a_ptrace_run_with_virtual_cpuid_and_no_gdbserver_is_considered() {
        assert_eq!(check_request(&request("ptrace")), Ok(()));
        for backend in [
            "kvm",
            "dbt",
            "sabre",
            "liteinst",
            "in-guest-trap",
            "e9patch",
        ] {
            let refusal = check_request(&request(backend)).unwrap_err();
            assert!(
                refusal.starts_with(&format!("--backend={backend}")),
                "{refusal}"
            );
        }
        assert!(check_request(&request("kvm")).unwrap_err().contains("XCR0"));
        assert!(
            check_request(&request("dbt"))
                .unwrap_err()
                .contains("carries no CPU profile")
        );
        let mut r = request("ptrace");
        r.virtualize_cpuid = false;
        assert!(check_request(&r).is_err());
        let mut r = request("ptrace");
        r.strace_only = true;
        assert!(check_request(&r).is_err());
        let mut r = request("ptrace");
        r.gdbserver = true;
        assert!(check_request(&r).unwrap_err().contains("--gdbserver"));
        let mut r = request("ptrace");
        r.namespace_only = true;
        assert!(check_request(&r).unwrap_err().contains("--namespace-only"));
    }

    /// The launch certificate, in one process (no other test admits): a v3
    /// configuration with facts but no admission is refused; a refused
    /// request neither probes nor forbids floating-point writes; an admission
    /// forbids them before it is visible, and then exactly the admitted facts
    /// launch, on ptrace only.
    #[test]
    fn only_the_admitted_facts_launch_and_the_write_ban_comes_first() {
        let host = HostXstate {
            vendor: [1, 2, 3],
            signature: 0x00b1_0f11,
            xcr0: 0x2e7,
            xsave_leaves: vec![[0, 0x2e7, 0x988, 0x988, 0]],
            xcomp_supp: 0x2e7,
            xcomp_perm: 0x2e7,
            xcomp_guest_perm: 0x2e7,
            xfeature_max: 20,
            probe_cases: 33,
            probe_forced_exit: false,
        };
        let mut config = detcore::Config {
            cpu_profile: detcore_model::cpu_profile::CpuProfile::X86_64V3,
            cpu_profile_host: Some(host.clone()),
            ..Default::default()
        };
        let unadmitted = refuse_unadmitted_launch(&config, true).unwrap_err();
        assert!(unadmitted.0.contains("did not qualify"), "{unadmitted}");
        assert_eq!(
            refuse_unadmitted_launch(&detcore::Config::default(), false),
            Ok(())
        );

        let refused = admit_with(&request("kvm"), || {
            panic!("a refused request probed the host")
        });
        assert!(refused.is_err());
        assert!(!safeptrace::fpu_register_writes_forbidden());
        assert!(ADMITTED.get().is_none());

        let report = xstate_probe::ProbeReport {
            cpu: 3,
            xcr0: 0x2e7,
            cases: 33,
            cells: 396,
            repetitions: 32,
            certified_pairs: 12672,
            forced_exit: false,
            reload_bound: 0,
            cpuid_min_cycles: 0,
            calibration: String::new(),
            retried_references: 0,
            retried_reloads: 0,
            retried_exits: 0,
            counter_reopens: 0,
        };
        let admitted = admit_with(&request("ptrace"), || {
            // The ban is not yet in force while the host qualifies.
            assert!(!safeptrace::fpu_register_writes_forbidden());
            Ok((report.clone(), host.clone()))
        });
        assert_eq!(admitted, Ok((report, host.clone())));
        assert!(safeptrace::fpu_register_writes_forbidden());
        assert_eq!(refuse_unadmitted_launch(&config, true), Ok(()));
        assert!(refuse_unadmitted_launch(&config, false).is_err());
        let mut other = host;
        other.probe_cases = 0;
        config.cpu_profile_host = Some(other);
        let forged = refuse_unadmitted_launch(&config, true).unwrap_err();
        assert!(
            forged.0.contains("not the ones this process qualified"),
            "{forged}"
        );
        config.cpu_profile_host = None;
        assert!(refuse_unadmitted_launch(&config, true).is_err());
    }

    #[test]
    fn a_replay_host_is_compared_fact_by_fact() {
        let recorded = HostXstate {
            vendor: [1, 2, 3],
            signature: 0x00b1_0f11,
            xcr0: 0x2e7,
            xsave_leaves: vec![[0, 0x2e7, 0x988, 0x988, 0]],
            xcomp_supp: 0x2e7,
            xcomp_perm: 0x2e7,
            xcomp_guest_perm: 0x2e7,
            xfeature_max: 20,
            probe_cases: 33,
            probe_forced_exit: false,
        };
        assert_eq!(first_difference(&recorded, &recorded.clone()), None);
        let differs = |f: fn(&mut HostXstate)| {
            let mut other = recorded.clone();
            f(&mut other);
            first_difference(&recorded, &other)
        };
        assert_eq!(differs(|h| h.vendor[0] = 9), Some("the CPU vendor"));
        assert_eq!(
            differs(|h| h.signature = 0x00a1_0f11),
            Some("the CPU family, model or stepping")
        );
        assert_eq!(differs(|h| h.xcr0 = 0x207), Some("XCR0"));
        assert_eq!(
            differs(|h| h.xsave_leaves[0][2] = 0xa88),
            Some("the XSAVE layout (CPUID leaf 0xD)")
        );
        assert_eq!(
            differs(|h| h.xcomp_perm = 0x207),
            Some("the ARCH_GET_XCOMP masks")
        );
        assert_eq!(differs(|h| h.xfeature_max = 19), Some("XFEATURE_MAX"));
        assert_eq!(
            differs(|h| h.probe_forced_exit = true),
            Some("the qualification it passed")
        );
        assert_eq!(
            CpuProfileRefusal("why".into()).to_string(),
            "--cpu-profile x86-64-v3 refused: why"
        );
    }

    #[test]
    fn kernels_before_5_16_are_refused() {
        assert_eq!(kernel_version("7.1.3-0_fbk0_0_g295ad0959f34"), Some((7, 1)));
        assert_eq!(check_kernel("5.16.0"), Ok(()));
        assert_eq!(check_kernel("6.4.3-0_fbk20"), Ok(()));
        assert!(check_kernel("5.15.120").is_err());
        assert!(check_kernel("4.18.0-553.el8").is_err());
        assert!(check_kernel("garbage").is_err());
    }

    struct FakeHost(BTreeMap<(u32, u32), [u32; 4]>);

    impl HostCpuid for FakeHost {
        fn cpuid(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
            self.0.get(&(leaf, subleaf)).copied().unwrap_or([0; 4])
        }
    }

    /// A bare-metal-shaped host with every v3 bit, AMD with XSAVEERPTR.
    fn qualifying_host() -> FakeHost {
        let mut regs = BTreeMap::new();
        regs.insert((0, 0), [0xd, 0x6874_7541, 0x444d_4163, 0x6974_6e65]);
        regs.insert((0x8000_0000, 0), [0x8000_0008, 0, 0, 0]);
        regs.insert((0x8000_0008, 0), [0, 1 << 2, 0, 0]);
        for &(leaf, subleaf, register, bits) in &detcore::v3_feature_words() {
            regs.entry((leaf, subleaf)).or_insert([0; 4])[register] |= bits;
        }
        regs.get_mut(&(1, 0)).unwrap()[2] |= 1 << 27;
        FakeHost(regs)
    }

    #[test]
    fn a_host_with_every_v3_bit_qualifies_and_removing_any_one_is_named() {
        let words = detcore::v3_feature_words();
        let host = qualifying_host();
        assert_eq!(check_host_cpuid(&host, &words), Ok(()));
        // The hypervisor bit is not required: v3 clears it.
        assert!(words.iter().all(|&(leaf, _, register, bits)| !(leaf == 1
            && register == 2
            && bits & (1 << 31) != 0)));
        let mut checked = 0;
        for &(leaf, subleaf, register, bits) in &words {
            for bit in (0..32).filter(|b| bits & (1 << b) != 0) {
                let mut host = qualifying_host();
                host.0.get_mut(&(leaf, subleaf)).unwrap()[register] &= !(1 << bit);
                if leaf == 1 && register == 2 && bit == 27 {
                    continue; // OSXSAVE: refused by its own check, below
                }
                let refusal = check_host_cpuid(&host, &words).unwrap_err();
                assert!(
                    refusal.contains(&format!(
                        "leaf {leaf:#x}.{subleaf} {} bit {bit}",
                        REGISTER_NAMES[register]
                    )),
                    "{refusal}"
                );
                checked += 1;
            }
        }
        let advertised: u32 = words.iter().map(|&(_, _, _, bits)| bits.count_ones()).sum();
        // Every advertised bit but OSXSAVE, which has its own check.
        assert_eq!(checked, advertised - 1);
    }

    #[test]
    fn osxsave_hybrid_parts_and_old_amd_are_refused() {
        let words = detcore::v3_feature_words();
        let mut host = qualifying_host();
        host.0.get_mut(&(1, 0)).unwrap()[2] &= !(1 << 27);
        assert!(check_host_cpuid(&host, &words).is_err());
        let mut host = qualifying_host();
        host.0.get_mut(&(7, 0)).unwrap()[3] |= 1 << 15;
        assert!(
            check_host_cpuid(&host, &words)
                .unwrap_err()
                .contains("hybrid")
        );
        let mut host = qualifying_host();
        host.0.get_mut(&(0x8000_0008, 0)).unwrap()[1] = 0;
        assert!(
            check_host_cpuid(&host, &words)
                .unwrap_err()
                .contains("XSAVEERPTR")
        );
    }
}
