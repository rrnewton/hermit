//! Closed host-capability vocabulary and probes shared by validation records.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

/// A machine facility that determines which validation population can run.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
pub enum HostCapability {
    #[serde(rename = "cpuid-faulting")]
    CpuidFaulting,
    #[serde(rename = "kvm")]
    Kvm,
}

impl HostCapability {
    pub const ALL: [Self; 2] = [Self::CpuidFaulting, Self::Kvm];

    pub fn value(self) -> &'static str {
        match self {
            Self::CpuidFaulting => "cpuid-faulting",
            Self::Kvm => "kvm",
        }
    }

    pub fn from_value(text: &str) -> Option<Self> {
        match text {
            "cpuid-faulting" => Some(Self::CpuidFaulting),
            "kvm" => Some(Self::Kvm),
            _ => None,
        }
    }
}

/// The effective verdict used to decide whether work ran on this machine.
///
/// `present` records the decision, including the existing fail-open rule that
/// doubt runs the work. `evidence` preserves why that decision was made.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityVerdict {
    /// `true` unless absence was positively established. Doubt means present.
    pub present: bool,
    /// What was actually observed, recorded verbatim so a reader never has to
    /// take the verdict on trust.
    pub evidence: String,
}

pub type HostCapabilities = BTreeMap<HostCapability, CapabilityVerdict>;

/// The one environment override, deliberately one-directional: it can only add
/// capabilities to the PRESENT set. Nothing can force a capability ABSENT.
pub const ASSUME_PRESENT_ENV: &str = "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT";

/// Ask the machine whether it has one capability.
///
/// Every uncertain outcome resolves to PRESENT, so a probe can only cause more
/// work to run. It can never excuse a failing node or cell.
pub fn probe_host_capability(capability: HostCapability) -> CapabilityVerdict {
    let forced = std::env::var(ASSUME_PRESENT_ENV).unwrap_or_default();
    if forced
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == capability.value())
    {
        return CapabilityVerdict {
            present: true,
            evidence: format!(
                "{ASSUME_PRESENT_ENV} names {}; assumed PRESENT without probing (this override can only ADD capabilities)",
                capability.value()
            ),
        };
    }
    match capability {
        HostCapability::CpuidFaulting => probe_cpuid_faulting(),
        HostCapability::Kvm => probe_kvm(),
    }
}

/// Record every capability in the closed vocabulary, including present ones.
pub fn probe_host_capabilities() -> HostCapabilities {
    HostCapability::ALL
        .into_iter()
        .map(|capability| (capability, probe_host_capability(capability)))
        .collect()
}

/// `true` only when two independent observations establish absence.
///
/// The kernel must return `ENODEV` from `arch_prctl(ARCH_SET_CPUID, 0)` and
/// `/proc/cpuinfo` must omit `cpuid_fault`. Any error, unreadable input, or
/// disagreement means PRESENT so the work runs and can fail normally.
pub fn cpuid_faulting_absent(syscall: Result<(), i32>, advertised: Option<bool>) -> bool {
    syscall == Err(libc::ENODEV) && advertised == Some(false)
}

fn probe_cpuid_faulting() -> CapabilityVerdict {
    let syscall = arch_prctl_set_cpuid_off();
    let advertised = cpuinfo_advertises_cpuid_fault();
    let syscall_text = match syscall {
        Ok(()) => "arch_prctl(ARCH_SET_CPUID, 0) = 0".to_string(),
        Err(0) => "arch_prctl(ARCH_SET_CPUID, 0) probe could not be completed".to_string(),
        Err(errno) => format!("arch_prctl(ARCH_SET_CPUID, 0) = -1 errno={errno}"),
    };
    let cpuinfo_text = match advertised {
        Some(true) => "/proc/cpuinfo advertises cpuid_fault",
        Some(false) => "/proc/cpuinfo does not advertise cpuid_fault",
        None => "/proc/cpuinfo could not be read",
    };
    CapabilityVerdict {
        present: !cpuid_faulting_absent(syscall, advertised),
        evidence: format!("{syscall_text}; {cpuinfo_text}"),
    }
}

fn arch_prctl_set_cpuid_off() -> Result<(), i32> {
    const ARCH_SET_CPUID: libc::c_int = 0x1012;
    // SAFETY: the child performs one syscall and `_exit`s; it never returns into
    // Rust, allocates, or touches inherited locks.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(0);
    }
    if child == 0 {
        let result = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) };
        let code = if result == 0 {
            0
        } else {
            // SAFETY: reading the thread-local errno immediately after the
            // failed syscall.
            let errno = unsafe { *libc::__errno_location() };
            errno.clamp(1, 255)
        };
        // SAFETY: this is the forked probe child and it must not run Rust
        // destructors inherited from the parent.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    if unsafe { libc::waitpid(child, &mut status, 0) } != child || !libc::WIFEXITED(status) {
        return Err(0);
    }
    match libc::WEXITSTATUS(status) {
        0 => Ok(()),
        errno => Err(errno),
    }
}

fn cpuinfo_advertises_cpuid_fault() -> Option<bool> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    Some(text.split_whitespace().any(|word| word == "cpuid_fault"))
}

/// `true` only when `/dev/kvm` and the processor flags independently establish
/// absence. Any error or disagreement means PRESENT.
///
/// The KVM DAG node separately refuses when `/dev/kvm` cannot be opened and
/// asserts its selected test count. That second guard matters because the KVM
/// tests themselves return early when the device is absent; a permissive probe
/// must not turn those early returns into apparent coverage.
pub fn kvm_absent(open: Result<(), i32>, advertised: Option<bool>) -> bool {
    open == Err(libc::ENOENT) && advertised == Some(false)
}

fn probe_kvm() -> CapabilityVerdict {
    let open = open_dev_kvm();
    let advertised = cpuinfo_advertises_virtualization();
    let open_text = match open {
        Ok(()) => "open(/dev/kvm, O_RDWR) = ok".to_string(),
        Err(errno) => format!("open(/dev/kvm, O_RDWR) = -1 errno={errno}"),
    };
    let cpuinfo_text = match advertised {
        Some(true) => "/proc/cpuinfo advertises vmx or svm",
        Some(false) => "/proc/cpuinfo advertises neither vmx nor svm",
        None => "/proc/cpuinfo could not be read",
    };
    CapabilityVerdict {
        present: !kvm_absent(open, advertised),
        evidence: format!("{open_text}; {cpuinfo_text}"),
    }
}

fn open_dev_kvm() -> Result<(), i32> {
    let path = std::ffi::CString::new("/dev/kvm").expect("static path has no NUL");
    // SAFETY: `path` is a valid NUL-terminated C string that outlives the call,
    // and the descriptor is closed immediately on success.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd >= 0 {
        // SAFETY: `fd` was just returned by a successful `open`.
        unsafe { libc::close(fd) };
        return Ok(());
    }
    // SAFETY: reading the thread-local errno immediately after a failed call.
    let errno = unsafe { *libc::__errno_location() };
    Err(errno.clamp(1, 255))
}

fn cpuinfo_advertises_virtualization() -> Option<bool> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    Some(
        text.split_whitespace()
            .any(|word| word == "vmx" || word == "svm"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_probe_records_every_closed_capability_with_evidence() {
        let capabilities = probe_host_capabilities();
        assert_eq!(capabilities.len(), HostCapability::ALL.len());
        for capability in HostCapability::ALL {
            let verdict = capabilities
                .get(&capability)
                .expect("every closed capability must have a verdict");
            assert!(!verdict.evidence.trim().is_empty());
        }
    }
}
