//! Launch observations for regular Nextest calibration. These observations do
//! not set limits or replace the prepared-artifact/source admission checks.
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::nextest_cpu::CpuQuota;
use crate::nextest_cpu::ExecutionCohort;
use crate::nextest_cpu::ExecutionDomain;
use crate::nextest_cpu::LaunchProofRef;
use crate::nextest_cpu::ResourceEnvelope;
use crate::nextest_cpu::ResourceLimit;

pub const PINNED_PROOF_PATH: &str = "/run/hermit-nextest-launch.json";
const MAX_PROOF: u64 = 262_144;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Controls {
    #[serde(deserialize_with = "required_quota")]
    cpu: Option<CpuQuota>,
    cpuset: Vec<u32>,
    memory: ResourceLimit,
    swap: ResourceLimit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Layer {
    anchor: Anchor,
    controls: Controls,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HostObservation {
    membership: String,
    namespace: String,
    affinity: Vec<u32>,
    /// Leaf first. The final entry is the host hierarchy root.
    ancestors: Vec<Layer>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
enum Observation {
    Unavailable {
        reason: String,
    },
    Complete {
        domain: ExecutionDomain,
        host: HostObservation,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LaunchObservation {
    schema: u64,
    invocation: String,
    observation: Observation,
}

fn required_quota<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<CpuQuota>, D::Error> {
    Option::deserialize(deserializer)
}

fn anchor(file: &File) -> Result<Anchor, String> {
    let m = file.metadata().map_err(|e| e.to_string())?;
    Ok(Anchor {
        device: m.dev(),
        inode: m.ino(),
    })
}

fn directory(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("cannot open cgroup directory {}: {e}", path.display()))?;
    let mut stat = unsafe { std::mem::zeroed::<libc::statfs>() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 || stat.f_type != 0x6367_7270 {
        return Err("launch resource observation requires a cgroup-v2 filesystem".into());
    }
    Ok(file)
}

fn child(directory: &File, name: &str, flags: i32) -> Result<File, std::io::Error> {
    let name = std::ffi::CString::new(name).expect("internal cgroup component contains no NUL");
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn bounded(file: File, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("launch observation exceeds its bounded input size".into());
    }
    Ok(bytes)
}

fn control(directory: &File, name: &str, global_root: bool) -> Result<Option<String>, String> {
    let file = match child(directory, name, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if global_root && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(format!("cannot observe cgroup {name}: {error}")),
    };
    String::from_utf8(bounded(file, 16_384)?)
        .map(|v| Some(v.trim().into()))
        .map_err(|e| e.to_string())
}

fn limit(value: Option<String>) -> Result<ResourceLimit, String> {
    match value.as_deref() {
        None | Some("max") => Ok(ResourceLimit::Unlimited),
        Some(value) => value
            .parse::<u64>()
            .map(ResourceLimit::Max)
            .map_err(|e| format!("invalid cgroup resource limit: {e}")),
    }
}

pub fn cpu_set(value: &str) -> Result<Vec<u32>, String> {
    let mut result = BTreeSet::new();
    for part in value.split(',') {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let a = a.parse::<u32>().map_err(|_| "invalid cgroup CPU set")?;
        let b = b.parse::<u32>().map_err(|_| "invalid cgroup CPU set")?;
        if b < a || b - a > 65_536 || b > 1_048_576 {
            return Err("invalid cgroup CPU range".into());
        }
        for cpu in a..=b {
            if !result.insert(cpu) {
                return Err("overlapping cgroup CPU ranges".into());
            }
        }
        if result.len() > 65_536 {
            return Err("cgroup CPU set exceeds supported observation size".into());
        }
    }
    if result.is_empty() {
        return Err("empty effective cgroup CPU set".into());
    }
    Ok(result.into_iter().collect())
}

fn controls(directory: &File, global_root: bool) -> Result<Controls, String> {
    let cpu = match control(directory, "cpu.max", global_root)? {
        None => None,
        Some(raw) => {
            let words = raw.split_whitespace().collect::<Vec<_>>();
            if words.len() != 2 {
                return Err("invalid cgroup cpu.max".into());
            }
            let period = words[1]
                .parse::<u64>()
                .map_err(|_| "invalid CPU quota period")?;
            if period == 0 {
                return Err("zero CPU quota period".into());
            }
            if words[0] == "max" {
                None
            } else {
                let quota = words[0].parse::<u64>().map_err(|_| "invalid CPU quota")?;
                if quota == 0 {
                    return Err("zero CPU quota".into());
                }
                Some(CpuQuota {
                    quota_usec: quota,
                    period_usec: period,
                })
            }
        }
    };
    Ok(Controls {
        cpu,
        cpuset: cpu_set(
            &control(directory, "cpuset.cpus.effective", false)?
                .ok_or("missing effective cpuset")?,
        )?,
        memory: limit(control(directory, "memory.max", global_root)?)?,
        swap: limit(control(directory, "memory.swap.max", global_root)?)?,
    })
}

fn bounded_text(path: &str, limit: u64) -> Result<String, String> {
    let file = File::open(path).map_err(|e| format!("cannot observe {path}: {e}"))?;
    String::from_utf8(bounded(file, limit)?).map_err(|e| e.to_string())
}

fn membership() -> Result<String, String> {
    let raw = bounded_text("/proc/self/cgroup", 16_384)?;
    let paths = raw
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .collect::<Vec<_>>();
    if paths.len() != 1
        || !paths[0].starts_with('/')
        || Path::new(paths[0])
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err("cannot establish a unified cgroup-v2 membership".into());
    }
    Ok(paths[0].into())
}

fn affinity() -> Result<Vec<u32>, String> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) } != 0 {
        return Err(format!(
            "cannot observe CPU affinity: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((0..libc::CPU_SETSIZE)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu as usize, &set) })
        .map(|cpu| cpu as u32)
        .collect())
}

fn is_container() -> bool {
    Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists()
}

fn observe_host() -> Result<HostObservation, String> {
    if is_container() {
        return Err("native launch observation is unavailable inside a container".into());
    }
    let membership = membership()?;
    let root = directory(Path::new("/sys/fs/cgroup"))?;
    let root_id = anchor(&root)?;
    // Linux cgroup-v2's global kernfs root is inode 1. A private cgroup
    // namespace exposes its leaf with another inode even when mountinfo says
    // root "/". Never interpret missing controls as unlimited on that leaf.
    if root_id.inode != 1 {
        return Err("global host cgroup root identity is unavailable".into());
    }
    // The maintained native launch boundary supplies host authority. Require
    // its global cgroup mount, not a namespaced leaf presented at the same path.
    let mounts = bounded_text("/proc/self/mountinfo", 1_048_576)?;
    if !mounts.lines().any(|line| {
        let words = line.split_whitespace().collect::<Vec<_>>();
        words.get(3) == Some(&"/")
            && words.get(4) == Some(&"/sys/fs/cgroup")
            && line.contains(" - cgroup2 ")
    }) {
        return Err("complete host cgroup hierarchy is unavailable".into());
    }
    let mut current = directory(Path::new("/sys/fs/cgroup"))?;
    for part in Path::new(&membership).components() {
        if let Component::Normal(name) = part {
            current = child(
                &current,
                name.to_str().ok_or("non-UTF-8 cgroup component")?,
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    let mut ancestors = Vec::new();
    for _ in 0..64 {
        let id = anchor(&current)?;
        let global_root = id == root_id;
        ancestors.push(Layer {
            anchor: id,
            controls: controls(&current, global_root)?,
        });
        if global_root {
            return Ok(HostObservation {
                membership,
                namespace: fs::read_link("/proc/self/ns/cgroup")
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
                affinity: affinity()?,
                ancestors,
            });
        }
        current =
            child(&current, "..", libc::O_RDONLY | libc::O_DIRECTORY).map_err(|e| e.to_string())?;
    }
    Err("host cgroup hierarchy exceeds the bounded observation depth".into())
}

fn minimum(a: ResourceLimit, b: &ResourceLimit) -> ResourceLimit {
    match (a, b) {
        (ResourceLimit::Max(a), ResourceLimit::Max(b)) => ResourceLimit::Max(a.min(*b)),
        (ResourceLimit::Unlimited, b) => b.clone(),
        (a, ResourceLimit::Unlimited) => a,
    }
}

fn cohort(domain: &ExecutionDomain, host: &HostObservation) -> Result<ExecutionCohort, String> {
    if host.ancestors.is_empty()
        || host.ancestors.len() > 64
        || host.namespace.is_empty()
        || !host.membership.starts_with('/')
        || host
            .ancestors
            .last()
            .is_none_or(|layer| layer.anchor.inode != 1)
        || host
            .ancestors
            .iter()
            .map(|layer| (layer.anchor.device, layer.anchor.inode))
            .collect::<BTreeSet<_>>()
            .len()
            != host.ancestors.len()
    {
        return Err("malformed complete launch observation".into());
    }
    let mut cpu = BTreeSet::new();
    let mut memory = ResourceLimit::Unlimited;
    let mut swap = ResourceLimit::Unlimited;
    for layer in &host.ancestors {
        if layer.anchor.device == 0
            || layer.anchor.inode == 0
            || layer.controls.cpuset.is_empty()
            || layer
                .controls
                .cpuset
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err("malformed ancestor in complete launch observation".into());
        }
        if let Some(limit) = &layer.controls.cpu {
            cpu.insert(limit.clone());
        }
        memory = minimum(memory, &layer.controls.memory);
        swap = minimum(swap, &layer.controls.swap);
    }
    let value = ExecutionCohort {
        schema: 1,
        domain: domain.clone(),
        resources: ResourceEnvelope {
            cpu_limits: cpu.into_iter().collect(),
            affinity: host.affinity.clone(),
            cpuset: host.ancestors[0].controls.cpuset.clone(),
            memory_bytes: memory,
            swap_bytes: swap,
        },
    };
    value.validate()?;
    Ok(value)
}

/// Called only at the explicit maintained native launch boundary. Never
/// executes a compiler, creates a cgroup or changes a resource control.
pub fn capture(domain: ExecutionDomain, output: &Path) -> Result<(), String> {
    domain.validate()?;
    if !output.is_absolute() {
        return Err("launch proof requires a fresh absolute output path".into());
    }
    let observation = match observe_host() {
        Ok(first) => match observe_host() {
            Ok(second) if first == second => {
                cohort(&domain, &first)?;
                Observation::Complete {
                    domain,
                    host: first,
                }
            }
            Ok(_) => Observation::Unavailable {
                reason: "cgroup membership or limits changed during launch capture".into(),
            },
            Err(reason) => Observation::Unavailable { reason },
        },
        Err(reason) => Observation::Unavailable { reason },
    };
    let bytes = serde_json::to_vec(&LaunchObservation {
        schema: 1,
        invocation: output.to_string_lossy().into_owned(),
        observation,
    })
    .map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .custom_flags(libc::O_NOFOLLOW)
        .open(output)
        .map_err(|e| {
            format!(
                "cannot exclusively create launch proof {}: {e}; use a fresh per-invocation path",
                output.display()
            )
        })?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

fn container_image() -> Result<(String, String), String> {
    let raw = bounded_text("/run/.containerenv", 16_384)?;
    let field = |key: &str| -> Result<String, String> {
        let prefix = format!("{key}=\"");
        let values = raw
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix).and_then(|v| v.strip_suffix('"')))
            .collect::<Vec<_>>();
        if values.len() != 1 {
            return Err(format!("Podman runtime evidence has no unique {key}"));
        }
        Ok(values[0].into())
    };
    Ok((field("image")?, field("imageid")?))
}

pub fn verify(path: &Path) -> Result<(Option<ExecutionCohort>, Option<LaunchProofRef>), String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(e) => return Err(format!("cannot read launch proof {}: {e}", path.display())),
    };
    let before = file.metadata().map_err(|e| e.to_string())?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_PROOF {
        return Err("launch proof must be a bounded nonempty regular file".into());
    }
    let bytes = bounded(file.try_clone().map_err(|e| e.to_string())?, MAX_PROOF)?;
    let after = file.metadata().map_err(|e| e.to_string())?;
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    ) || bytes.len() as u64 != before.len()
    {
        return Err("launch proof changed while read".into());
    }
    let proof: LaunchObservation =
        serde_json::from_slice(&bytes).map_err(|e| format!("malformed launch proof: {e}"))?;
    if proof.schema != 1 || !Path::new(&proof.invocation).is_absolute() {
        return Err("unsupported or malformed launch proof identity".into());
    }
    let value = match proof.observation {
        Observation::Unavailable { reason } => {
            if reason.trim().is_empty() {
                return Err("unavailable launch observation requires a reason".into());
            }
            None
        }
        Observation::Complete { domain, host } => {
            let value = cohort(&domain, &host)?;
            match &domain {
                ExecutionDomain::Host => {
                    if observe_host()? != host {
                        return Err("native launch membership or resource limits changed; capture a fresh launch proof".into());
                    }
                }
                ExecutionDomain::PinnedRoot { image, image_id } => {
                    // Only the wrapper's dedicated read-only mount conveys
                    // hidden host ancestors. A retained copy is evidence, not
                    // authority, even if its visible leaf/image happen to match.
                    if path != Path::new(PINNED_PROOF_PATH) {
                        return Err("pinned launch proof must come from the maintained wrapper's fixed read-only mount".into());
                    }
                    let mounts = bounded_text("/proc/self/mountinfo", 1_048_576)?;
                    let entries = mounts
                        .lines()
                        .filter_map(|line| {
                            let words = line.split_whitespace().collect::<Vec<_>>();
                            (words.get(4) == Some(&PINNED_PROOF_PATH)).then_some(words)
                        })
                        .collect::<Vec<_>>();
                    if entries.len() != 1
                        || !entries[0]
                            .get(5)
                            .is_some_and(|flags| flags.split(',').any(|flag| flag == "ro"))
                    {
                        return Err("pinned launch proof has no unique dedicated read-only mount; use the maintained wrapper".into());
                    }
                    if container_image()? != (image.clone(), image_id.clone()) {
                        return Err(
                            "actual Podman image differs from the launch observation".into()
                        );
                    }
                    let root = directory(Path::new("/sys/fs/cgroup"))?;
                    if membership()? != "/"
                        || anchor(&root)? != host.ancestors[0].anchor
                        || controls(&root, false)? != host.ancestors[0].controls
                        || affinity()? != host.affinity
                    {
                        return Err("pinned launch cgroup anchor or visible limits differ from the host observation".into());
                    }
                    // Outer ancestors were observed by the host launcher. They
                    // are hidden here; do not claim an inner live recheck.
                }
            }
            Some(value)
        }
    };
    Ok((
        value,
        Some(LaunchProofRef {
            path: path.to_string_lossy().into_owned(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        }),
    ))
}
