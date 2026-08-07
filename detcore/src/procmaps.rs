/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;

pub use procfs::process::MMapPath;
pub use procfs::process::MemoryMap;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;

use crate::Digest;

// `display`/`display_pathname` used to render the whole ptrace memory record --
// bounds, permissions, and a bracketed pathname (`[heap]`). Both producers now
// share `format_memory_record`, which takes the region kind through
// `region_kind_token` and the remaining `/proc` fields through `map_detail`, so
// the old renderer had no callers left.

/// The canonical kind token for a hashed memory region, IDENTICAL across
/// backends.
///
/// The two producers of a `[memory]` record name the same region differently:
/// the `/proc/<pid>/maps` path derives it from [`MMapPath`] (`[heap]`) and the
/// backend-reported path from `reverie::DetlogRegionKind` (`Heap`). Two spellings
/// of one concept make the records incomparable before their digests are ever
/// examined, so both callers route through this token instead.
pub fn region_kind_token(kind: &str) -> &'static str {
    match kind {
        "Heap" | "heap" => "heap",
        "Stack" | "stack" => "stack",
        _ => "other",
    }
}

/// One `[memory]` detlog record, in the single shape both producers emit.
///
/// # Why the domain travels with the digest
///
/// A digest is meaningless without the extent it was taken over: `SHA256` of
/// `0x1000` zero bytes and `SHA256` of `0xd80` zero bytes are different values
/// describing *identical* memory. Before this, a record carried only the bounds
/// and the digest in two different layouts, so a domain difference and a content
/// difference were the same observation -- a comparator could see that two
/// records disagreed but not WHY, and the two failure modes have opposite
/// remedies. `size=` is therefore emitted explicitly rather than left implicit in
/// `end - start`: it is the field a comparator keys on to classify a divergence
/// as MEASUREMENT DOMAIN rather than BYTE CONTENT.
///
/// The digest itself is unchanged and still covers exactly `[start, end)`. This
/// function records what was measured; it does not alter the measurement. In
/// particular it does not pad, truncate, or page-align any range to make two
/// backends agree -- that would trade a visible divergence for an invisible one
/// and would weaken the same-backend `--verify` oracle, where the domains
/// already match.
///
/// `detail` carries producer-specific trailing fields (the `/proc` path's
/// permissions, offset, device and inode) and is absent for a backend-reported
/// region, which has no such metadata. Everything a cross-backend comparison
/// needs -- kind, bounds, size, digest -- precedes it in a fixed layout.
pub fn format_memory_record(
    dettid: impl fmt::Display,
    kind: &str,
    start: u64,
    end: u64,
    detail: Option<&str>,
    digest: &Digest,
) -> String {
    // Saturating for the same reason `compute_hash_range` saturates: an inverted
    // range must report a zero-size domain, not wrap to a huge one.
    let size = end.saturating_sub(start);
    let mut record = format!(
        "[memory][dtid {}] {} {:#x}-{:#x} size={:#x}",
        dettid,
        region_kind_token(kind),
        start,
        end,
        size
    );
    if let Some(detail) = detail {
        record.push(' ');
        record.push_str(detail);
    }
    record.push_str("->");
    record.push_str(&digest.to_string());
    record
}

/// The producer-specific trailing metadata for a `/proc/<pid>/maps` entry.
///
/// The pathname is deliberately omitted: it is exactly the information already
/// canonicalized into the kind token, and repeating it as `[heap]` beside `heap`
/// would reintroduce the spelling difference this record exists to remove.
pub fn map_detail(map: &MemoryMap) -> String {
    format!(
        "{:?} {:x} {:x}:{:x} {}",
        map.perms, map.offset, map.dev.0, map.dev.1, map.inode
    )
}

/// The comparable fields of a `[memory]` record, recovered from a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRecord {
    /// Canonical region kind (`heap` / `stack`), identical across backends.
    pub kind: String,
    /// Size of the hashed range in bytes: the MEASUREMENT DOMAIN.
    pub size: u64,
    /// Hex digest of the bytes in that range.
    pub digest: String,
}

/// How two `[memory]` records differ.
///
/// The whole point of recording the domain is that these are DIFFERENT
/// FINDINGS with different remedies, and a bare "the digests disagree" cannot
/// tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryDivergence {
    /// Same domain, same digest.
    None,
    /// Different region kinds compared -- the records are not counterparts.
    KindMismatch { left: String, right: String },
    /// MEASUREMENT DOMAIN: different numbers of bytes were hashed, so the
    /// digests cannot match however identical the memory is. Not evidence of a
    /// content difference, and not evidence of its absence either -- it is an
    /// unmeasured comparison.
    Domain { left: u64, right: u64 },
    /// BYTE CONTENT: the same number of bytes hashed to different values. This
    /// is a real divergence.
    Content { size: u64 },
}

/// Recover the comparable fields from a formatted `[memory]` record.
///
/// Returns `None` for a line that is not one, including a pre-`size=` record
/// from an older log -- an old record genuinely cannot state its domain, and
/// inferring one from `end - start` would manufacture the very field whose
/// absence is the point.
pub fn parse_memory_record(line: &str) -> Option<MemoryRecord> {
    let rest = line.split_once("[memory][dtid ")?.1;
    let rest = rest.split_once("] ")?.1;
    let (kind, rest) = rest.split_once(' ')?;
    let (_bounds, rest) = rest.split_once(" size=")?;
    let (size, rest) = rest.split_once("->").map(|(s, d)| {
        // `detail`, when present, sits between the size and the digest.
        match s.split_once(' ') {
            Some((size, _detail)) => (size, d),
            None => (s, d),
        }
    })?;
    Some(MemoryRecord {
        kind: kind.to_string(),
        size: u64::from_str_radix(size.trim().strip_prefix("0x")?, 16).ok()?,
        digest: rest.trim().to_string(),
    })
}

/// Classify a disagreement between two `[memory]` records.
///
/// Domain is checked BEFORE content, because an unequal domain makes the digest
/// comparison meaningless rather than merely failed: reporting "different bytes"
/// off the back of a size mismatch is the exact conflation this record shape was
/// changed to prevent.
pub fn classify_memory_divergence(left: &MemoryRecord, right: &MemoryRecord) -> MemoryDivergence {
    if left.kind != right.kind {
        return MemoryDivergence::KindMismatch {
            left: left.kind.clone(),
            right: right.kind.clone(),
        };
    }
    if left.size != right.size {
        return MemoryDivergence::Domain {
            left: left.size,
            right: right.size,
        };
    }
    if left.digest != right.digest {
        return MemoryDivergence::Content { size: left.size };
    }
    MemoryDivergence::None
}

fn map_error(err: procfs::ProcError) -> reverie::Error {
    match err {
        procfs::ProcError::Io(err, _) => reverie::Error::Io(err),
        err => reverie::Error::Tool(anyhow::anyhow!(err)),
    }
}

pub fn from_pid<F>(pid: Pid, filter: F) -> Result<Vec<MemoryMap>, reverie::Error>
where
    F: Fn(&MemoryMap) -> bool,
{
    match procfs::process::Process::new(pid.as_raw()) {
        Ok(process) => match process.maps() {
            Ok(mut maps) => {
                maps.0.retain(filter);
                Ok(maps.0)
            }
            Err(err) => Err(map_error(err)),
        },
        Err(err) => Err(map_error(err)),
    }
}

pub fn compute_hash<G, T: Tool>(guest: &mut G, map: &MemoryMap) -> Result<Digest, reverie::Error>
where
    G: Guest<T>,
{
    compute_hash_range(guest, map.address.0, map.address.1)
}

/// Hash the guest bytes in the half-open guest-virtual range `[start, end)`,
/// read through `guest.memory()`. Used for backend-reported memory regions
/// ([`reverie::Guest::detlog_memory_regions`]) where the range is a guest
/// address rather than an entry parsed from a host `/proc/<pid>/maps`.
pub fn compute_hash_range<G, T: Tool>(
    guest: &mut G,
    start: u64,
    end: u64,
) -> Result<Digest, reverie::Error>
where
    G: Guest<T>,
{
    let size = end.saturating_sub(start) as usize;
    let memory = guest.memory();
    let mut buf = vec![0; size];
    let start = Addr::<u8>::from_raw(start as usize).unwrap();
    memory.read_values(start, buf.as_mut_slice())?;
    Ok(Digest::new(buf.as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Digest of `n` zero bytes -- the shape of a freshly-`brk`'d heap tail,
    /// which is what made the record1 artifact so easy to misread as a content
    /// difference: both regions really are all zeroes.
    fn zeros(n: usize) -> Digest {
        Digest::new(&vec![0u8; n])
    }

    #[test]
    fn record_carries_its_domain_size() {
        let rec = format_memory_record(3, "Heap", 0x602000, 0x603000, None, &zeros(0x1000));
        assert!(
            rec.contains("size=0x1000"),
            "the domain must be an explicit field, not implied by the bounds: {rec}"
        );
        let parsed = parse_memory_record(&rec).expect("own output must parse");
        assert_eq!(parsed.size, 0x1000);
        assert_eq!(parsed.kind, "heap");
    }

    #[test]
    fn both_producers_emit_the_same_shape_for_the_same_region() {
        // The ptrace producer additionally carries /proc metadata; the
        // backend-reported one has none. The COMPARABLE fields must still line
        // up, which is what "compares like-for-like" means.
        let digest = zeros(0x1000);
        let backend = format_memory_record(3, "Heap", 0x602000, 0x603000, None, &digest);
        let procfs = format_memory_record(
            3,
            "Heap",
            0x602000,
            0x603000,
            Some("READ | WRITE 0 0:0 0"),
            &digest,
        );
        assert_ne!(
            backend, procfs,
            "the detail field should still distinguish them"
        );

        let a = parse_memory_record(&backend).expect("backend record parses");
        let b = parse_memory_record(&procfs).expect("procfs record parses");
        assert_eq!(
            a, b,
            "the comparable fields must be identical:\n{backend}\n{procfs}"
        );
        assert_eq!(classify_memory_divergence(&a, &b), MemoryDivergence::None);
    }

    #[test]
    fn identical_region_and_bytes_produce_equal_digests() {
        // Both producers hash through the same `compute_hash_range`, so equality
        // here is the property the parity claim rests on. Bracketed against the
        // altered-byte case below so it is not a vacuous "everything is equal".
        let bytes: Vec<u8> = (0..0x1000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(Digest::new(&bytes), Digest::new(&bytes));
    }

    #[test]
    fn an_altered_byte_still_diverges() {
        // The refusal must not be a blanket "memory records always agree now".
        let mut bytes = vec![0u8; 0x1000];
        let base = Digest::new(&bytes);
        bytes[0x800] = 1;
        let altered = Digest::new(&bytes);
        assert_ne!(base, altered, "a single flipped byte must still be caught");

        let a = parse_memory_record(&format_memory_record(
            3, "Heap", 0x602000, 0x603000, None, &base,
        ))
        .unwrap();
        let b = parse_memory_record(&format_memory_record(
            3, "Heap", 0x602000, 0x603000, None, &altered,
        ))
        .unwrap();
        assert_eq!(
            classify_memory_divergence(&a, &b),
            MemoryDivergence::Content { size: 0x1000 },
            "equal domains with unequal digests is a CONTENT divergence"
        );
    }

    #[test]
    fn the_record1_domain_artifact_is_classified_as_domain_not_content() {
        // The exact case from the KVM heap decomposition: ptrace hashes the
        // page-granular VMA (0x1000) and KVM the unrounded break (0xd80). Both
        // regions are entirely zero -- the memory AGREES -- yet the digests
        // cannot match. Before the domain was recorded this was indistinguishable
        // from real corruption.
        let ptrace_digest = zeros(0x1000);
        let kvm_digest = zeros(0xd80);
        assert_ne!(
            ptrace_digest, kvm_digest,
            "premise: unequal-length zero runs hash differently, by construction"
        );

        let ptrace = parse_memory_record(&format_memory_record(
            3,
            "Heap",
            0x602000,
            0x603000,
            Some("READ | WRITE 0 0:0 0"),
            &ptrace_digest,
        ))
        .unwrap();
        let kvm = parse_memory_record(&format_memory_record(
            3,
            "Heap",
            0x602000,
            0x602d80,
            None,
            &kvm_digest,
        ))
        .unwrap();

        assert_eq!(
            classify_memory_divergence(&ptrace, &kvm),
            MemoryDivergence::Domain {
                left: 0x1000,
                right: 0xd80
            },
            "a size mismatch must report DOMAIN, never CONTENT -- the digests \
             differ but nothing is known about the bytes"
        );
    }

    #[test]
    fn the_two_producers_spell_the_kind_identically() {
        // `MMapPath::Heap` debug-prints `Heap`; `DetlogRegionKind::Heap` also
        // prints `Heap`, but the ptrace record used to carry `[heap]` from the
        // pathname instead. One token, both paths.
        assert_eq!(region_kind_token("Heap"), region_kind_token("heap"));
        assert_eq!(region_kind_token("Stack"), region_kind_token("stack"));
        assert_ne!(region_kind_token("Heap"), region_kind_token("Stack"));
        assert_eq!(region_kind_token("Anonymous"), "other");
    }

    #[test]
    fn comparing_different_kinds_is_refused_rather_than_scored() {
        let heap = parse_memory_record(&format_memory_record(
            3,
            "Heap",
            0x602000,
            0x603000,
            None,
            &zeros(0x1000),
        ))
        .unwrap();
        let stack = parse_memory_record(&format_memory_record(
            3,
            "Stack",
            0x7ffff000,
            0x80000000,
            None,
            &zeros(0x1000),
        ))
        .unwrap();
        assert!(matches!(
            classify_memory_divergence(&heap, &stack),
            MemoryDivergence::KindMismatch { .. }
        ));
    }

    #[test]
    fn a_pre_size_record_does_not_parse() {
        // An old log cannot state its domain. Returning None keeps that absence
        // visible instead of back-filling `end - start` and presenting an
        // inferred domain as a recorded one.
        let old = "[memory][dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap]->74b43faf";
        assert!(parse_memory_record(old).is_none());
    }

    #[test]
    fn an_inverted_range_reports_a_zero_domain() {
        let rec = format_memory_record(3, "Heap", 0x603000, 0x602000, None, &zeros(0));
        assert!(rec.contains("size=0x0"), "must not wrap: {rec}");
    }
}
