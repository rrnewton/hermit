/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A process's `smaps`, `smaps_rollup` and `numa_maps` rendered from the
//! backend's memory map (`reverie::Guest::memory_map`), where Detcore renders
//! them (`procfs::RenderedFiles::address_space`: where the guest's procfs is
//! not the kernel's, KVM). Each is the text Linux prints for the listed
//! mappings (`show_smap`, `__show_smap`, `show_smaps_rollup` and
//! `show_numa_map` in `fs/proc/task_mmu.c`, v6.17), which the sanitizer that
//! serves the kernel's file of the same name on the other backends then
//! normalizes (`procfs::sanitize_smaps`, `sanitize_smaps_rollup`,
//! `sanitize_numa_maps`). So for every mapping it lists, the guest reads the
//! bytes the sanitizer gives the kernel's file for that mapping on ptrace,
//! but for `ProtectionKey`, which follows the kernel serving the process
//! (below).
//!
//! A mapping's line of `maps` is its header in `smaps` ([`super::render`]'s,
//! from the memory map's facts). Linux prints more about it than the memory
//! map lists, and each such field is derived the same way on every backend,
//! from facts, or the file is refused by name:
//!
//! - `Size` is the range's; `KernelPageSize` and `MMUPageSize` are 4 kB for
//!   every mapping that is not hugetlbfs memory (`vma_kernel_pagesize`).
//! - The accounting fields count the pages the host holds; each sanitizer
//!   prints every one of them as `0 kB` (`procfs::SMAPS_ACCOUNTING_FIELDS`),
//!   and they are rendered as those of an untouched mapping, zero.
//! - `THPeligible` (`thp_vma_allowable_orders(vma, vma->vm_flags, TVA_SMAPS |
//!   TVA_ENFORCE_SYSFS, THP_ORDERS_ALL)`, include/linux/huge_mm.h and
//!   mm/huge_memory.c), from the mapping's flags and the host's settings
//!   (`thp_eligible`): for an anonymous mapping that no `MADV_HUGEPAGE`
//!   marked (Detcore answers every `MADV_HUGEPAGE` and `MADV_NOHUGEPAGE`
//!   itself and passes neither on, `syscalls::memory`, so none has `hg`), 1
//!   where the host's transparent huge pages allow an order ([`HostThp`]),
//!   the mapping has no `nh` (which `MAP_STACK` sets), the address space has
//!   not disabled them (`PR_SET_THP_DISABLE`, which Detcore passes to the
//!   backend and asks it about), and the mapping holds a naturally aligned
//!   block of that order, tried from the largest. For a file or shared
//!   memory, 0 where no order the host allows any kind of file
//!   ([`HostThp::file_orders`]) fits at the mapping's offset, and otherwise
//!   the file is refused: the answer then follows the kind of file and
//!   whether it is open for writing anywhere on the host. The host's
//!   settings are those the kernel beneath ptrace consults for the same
//!   answer, and the ones a KVM guest reads in
//!   `/sys/kernel/mm/transparent_hugepage`, read once as Detcore starts and
//!   logged at INFO ([`host_smaps_facts`]), so the value follows the host's
//!   settings exactly as ptrace's sanitized file does. Where the host lets an
//!   anonymous mapping have huge pages without `MADV_HUGEPAGE` (`always`),
//!   the value also follows the mapping's size and alignment, so KVM's
//!   8188 kB stack window, which holds an aligned 2 MiB block, prints 1 where
//!   a native-sized stack prints 0: the same layout difference as its range
//!   and `Size`.
//! - `ProtectionKey` is printed where the kernel serving the process enables
//!   protection keys (`arch_pkeys_enabled`, `cpu_feature_enabled(OSPKE)`): a
//!   fact of that kernel, not of the CPU the guest is shown (ptrace's guests
//!   are shown a virtual CPU without OSPKE, and the host kernel beneath them
//!   prints the line). Detcore asks that kernel, through the backend
//!   (`Detcore::serving_kernel_enables_protection_keys`). Where it enables
//!   none, as KVM's does (its vCPU runs with CR4.PKE clear, and LinuxEmu
//!   answers the protection-key calls as Linux does on such a CPU), the line
//!   is not printed, as Linux prints none. Where it enables them, the file is
//!   refused: a mapping's key then follows more calls than Detcore records
//!   (`mprotect(PROT_EXEC)` alone gives the execute-only key,
//!   `__arch_override_mprotect_pkey`), so a 0 could be wrong. The renderer
//!   still prints the line with the key 0 from facts that say keys are
//!   enabled and unused ([`SmapsFacts::protection_keys_enabled`]), as the host
//!   kernel does for the test process ([`host_protection_keys_enabled`]).
//! - `VmFlags`: the mapping's `vm_flags` as the backend reports them
//!   ([`MappingFact::vm_flags`], Linux 7.1's x86-64 bits), each set bit's
//!   mnemonic in the order of the bits (`show_smap_vma_flags`: `gu`, bit 11,
//!   before `lo`, bit 13, and `ac`, bit 20). A mapping whose flags the backend
//!   cannot establish (`None`: on KVM, for example, private anonymous memory
//!   whose VM_ACCOUNT an `mprotect(2)` that took write away may have cleared,
//!   or a file on a filesystem outside LinuxEmu's allowlist) refuses the file
//!   by name. The flags `madvise(2)` fork and dump advice and `mlock(2)` set
//!   (`dc`, `dd`, `wf`, `lo`, `lf`) are not among those the backend reports,
//!   so a map of an address space where such a call reached the backend for a
//!   listed mapping (`MemoryMetadata::mapping_flags_changed`) is refused; a
//!   call on a mapping the memory map does not list changes nothing the file
//!   shows. Access-pattern advice (`sr`, `rr`) and guard installs (`gu`) are
//!   the backend's to report. A host kernel that prints `sd` (VM_SOFTDIRTY,
//!   which no fact carries) refuses the file, as read once when Detcore
//!   starts ([`host_smaps_facts`]).
//!
//! `numa_maps` prints, for each mapping:
//!
//! - its policy: `default`. Detcore answers every memory-policy call itself,
//!   for one virtual NUMA node, and records no policy (`mbind` and
//!   `set_mempolicy` change nothing; `get_mempolicy` answers `MPOL_DEFAULT`),
//!   so the policy Linux prints, the mapping's or else the task's
//!   (`__get_vma_policy`, `get_task_policy`), is the default, which
//!   `mpol_to_str` prints as `default`;
//! - its name: `stack` for the initial stack, the only mapping it renders a
//!   row for (below);
//! - its page counts (`anon=`, `dirty=`, ..., `N<node>=`), which the sanitizer
//!   drops, rendered as none;
//! - `kernelpagesize_kB=4`, which Linux prints only for a mapping that holds
//!   at least one resident page (`if (!md->pages) goto out`). The initial
//!   stack does from exec on: its top page holds the arguments, the
//!   environment and the auxiliary vector exec wrote. Until advice that
//!   discards pages reaches it (`MADV_DONTNEED`, `MADV_DONTNEED_LOCKED`,
//!   `MADV_GUARD_INSTALL`, or `MADV_WIPEONFORK` for a later fork's copy:
//!   `MemoryMetadata::pages_discarded`): a thread running on another stack can
//!   discard all of it, and then nothing says whether a page came back, so
//!   `numa_maps` is refused. Whether any other mapping holds a page is the
//!   host's residency, which is no fact Detcore has, so `numa_maps` too
//!   refuses any mapping but the initial stack.
//!
//! `smaps_rollup` prints one header for the whole address space, from its
//! first mapping's start to its last one's end (`show_smaps_rollup`, the gate
//! area excluded, which is not in the address space's tree), and the sums of
//! the accounting fields, which the sanitizer prints as `0 kB`. Only a
//! complete memory map says where the address space starts: an incomplete
//! one (KVM's, where LinuxEmu does not know some page's mapping) is refused,
//! since a reader must
//! not infer from a missing entry that no mapping is there
//! (`reverie::MemoryMap::complete`).
//!
//! An incomplete list renders only the mappings it lists, as [`super::render`]
//! does for `maps`: the others are missing from the file, a named difference
//! of the backend (on KVM, <https://github.com/rrnewton/reverie/issues/1211>).
//! An incomplete list that lists nothing is refused.

use std::path::Path;

use reverie::MappingBacking;
use reverie::MappingFact;
use reverie::MemoryMap;

/// `SEQ_PUT_DEC`'s labels in `__show_smap` (fs/proc/task_mmu.c, v6.17), with
/// their padding and the width of the number after them, in Linux's order,
/// and whether a label is printed only in `smaps_rollup` (`rollup_mode`).
const ACCOUNTING: [(&str, usize, bool); 22] = [
    ("Rss:            ", 8, false),
    ("Pss:            ", 8, false),
    ("Pss_Dirty:      ", 8, false),
    ("Pss_Anon:       ", 8, true),
    ("Pss_File:       ", 8, true),
    ("Pss_Shmem:      ", 8, true),
    ("Shared_Clean:   ", 8, false),
    ("Shared_Dirty:   ", 8, false),
    ("Private_Clean:  ", 8, false),
    ("Private_Dirty:  ", 8, false),
    ("Referenced:     ", 8, false),
    ("Anonymous:      ", 8, false),
    ("KSM:            ", 8, false),
    ("LazyFree:       ", 8, false),
    ("AnonHugePages:  ", 8, false),
    ("ShmemPmdMapped: ", 8, false),
    ("FilePmdMapped:  ", 8, false),
    ("Shared_Hugetlb: ", 8, false),
    // `seq_put_decimal_ull_width(m, " kB\nPrivate_Hugetlb: ", ..., 7)`: one
    // column of the number's width goes to the longer label.
    ("Private_Hugetlb: ", 7, false),
    ("Swap:           ", 8, false),
    ("SwapPss:        ", 8, false),
    ("Locked:         ", 8, false),
];

/// The orders of transparent huge page an anonymous mapping can have
/// (`THP_ORDERS_ALL_ANON`: orders 2 to `PMD_ORDER`, 9 on x86-64 with 4 KiB
/// pages, include/linux/huge_mm.h in v6.17 and v7.1.3).
const THP_ORDERS_ALL_ANON: u64 = ((1 << 10) - 1) & !0b11;

/// The orders of transparent huge page a file or shared memory mapping can
/// have (`THP_ORDERS_ALL_FILE_DEFAULT`: orders 1 to `MAX_PAGECACHE_ORDER`,
/// which is `PMD_ORDER`, 9, on x86-64 with 4 KiB pages).
const THP_ORDERS_ALL_FILE_DEFAULT: u64 = ((1 << 10) - 1) & !0b1;

/// The PMD order, 9 on x86-64 with 4 KiB pages (`HPAGE_PMD_ORDER`).
const PMD_ORDER: u64 = 1 << 9;

/// `show_smap_vma_flags`' mnemonics (fs/proc/task_mmu.c, Linux 7.1) for
/// every bit a [`MappingFact::vm_flags`] may hold
/// ([`reverie::VM_KNOWN_FLAGS`]), in the order of their bits, the order Linux
/// prints them in.
const VM_FLAG_MNEMONICS: [(u64, &str); 19] = [
    (reverie::VM_READ, "rd"),
    (reverie::VM_WRITE, "wr"),
    (reverie::VM_EXEC, "ex"),
    (reverie::VM_SHARED, "sh"),
    (reverie::VM_MAYREAD, "mr"),
    (reverie::VM_MAYWRITE, "mw"),
    (reverie::VM_MAYEXEC, "me"),
    (reverie::VM_MAYSHARE, "ms"),
    (reverie::VM_GROWSDOWN, "gd"),
    (reverie::VM_MAYBE_GUARD, "gu"),
    (reverie::VM_LOCKED, "lo"),
    (reverie::VM_SEQ_READ, "sr"),
    (reverie::VM_RAND_READ, "rr"),
    (reverie::VM_ACCOUNT, "ac"),
    (reverie::VM_NORESERVE, "nr"),
    (reverie::VM_SYNC, "sf"),
    (reverie::VM_HUGEPAGE, "hg"),
    (reverie::VM_NOHUGEPAGE, "nh"),
    (reverie::VM_MERGEABLE, "mg"),
];

/// The `VmFlags` line Linux prints for a mapping whose `vm_flags` are
/// `flags` (`show_smap_vma_flags`): each set bit's mnemonic, in the order of
/// the bits, with the space after it. `None` where `flags` holds a bit this
/// renderer has no mnemonic for, which [`MappingFact::validate`] refuses.
fn vm_flags_line(flags: u64) -> Option<String> {
    let mut line = String::from("VmFlags: ");
    let mut named = 0;
    for (bit, mnemonic) in VM_FLAG_MNEMONICS {
        if flags & bit != 0 {
            line.push_str(mnemonic);
            line.push(' ');
            named |= bit;
        }
    }
    line.push('\n');
    (named == flags).then_some(line)
}

/// The size of a page, and of every page of a mapping that is not hugetlbfs
/// memory (`vma_kernel_pagesize`, `vma_mmu_pagesize`).
const PAGE_SIZE: u64 = 4096;

/// The top-level setting for shared memory and tmpfs
/// (`/sys/kernel/mm/transparent_hugepage/shmem_enabled`, `shmem_huge` in
/// mm/shmem.c).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShmemHuge {
    /// `always`, `within_size` or `advise`: each mount's own `huge=` option
    /// decides for a tmpfs file, and the per-size settings for anonymous
    /// shared memory.
    MountDecides,
    /// `never`.
    Never,
    /// `deny`: no shared memory has huge pages, whatever the mount says.
    Deny,
    /// `force`: every tmpfs file may have PMD-sized pages, and anonymous
    /// shared memory the per-size `inherit` orders alone.
    Force,
}

/// What the host kernel's transparent huge page settings allow a mapping, as
/// `/sys/kernel/mm/transparent_hugepage` shows them: the inputs of
/// `thp_vma_allowable_orders`'s mask with `TVA_ENFORCE_SYSFS` for anonymous
/// memory, and of `shmem_allowable_huge_orders` for shared memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostThp {
    /// Whether the global setting (`enabled`) is `always`
    /// (`hugepage_global_always`).
    pub(crate) global_always: bool,
    /// The orders whose own setting (`hugepages-<size>kB/enabled`) is
    /// `always` (`huge_anon_orders_always`).
    pub(crate) always: u64,
    /// The orders whose own setting is `inherit`
    /// (`huge_anon_orders_inherit`): the PMD order alone where the kernel has
    /// no per-size settings.
    pub(crate) inherit: u64,
    /// The top-level shared memory setting (`shmem_enabled`); `Never` where
    /// the kernel has none (no tmpfs huge pages: before Linux 4.8, or
    /// without `CONFIG_SHMEM`).
    pub(crate) shmem: ShmemHuge,
    /// The orders whose own shared memory setting
    /// (`hugepages-<size>kB/shmem_enabled`) is `always`
    /// (`huge_shmem_orders_always`).
    pub(crate) shmem_always: u64,
    /// ... is `within_size` (`huge_shmem_orders_within_size`).
    pub(crate) shmem_within_size: u64,
    /// ... is `inherit` (`huge_shmem_orders_inherit`): the PMD order alone
    /// where the kernel has no per-size shared memory settings, as anonymous
    /// shared memory then followed the top-level setting at the PMD size.
    pub(crate) shmem_inherit: u64,
    /// ... is `advise` (`huge_shmem_orders_madvise`), which only
    /// `MADV_HUGEPAGE` (`VM_HUGEPAGE`) brings in.
    pub(crate) shmem_advise: u64,
}

impl HostThp {
    /// The orders an anonymous mapping no `MADV_HUGEPAGE` marked may have
    /// (`thp_vma_allowable_orders`): those set to `always`, and those set to
    /// `inherit` where the global setting is `always`.
    pub(crate) fn anonymous_orders(&self) -> u64 {
        let mut orders = self.always;
        if self.global_always {
            orders |= self.inherit;
        }
        orders & THP_ORDERS_ALL_ANON
    }

    /// Every order a file or shared memory mapping no `MADV_HUGEPAGE` marked
    /// may have on this host, whatever its file (`__thp_vma_allowable_orders`
    /// and `shmem_allowable_huge_orders`, mm/huge_memory.c and mm/shmem.c in
    /// v6.17 and v7.1): the union, over the three kinds of file, of
    ///
    /// - a regular file: every file order where the global setting is
    ///   `always`, and none otherwise (`hugepage_global_always`);
    /// - a tmpfs file with a name: at most the PMD order
    ///   (`shmem_huge_global_enabled`), none under `deny`;
    /// - anonymous shared memory (shared anonymous memory, a memfd, an
    ///   unlinked tmpfs file): none under `deny`, the per-size `inherit`
    ///   orders under `force`, and otherwise those set to `always`,
    ///   `within_size` or `inherit`.
    ///
    /// Which of the three a mapping is, and whether the file is open for
    /// writing (`file_thp_enabled`) or how its mount was made, is not in the
    /// memory map, so a renderer takes the union: where no order of it fits
    /// a mapping, Linux prints 0 for it, whichever its file is.
    pub(crate) fn file_orders(&self) -> u64 {
        let mut orders = 0;
        if self.global_always {
            orders |= THP_ORDERS_ALL_FILE_DEFAULT;
        }
        match self.shmem {
            ShmemHuge::Deny => {}
            ShmemHuge::Force => orders |= PMD_ORDER | self.shmem_inherit,
            ShmemHuge::MountDecides | ShmemHuge::Never => {
                orders |=
                    PMD_ORDER | self.shmem_always | self.shmem_within_size | self.shmem_inherit
            }
        }
        orders & THP_ORDERS_ALL_FILE_DEFAULT
    }

    /// The settings under `root`, the kernel's
    /// `/sys/kernel/mm/transparent_hugepage`: `Ok(None)` where it does not
    /// exist, a kernel without transparent huge pages or on hardware that has
    /// none (`hugepage_init` makes no directory then), where every mapping's
    /// answer is 0. `Err` names a setting that could not be read or parsed.
    pub(crate) fn read(root: &Path) -> Result<Option<Self>, String> {
        let global = match std::fs::read(root.join("enabled")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !root.exists() => {
                return Ok(None);
            }
            Err(error) => return Err(format!("{}/enabled: {error}", root.display())),
        };
        let global = selected(&global)
            .ok_or_else(|| format!("{}/enabled has no selected setting", root.display()))?;
        let shmem_path = root.join("shmem_enabled");
        let shmem = match std::fs::read(&shmem_path) {
            Ok(text) => match selected(&text) {
                Some("always" | "within_size" | "advise") => ShmemHuge::MountDecides,
                Some("never") => ShmemHuge::Never,
                Some("deny") => ShmemHuge::Deny,
                Some("force") => ShmemHuge::Force,
                _ => {
                    return Err(format!(
                        "{} has no setting Linux gives",
                        shmem_path.display()
                    ));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ShmemHuge::Never,
            Err(error) => return Err(format!("{}: {error}", shmem_path.display())),
        };
        let mut thp = HostThp {
            global_always: global == "always",
            always: 0,
            inherit: 0,
            shmem,
            shmem_always: 0,
            shmem_within_size: 0,
            shmem_inherit: 0,
            shmem_advise: 0,
        };
        let mut per_size_shmem = false;
        // Every size directory, `hugepages-<size>kB`, listed with raw
        // `getdents64` (`util::find_in_directory`), in the order of size.
        let mut sizes = Vec::new();
        crate::util::find_in_directory(root, |name| {
            if let Some(kilobytes) = name
                .to_str()
                .and_then(|name| name.strip_prefix("hugepages-"))
                .and_then(|name| name.strip_suffix("kB"))
                .and_then(|size| size.parse::<u64>().ok())
            {
                sizes.push(kilobytes);
            }
            None::<()>
        })
        .map_err(|error| format!("{}: {error}", root.display()))?;
        sizes.sort_unstable();
        let per_size = !sizes.is_empty();
        for kilobytes in sizes {
            let directory = root.join(format!("hugepages-{kilobytes}kB"));
            let pages = kilobytes * 1024 / PAGE_SIZE;
            if !pages.is_power_of_two() || pages < 2 {
                return Err(format!("{}: an unexpected size", directory.display()));
            }
            let order = pages.trailing_zeros();
            // A size's shared memory setting, which the kernel has from Linux
            // 6.11 for every file order (order 1, `hugepages-8kB`, has only
            // this one).
            let path = directory.join("shmem_enabled");
            match std::fs::read(&path) {
                Ok(text) => {
                    per_size_shmem = true;
                    match selected(&text) {
                        Some("always") => thp.shmem_always |= 1 << order,
                        Some("within_size") => thp.shmem_within_size |= 1 << order,
                        Some("inherit") => thp.shmem_inherit |= 1 << order,
                        Some("advise") => thp.shmem_advise |= 1 << order,
                        Some("never") => {}
                        _ => return Err(format!("{} has no setting Linux gives", path.display())),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
            let path = directory.join("enabled");
            // A size anonymous memory cannot have (order 1, which only files
            // and shared memory use) has no `enabled`.
            let text = match std::fs::read(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            };
            match selected(&text) {
                Some("always") => thp.always |= 1 << order,
                Some("inherit") => thp.inherit |= 1 << order,
                Some("madvise" | "never") => {}
                _ => return Err(format!("{} has no setting Linux gives", path.display())),
            }
        }
        if !per_size {
            // Before per-size settings, the PMD size followed the global
            // setting alone (`huge_anon_orders_inherit`'s default).
            thp.inherit = PMD_ORDER;
        }
        if !per_size_shmem {
            // Before per-size shared memory settings, anonymous shared memory
            // followed the top-level setting at the PMD size alone.
            thp.shmem_inherit = PMD_ORDER;
        }
        Ok(Some(thp))
    }
}

/// The facts about the host kernel a rendered `smaps` derives fields from:
/// its transparent huge page settings ([`HostThp::read`] of
/// `/sys/kernel/mm/transparent_hugepage`) and whether it prints `sd`
/// ([`soft_dirty_printed`]). Each is `Err` naming what could not be read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostSmapsFacts {
    /// The transparent huge page settings; `None` where the kernel has no
    /// transparent huge pages.
    pub(crate) thp: Result<Option<HostThp>, String>,
    /// Whether the kernel prints `sd` (VM_SOFTDIRTY) on mappings.
    pub(crate) soft_dirty: Result<bool, String>,
}

/// The host's facts ([`HostSmapsFacts`]), read once per tool process, the
/// first time they are asked for, and logged then, once, at INFO, so that a
/// `--verify-strict` comparison of two runs names a difference in them at
/// that line, and every `smaps` a run renders takes the same answer even if
/// the host's administrator changes them while it runs. Detcore asks for
/// them as it starts, before the guest's first instruction, wherever it
/// renders a process's `smaps` ([`init_host_smaps_facts`]); no render reads
/// the host.
pub(crate) fn host_smaps_facts() -> HostSmapsFacts {
    static HOST: std::sync::OnceLock<HostSmapsFacts> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let facts = HostSmapsFacts {
            thp: HostThp::read(Path::new("/sys/kernel/mm/transparent_hugepage")),
            soft_dirty: soft_dirty_printed(),
        };
        tracing::info!(
            "[detcore] host facts a rendered smaps derives fields from, fixed for the run: \
             transparent huge pages {:?}; the kernel prints sd: {:?}",
            facts.thp,
            facts.soft_dirty,
        );
        facts
    })
    .clone()
}

/// Read the host's facts ([`host_smaps_facts`]) now, as Detcore starts,
/// before the guest runs.
pub(crate) fn init_host_smaps_facts() {
    let _ = host_smaps_facts();
}

/// Whether the host kernel prints `sd` (VM_SOFTDIRTY) in `smaps`: a kernel
/// built with `CONFIG_MEM_SOFT_DIRTY` sets the flag on every new mapping
/// (`mmap_region`, `INIT_VM_FLAG`), and only clearing the soft-dirty bits
/// (`clear_refs`) takes it off again. Read from this tool process's own
/// `smaps`, which runs on that kernel: true where any of its mappings has the
/// flag. Mappings carry no `sd` in [`MappingFact::vm_flags`], so where the
/// host prints it a rendered `smaps` is refused.
fn soft_dirty_printed() -> Result<bool, String> {
    let text =
        std::fs::read("/proc/self/smaps").map_err(|error| format!("/proc/self/smaps: {error}"))?;
    let text = std::str::from_utf8(&text).map_err(|error| format!("/proc/self/smaps: {error}"))?;
    let mut flags_lines = text
        .lines()
        .filter_map(|line| line.strip_prefix("VmFlags:"))
        .peekable();
    if flags_lines.peek().is_none() {
        return Err("/proc/self/smaps lists no VmFlags".to_string());
    }
    Ok(flags_lines.any(|flags| flags.split_whitespace().any(|flag| flag == "sd")))
}

/// The word a sysfs choice file selects, the one in brackets, as
/// `always [madvise] never` selects `madvise`.
fn selected(text: &[u8]) -> Option<&str> {
    std::str::from_utf8(text)
        .ok()?
        .split_whitespace()
        .find_map(|word| {
            word.strip_prefix('[')
                .and_then(|word| word.strip_suffix(']'))
        })
}

/// Whether the host kernel prints `ProtectionKey` in `smaps`
/// (`arch_pkeys_enabled`): on x86-64, whether it enabled the CPU's
/// protection keys, which the OSPKE bit of CPUID leaf 7 reports (it mirrors
/// CR4.PKE), read in the test's own process, which runs on the host kernel.
/// Only for the test that compares the renderer with that kernel's file: a
/// guest's answer comes from the kernel serving it (the module's docs).
/// `None` on another architecture.
#[cfg(test)]
pub(crate) fn host_protection_keys_enabled() -> Option<bool> {
    #[cfg(target_arch = "x86_64")]
    {
        // Leaf 7 is read only where leaf 0 says the CPU has it.
        if std::arch::x86_64::__get_cpuid_max(0).0 < 7 {
            return Some(false);
        }
        Some(std::arch::x86_64::__cpuid_count(7, 0).ecx & (1 << 4) != 0)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// What, beyond the memory map, the derived fields of a process's `smaps`
/// need (the module's docs say which field takes which).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SmapsFacts {
    /// Whether Linux prints `ProtectionKey`: whether the kernel serving the
    /// process enables protection keys (the module's docs). Never true for a
    /// guest, whose file is refused there; true for the test that compares
    /// the renderer with the host kernel's file.
    pub(crate) protection_keys_enabled: bool,
    /// Whether the address space allocated or applied a protection key
    /// (`MemoryMetadata::protection_keys`).
    pub(crate) protection_keys_used: bool,
    /// Whether a call that sets mapping flags the memory map does not list
    /// reached the backend for the address space
    /// (`MemoryMetadata::mapping_flags_changed`).
    pub(crate) mapping_flags_changed: bool,
    /// The orders of transparent huge page an anonymous mapping may have
    /// ([`HostThp::anonymous_orders`]), none where the host has none.
    pub(crate) thp_orders: u64,
    /// The orders of transparent huge page a file or shared memory mapping
    /// may have, whatever its file ([`HostThp::file_orders`]), none where the
    /// host has none.
    pub(crate) file_thp_orders: u64,
    /// Whether the address space disabled transparent huge pages
    /// (`PR_GET_THP_DISABLE`), where [`Self::thp_orders`] or
    /// [`Self::file_thp_orders`] makes it matter; `None` where it was not
    /// asked because no order is allowed.
    pub(crate) thp_disabled: Option<bool>,
    /// Whether the host kernel prints `sd` (VM_SOFTDIRTY) on mappings
    /// ([`soft_dirty_printed`]), which no fact carries.
    pub(crate) soft_dirty_printed: bool,
}

/// Whether the mapping `[start, end)`, whose file offset in pages is
/// `pgoff` where it maps a file or shared memory (`None` for anonymous
/// memory, whose offset Linux does not check), holds a naturally aligned
/// block of one of `orders`, tried from the largest, as Linux checks it for
/// `smaps` (`__thp_vma_allowable_orders`: `thp_vma_suitable_order` at
/// `vm_end` less the block's size). A file mapping's block must also lie at
/// a file offset aligned to its size: `vm_start >> PAGE_SHIFT` less
/// `vm_pgoff`, aligned to the block's pages.
fn thp_fits(orders: u64, start: u64, end: u64, pgoff: Option<u64>) -> bool {
    (1..=9)
        .rev()
        .filter(|order| orders & (1 << order) != 0)
        .any(|order| {
            let size = PAGE_SIZE << order;
            let file_aligned = pgoff.is_none_or(|pgoff| {
                (start / PAGE_SIZE).wrapping_sub(pgoff) & ((1 << order) - 1) == 0
            });
            file_aligned
                && end.checked_sub(size).is_some_and(|address| {
                    let aligned = address & !(size - 1);
                    aligned >= start && aligned + size <= end
                })
        })
}

/// Linux's `THPeligible` for `mapping`, whose `vm_flags` are `flags`
/// (`__thp_vma_allowable_orders(vma, vma->vm_flags, TVA_SMAPS |
/// TVA_ENFORCE_SYSFS, THP_ORDERS_ALL)`, mm/huge_memory.c in v6.17 and v7.1),
/// with the host's and the address space's settings from `facts`, or the
/// refusal that names why it is not a fact:
///
/// - a mapping `MADV_HUGEPAGE` marked (`hg`) is refused: its answer follows
///   settings the rule below leaves out. Detcore answers that advice itself
///   and never passes it on, so no mapping has the flag under Hermit;
/// - anonymous memory (unnamed or named anonymous memory, the heap, the
///   stack): 1 where an order the host allows anonymous memory
///   ([`HostThp::anonymous_orders`]) fits ([`thp_fits`]), the mapping has no
///   `nh` (VM_NOHUGEPAGE, which `MAP_STACK` sets), and the address space has
///   not disabled huge pages (`vma_thp_disabled`);
/// - a file or shared memory: 0 where no order of [`HostThp::file_orders`]
///   fits at the mapping's offset, or where `nh` is set or the address
///   space disabled huge pages; otherwise refused, as whether Linux gives it
///   huge pages then depends on which kind of file it maps, whether that file
///   is open for writing anywhere on the host (`file_thp_enabled`) and how
///   its mount was made, none of which the memory map says;
/// - any other mapping is refused.
fn thp_eligible(mapping: &MappingFact, flags: u64, facts: &SmapsFacts) -> Result<bool, Refusal> {
    let refused = |why: &str| {
        Err(format!(
            "Linux's smaps prints whether {} may have transparent huge pages, and {why}",
            named(mapping)
        ))
    };
    if flags & reverie::VM_HUGEPAGE != 0 {
        return refused("MADV_HUGEPAGE marked it, which Detcore does not derive the answer for");
    }
    let (orders, pgoff) = match &mapping.backing {
        MappingBacking::Anonymous
        | MappingBacking::Heap
        | MappingBacking::Stack
        | MappingBacking::NamedAnonymous(_) => (facts.thp_orders, None),
        MappingBacking::File { .. } | MappingBacking::NamedSharedAnonymous { .. } => {
            (facts.file_thp_orders, Some(mapping.offset / PAGE_SIZE))
        }
        _ => return refused("Detcore does not derive that for a mapping of its kind"),
    };
    if !thp_fits(orders, mapping.start, mapping.end, pgoff) || flags & reverie::VM_NOHUGEPAGE != 0 {
        return Ok(false);
    }
    let Some(disabled) = facts.thp_disabled else {
        return refused("whether the address space disabled them was not asked");
    };
    match (pgoff, disabled) {
        (_, true) => Ok(false),
        (None, false) => Ok(true),
        (Some(_), false) => refused(
            "the host's settings let a file or shared memory mapping have them where it \
             holds an aligned block, so the answer depends on the kind of file it maps, \
             whether that file is open for writing and how its mount was made, which the \
             backend's memory map does not say",
        ),
    }
}

/// A named refusal of a whole file: why Detcore cannot render it.
pub(crate) type Refusal = String;

/// The range and name of `mapping`, as the reason of a refusal names it.
fn named(mapping: &MappingFact) -> String {
    let mut line = Vec::new();
    match super::line(mapping, &mut line) {
        Some(()) => String::from_utf8_lossy(&line).trim_end().to_string(),
        None => format!("{:08x}-{:08x}", mapping.start, mapping.end),
    }
}

/// Refuse an incomplete list that lists nothing: KVM's, once LinuxEmu has
/// lost track of the stack window ([`super::render`]).
fn refuse_empty_incomplete(map: &MemoryMap, file: &str) -> Result<(), Refusal> {
    if map.mappings.is_empty() && !map.complete {
        Err(format!(
            "Detcore renders a process's {file} from the backend's memory map, which lists \
             only some of its mappings and at this point none"
        ))
    } else {
        Ok(())
    }
}

/// Whether `mapping` is the kernel's gate area, which lies above the user
/// address space (`[vsyscall]` on x86-64) and is no mapping of the address
/// space's tree: kernel addresses have their top bit set.
fn is_gate_area(mapping: &MappingFact) -> bool {
    mapping.start >= 1 << 63
}

/// The text of a process's `smaps` for `map`, as Linux prints it
/// (`show_smap`), with the fields the memory map does not list derived from
/// `facts`, or the refusal that names why Detcore cannot give one of them.
pub(crate) fn render_smaps(map: &MemoryMap, facts: &SmapsFacts) -> Result<Vec<u8>, Refusal> {
    refuse_empty_incomplete(map, "smaps")?;
    let mut out = Vec::new();
    for mapping in &map.mappings {
        if facts.mapping_flags_changed {
            return Err(format!(
                "Linux's smaps prints the flags it keeps on each mapping, and madvise \
                 advice or mlock that reached the backend may have set flags on {} that \
                 the backend's memory map does not list",
                named(mapping)
            ));
        }
        if facts.protection_keys_enabled && facts.protection_keys_used {
            return Err(format!(
                "Linux's smaps prints each mapping's protection key, and the address space \
                 allocated or applied one, which the backend's memory map does not list \
                 for {}",
                named(mapping)
            ));
        }
        let Some(flags) = mapping.vm_flags else {
            return Err(format!(
                "Linux's smaps prints the flags it keeps on each mapping, and for {} the \
                 backend's memory map does not establish them",
                named(mapping)
            ));
        };
        if facts.soft_dirty_printed {
            return Err(format!(
                "Linux's smaps prints the flags it keeps on each mapping, and this host's \
                 kernel prints the soft-dirty flag (sd), which the backend's memory map does \
                 not list for {}",
                named(mapping)
            ));
        }
        let vm_flags = vm_flags_line(flags).ok_or_else(|| {
            format!(
                "the backend's memory map gives {} a flag Detcore cannot print: {flags:#x}",
                named(mapping)
            )
        })?;
        let eligible = thp_eligible(mapping, flags, facts)?;
        super::line(mapping, &mut out).ok_or_else(|| {
            format!(
                "the backend's memory map lists a mapping Detcore cannot print: {:08x}-{:08x}",
                mapping.start, mapping.end
            )
        })?;
        let kilobytes = (mapping.end - mapping.start) >> 10;
        out.extend_from_slice(
            format!(
                "Size:           {kilobytes:>8} kB\n\
                 KernelPageSize: {:>8} kB\n\
                 MMUPageSize:    {:>8} kB\n",
                PAGE_SIZE >> 10,
                PAGE_SIZE >> 10,
            )
            .as_bytes(),
        );
        accounting(&mut out, false);
        out.extend_from_slice(format!("THPeligible:    {:>8}\n", u8::from(eligible)).as_bytes());
        if facts.protection_keys_enabled {
            out.extend_from_slice(format!("ProtectionKey:  {:>8}\n", 0).as_bytes());
        }
        out.extend_from_slice(vm_flags.as_bytes());
    }
    Ok(out)
}

/// Append `__show_smap`'s accounting fields, each zero, those of
/// `smaps_rollup` too where `rollup`.
fn accounting(out: &mut Vec<u8>, rollup: bool) {
    for (label, width, rollup_only) in ACCOUNTING {
        if rollup_only && !rollup {
            continue;
        }
        out.extend_from_slice(format!("{label}{:>width$} kB\n", 0).as_bytes());
    }
}

/// Why Detcore cannot render a process's `smaps_rollup` from `map`, if it
/// cannot: an incomplete list does not say where the address space starts.
/// The one rule [`render_smaps_rollup`] refuses by, and the one a listing
/// of the process's directory leaves the name out by
/// (`Detcore::task_placeholder_snapshot`), so that a listing never names a
/// `smaps_rollup` its read refuses (dh-62azc).
pub(crate) fn smaps_rollup_refusal(map: &MemoryMap) -> Option<Refusal> {
    (!map.complete).then(|| {
        "Linux's smaps_rollup gives the range from the address space's first mapping to \
         its last one, and the backend's memory map lists only some of its mappings"
            .to_string()
    })
}

/// The text of a process's `smaps_rollup` for `map`, as Linux prints it
/// (`show_smaps_rollup`), or the refusal that names why Detcore cannot give
/// it ([`smaps_rollup_refusal`]).
pub(crate) fn render_smaps_rollup(map: &MemoryMap) -> Result<Vec<u8>, Refusal> {
    if let Some(refusal) = smaps_rollup_refusal(map) {
        return Err(refusal);
    }
    let mut own = map.mappings.iter().filter(|mapping| !is_gate_area(mapping));
    let first = own.next();
    let (start, end) = match first {
        Some(first) => (
            first.start,
            own.next_back().map_or(first.end, |last| last.end),
        ),
        // `vma_start` and `last_vma_end` stay 0 for an empty tree.
        None => (0, 0),
    };
    let header = format!("{start:08x}-{end:08x} ---p 00000000 00:00 0 ");
    let mut out = header.clone().into_bytes();
    // `seq_pad`: the column `show_vma_header_prefix` set, then one space.
    out.extend(std::iter::repeat_n(
        b' ',
        super::NAME_COLUMN.saturating_sub(header.len()) + 1,
    ));
    out.extend_from_slice(b"[rollup]\n");
    accounting(&mut out, true);
    Ok(out)
}

/// The text of a process's `numa_maps` for `map`, as Linux prints it
/// (`show_numa_map`, the module's docs), or the refusal that names why
/// Detcore cannot give a row: in an address space where advice that discards
/// pages reached a listed mapping (`pages_discarded`,
/// `MemoryMetadata::pages_discarded`), whether it still holds a resident
/// page, which decides `kernelpagesize_kB`, is not a fact.
pub(crate) fn render_numa_maps(map: &MemoryMap, pages_discarded: bool) -> Result<Vec<u8>, Refusal> {
    refuse_empty_incomplete(map, "numa_maps")?;
    let mut out = Vec::new();
    for mapping in &map.mappings {
        // The gate area has no memory map of its own: no row (`if (!mm)`).
        if is_gate_area(mapping) {
            continue;
        }
        if !matches!(mapping.backing, MappingBacking::Stack) {
            return Err(format!(
                "Linux's numa_maps says of each mapping whether it holds a resident page, \
                 and for {} that is the host's residency, not in the backend's memory map",
                named(mapping)
            ));
        }
        if pages_discarded {
            return Err(format!(
                "Linux's numa_maps says of each mapping whether it holds a resident page, and \
                 madvise advice that discards pages reached the backend for {}, so whether it \
                 still holds one is not a fact",
                named(mapping)
            ));
        }
        out.extend_from_slice(
            format!(
                "{:08x} default stack kernelpagesize_kB={}\n",
                mapping.start,
                PAGE_SIZE >> 10
            )
            .as_bytes(),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// The `[stack]` block of `/proc/self/smaps` a native process printed on
    /// this repository's Linux 7.1.3 test host (transparent huge pages
    /// `madvise`, 2048 kB `inherit`, every other size `never`; OSPKE set).
    const LINUX_STACK: &str = "\
        7ffcec80f000-7ffcec833000 rw-p 00000000 00:00 0                          [stack]\n\
        Size:                144 kB\n\
        KernelPageSize:        4 kB\n\
        MMUPageSize:           4 kB\n\
        Rss:                  24 kB\n\
        Pss:                  24 kB\n\
        Pss_Dirty:            24 kB\n\
        Shared_Clean:          0 kB\n\
        Shared_Dirty:          0 kB\n\
        Private_Clean:         0 kB\n\
        Private_Dirty:        24 kB\n\
        Referenced:           24 kB\n\
        Anonymous:            24 kB\n\
        KSM:                   0 kB\n\
        LazyFree:              0 kB\n\
        AnonHugePages:         0 kB\n\
        ShmemPmdMapped:        0 kB\n\
        FilePmdMapped:         0 kB\n\
        Shared_Hugetlb:        0 kB\n\
        Private_Hugetlb:       0 kB\n\
        Swap:                  0 kB\n\
        SwapPss:               0 kB\n\
        Locked:                0 kB\n\
        THPeligible:           0\n\
        ProtectionKey:         0\n\
        VmFlags: rd wr mr mw me gd ac \n";

    /// The facts of that host, for an address space that set no flag and no
    /// key.
    const HOST: SmapsFacts = SmapsFacts {
        protection_keys_enabled: true,
        protection_keys_used: false,
        mapping_flags_changed: false,
        thp_orders: 0,
        file_thp_orders: 0,
        thp_disabled: None,
        soft_dirty_printed: false,
    };

    /// `VM_READ`, `VM_WRITE` and `VM_EXEC` as `prot` says.
    fn access(prot: libc::c_int) -> u64 {
        [
            (libc::PROT_READ, reverie::VM_READ),
            (libc::PROT_WRITE, reverie::VM_WRITE),
            (libc::PROT_EXEC, reverie::VM_EXEC),
        ]
        .into_iter()
        .filter(|(bit, _)| prot & bit != 0)
        .fold(0, |flags, (_, flag)| flags | flag)
    }

    /// `VM_MAYREAD | VM_MAYWRITE | VM_MAYEXEC`.
    const MAY: u64 = reverie::VM_MAYREAD | reverie::VM_MAYWRITE | reverie::VM_MAYEXEC;

    /// The initial stack with protection `prot` and the flags exec leaves it
    /// with (`VM_STACK_FLAGS`: `mr mw me gd ac` beside the protection's).
    fn stack(start: u64, end: u64, prot: libc::c_int) -> MappingFact {
        MappingFact::new(start, end, prot as u32, false, 0, MappingBacking::Stack)
            .unwrap()
            .with_vm_flags(access(prot) | MAY | reverie::VM_GROWSDOWN | reverie::VM_ACCOUNT)
            .unwrap()
    }

    /// A mapping with `backing`, `prot`, `shared`, `offset` and `vm_flags`
    /// beside the protection's own bits.
    fn mapping(
        (start, end): (u64, u64),
        prot: libc::c_int,
        shared: bool,
        offset: u64,
        backing: MappingBacking,
        flags: u64,
    ) -> MappingFact {
        MappingFact::new(start, end, prot as u32, shared, offset, backing)
            .unwrap()
            .with_vm_flags(access(prot) | flags)
            .unwrap()
    }

    /// A file's backing, `/f` on device 0:2a.
    fn file() -> MappingBacking {
        MappingBacking::File {
            raw_device: libc::makedev(0, 0x2a),
            raw_inode: 17,
            path: b"/f".to_vec(),
        }
    }

    fn read_write() -> libc::c_int {
        libc::PROT_READ | libc::PROT_WRITE
    }

    /// The `[stack]` block renders as Linux printed it, but for the
    /// accounting fields, which count the host's pages; through the
    /// sanitizer the two are the same bytes.
    #[test]
    fn the_stack_renders_as_linux_prints_it() {
        let map = MemoryMap::new(
            vec![stack(0x7ffc_ec80_f000, 0x7ffc_ec83_3000, read_write())],
            false,
        )
        .unwrap();
        let rendered = render_smaps(&map, &HOST).unwrap();
        assert_eq!(
            String::from_utf8(rendered.clone()).unwrap(),
            "\
            7ffcec80f000-7ffcec833000 rw-p 00000000 00:00 0                          [stack]\n\
            Size:                144 kB\n\
            KernelPageSize:        4 kB\n\
            MMUPageSize:           4 kB\n\
            Rss:                   0 kB\n\
            Pss:                   0 kB\n\
            Pss_Dirty:             0 kB\n\
            Shared_Clean:          0 kB\n\
            Shared_Dirty:          0 kB\n\
            Private_Clean:         0 kB\n\
            Private_Dirty:         0 kB\n\
            Referenced:            0 kB\n\
            Anonymous:             0 kB\n\
            KSM:                   0 kB\n\
            LazyFree:              0 kB\n\
            AnonHugePages:         0 kB\n\
            ShmemPmdMapped:        0 kB\n\
            FilePmdMapped:         0 kB\n\
            Shared_Hugetlb:        0 kB\n\
            Private_Hugetlb:       0 kB\n\
            Swap:                  0 kB\n\
            SwapPss:               0 kB\n\
            Locked:                0 kB\n\
            THPeligible:           0\n\
            ProtectionKey:         0\n\
            VmFlags: rd wr mr mw me gd ac \n"
        );
        let table = BTreeMap::new();
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_smaps(&rendered, &table)).unwrap(),
            String::from_utf8(crate::procfs::sanitize_smaps(
                LINUX_STACK.as_bytes(),
                &table
            ))
            .unwrap()
        );
    }

    /// Each accounting field, in Linux's order and only those, survives the
    /// sanitizer as `0 kB`; dropping one changes the sanitized bytes.
    #[test]
    fn every_accounting_field_is_rendered_in_linuxs_order() {
        let map =
            MemoryMap::new(vec![stack(0x7000_0000, 0x7002_1000, read_write())], false).unwrap();
        let sanitized =
            crate::procfs::sanitize_smaps(&render_smaps(&map, &HOST).unwrap(), &BTreeMap::new());
        let labels: Vec<&str> = std::str::from_utf8(&sanitized)
            .unwrap()
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':').map(|(label, _)| label))
            .collect();
        let expected: Vec<&str> = ["Size", "KernelPageSize", "MMUPageSize"]
            .into_iter()
            .chain(
                ACCOUNTING
                    .iter()
                    .filter(|(_, _, rollup_only)| !rollup_only)
                    .map(|(label, _, _)| label.split(':').next().unwrap()),
            )
            .chain(["THPeligible", "ProtectionKey", "VmFlags"])
            .collect();
        assert_eq!(labels, expected);
        assert_eq!(
            ACCOUNTING
                .iter()
                .map(|(label, _, _)| label.split(':').next().unwrap())
                .collect::<Vec<_>>(),
            crate::procfs::SMAPS_ACCOUNTING_FIELDS
        );
    }

    /// The stack's flags follow its protection; `ProtectionKey` is printed
    /// only where the host enables keys.
    #[test]
    fn the_stack_flags_follow_its_protection() {
        let rendered = |prot, facts: &SmapsFacts| {
            let map = MemoryMap::new(vec![stack(0x7000_0000, 0x7002_1000, prot)], false).unwrap();
            String::from_utf8(render_smaps(&map, facts).unwrap()).unwrap()
        };
        assert!(rendered(read_write(), &HOST).ends_with(
            "THPeligible:           0\nProtectionKey:         0\nVmFlags: rd wr mr mw me gd ac \n"
        ));
        assert!(
            rendered(libc::PROT_READ | libc::PROT_EXEC, &HOST)
                .ends_with("VmFlags: rd ex mr mw me gd ac \n")
        );
        assert!(rendered(libc::PROT_NONE, &HOST).ends_with("VmFlags: mr mw me gd ac \n"));
        let no_keys = SmapsFacts {
            protection_keys_enabled: false,
            ..HOST
        };
        let text = rendered(read_write(), &no_keys);
        assert!(!text.contains("ProtectionKey"), "{text}");
        assert!(text.ends_with("THPeligible:           0\nVmFlags: rd wr mr mw me gd ac \n"));
    }

    /// `thp_fits`: a block of an allowed order must lie whole within the
    /// mapping, naturally aligned, and for a file at an offset aligned to
    /// its size; orders are tried from the largest.
    #[test]
    fn thp_fits_takes_an_aligned_block_of_an_allowed_order() {
        let pmd = PMD_ORDER;
        let two_mib = 2 << 20;
        // The block must lie whole within the mapping.
        assert!(!thp_fits(
            pmd,
            0x7000_0000,
            0x7000_0000 + two_mib - 4096,
            None
        ));
        assert!(thp_fits(pmd, 0x7000_0000, 0x7000_0000 + two_mib, None));
        assert!(!thp_fits(
            pmd,
            0x7000_1000,
            0x7000_0000 + two_mib + 4096,
            None
        ));
        assert!(thp_fits(pmd, 0x7000_1000, 0x7000_0000 + 2 * two_mib, None));
        assert!(!thp_fits(0, 0x7000_0000, 0x7000_0000 + two_mib, None));
        // A smaller order fits where the PMD one does not.
        assert!(thp_fits(pmd | 1 << 4, 0x7000_0000, 0x7001_0000, None));
        assert!(!thp_fits(pmd | 1 << 4, 0x7000_1000, 0x7001_0000, None));
        // Order 1, which only files have.
        assert!(thp_fits(1 << 1, 0x7000_0000, 0x7000_2000, Some(0)));
        // A mapping smaller than the block, near address 0.
        assert!(!thp_fits(pmd, 0, 4096, None));
        // A file's block also needs `vm_start >> PAGE_SHIFT` less its page
        // offset aligned to the block: offset 0 at an aligned start fits,
        // offset one page there does not, and offset one page at a start one
        // page past alignment does.
        assert!(thp_fits(pmd, 0x7000_0000, 0x7000_0000 + two_mib, Some(0)));
        assert!(!thp_fits(pmd, 0x7000_0000, 0x7000_0000 + two_mib, Some(1)));
        assert!(thp_fits(
            pmd,
            0x7000_1000,
            0x7000_0000 + 2 * two_mib,
            Some(1)
        ));
        // An offset past the start's page number: the subtraction wraps, as
        // Linux's unsigned one does, and is still aligned.
        assert!(thp_fits(
            pmd,
            0x7000_0000,
            0x7000_0000 + two_mib,
            Some(0x7_0200)
        ));
        assert!(!thp_fits(
            pmd,
            0x7000_0000,
            0x7000_0000 + two_mib,
            Some(0x7_0201)
        ));
    }

    /// `THPeligible` per class: anonymous memory 1 where an allowed order
    /// fits, unless `nh` or the address space disabled huge pages; a file or
    /// shared memory 0 where no order fits (or `nh`, or disabled) and
    /// refused otherwise; `hg` refused; an unasked answer refused only where
    /// it matters.
    #[test]
    fn thp_eligibility_follows_the_class_the_flags_and_the_settings() {
        let pmd = PMD_ORDER;
        let two_mib: u64 = 2 << 20;
        let range = (0x7000_0000, 0x7000_0000 + two_mib);
        let small = (0x7000_0000, 0x7000_0000 + two_mib - 4096);
        let rw = libc::PROT_READ | libc::PROT_WRITE;
        let anonymous = |range, flags| {
            mapping(
                range,
                rw,
                false,
                0,
                MappingBacking::Anonymous,
                MAY | reverie::VM_ACCOUNT | flags,
            )
        };
        let private_file = |range, offset, flags| {
            mapping(range, libc::PROT_READ, false, offset, file(), MAY | flags)
        };
        let shared_memory = |range| {
            mapping(
                range,
                rw,
                true,
                0,
                MappingBacking::NamedSharedAnonymous {
                    raw_device: 1,
                    raw_inode: 1036,
                    name: b"shared".to_vec(),
                },
                MAY | reverie::VM_SHARED | reverie::VM_MAYSHARE,
            )
        };
        let facts = SmapsFacts {
            thp_orders: pmd,
            file_thp_orders: pmd,
            thp_disabled: Some(false),
            ..HOST
        };
        let eligible = |mapping: &MappingFact, facts: &SmapsFacts| {
            thp_eligible(mapping, mapping.vm_flags.unwrap(), facts)
        };
        assert_eq!(eligible(&anonymous(range, 0), &facts), Ok(true));
        assert_eq!(eligible(&anonymous(small, 0), &facts), Ok(false));
        // MAP_STACK's `nh`, and an address space that disabled huge pages.
        assert_eq!(
            eligible(&anonymous(range, reverie::VM_NOHUGEPAGE), &facts),
            Ok(false)
        );
        let disabled = SmapsFacts {
            thp_disabled: Some(true),
            ..facts
        };
        assert_eq!(eligible(&anonymous(range, 0), &disabled), Ok(false));
        // Each anonymous class follows the anonymous orders, not the file
        // ones.
        let file_orders_only = SmapsFacts {
            thp_orders: 0,
            ..facts
        };
        for backing in [
            MappingBacking::Heap,
            MappingBacking::Stack,
            MappingBacking::NamedAnonymous(b"a".to_vec()),
        ] {
            let mapping = mapping(range, rw, false, 0, backing, MAY | reverie::VM_ACCOUNT);
            assert_eq!(eligible(&mapping, &facts), Ok(true), "{mapping:?}");
            assert_eq!(
                eligible(&mapping, &file_orders_only),
                Ok(false),
                "{mapping:?}"
            );
        }
        // A file or shared memory: 0 where no order fits at its offset, or
        // where `nh` or the address space rules huge pages out; refused where
        // one fits, as which file it maps is not a fact.
        assert_eq!(eligible(&private_file(small, 0, 0), &facts), Ok(false));
        assert_eq!(eligible(&private_file(range, 4096, 0), &facts), Ok(false));
        let refusal = eligible(&private_file(range, 0, 0), &facts).unwrap_err();
        assert!(
            refusal.contains("70000000-70200000 r--p 00000000 00:2a 17")
                && refusal.contains("open for writing"),
            "{refusal}"
        );
        assert!(eligible(&shared_memory(range), &facts).is_err());
        assert_eq!(eligible(&shared_memory(small), &facts), Ok(false));
        assert_eq!(
            eligible(&private_file(range, 0, reverie::VM_NOHUGEPAGE), &facts),
            Ok(false)
        );
        assert_eq!(eligible(&private_file(range, 0, 0), &disabled), Ok(false));
        // Files follow the file orders, not the anonymous ones.
        let anonymous_orders_only = SmapsFacts {
            file_thp_orders: 0,
            ..facts
        };
        assert_eq!(
            eligible(&private_file(range, 0, 0), &anonymous_orders_only),
            Ok(false)
        );
        // `hg` is refused for every class.
        for mapping in [
            anonymous(small, reverie::VM_HUGEPAGE),
            private_file(small, 0, reverie::VM_HUGEPAGE),
        ] {
            let refusal = eligible(&mapping, &facts).unwrap_err();
            assert!(refusal.contains("MADV_HUGEPAGE"), "{refusal}");
        }
        // Whether the address space disabled huge pages matters only where an
        // order fits.
        let unasked = SmapsFacts {
            thp_disabled: None,
            ..facts
        };
        assert!(eligible(&anonymous(range, 0), &unasked).is_err());
        assert_eq!(eligible(&anonymous(small, 0), &unasked), Ok(false));
        assert_eq!(
            eligible(&anonymous(range, reverie::VM_NOHUGEPAGE), &unasked),
            Ok(false)
        );
        // A kind of mapping no rule here covers.
        let vdso = MappingFact::new(range.0, range.1, 5, false, 0, MappingBacking::Vdso)
            .unwrap()
            .with_vm_flags(
                reverie::VM_READ | reverie::VM_EXEC | reverie::VM_MAYREAD | reverie::VM_MAYEXEC,
            )
            .unwrap();
        assert!(eligible(&vdso, &facts).is_err());
        // Through the renderer.
        let map = MemoryMap::new(vec![anonymous(range, 0)], true).unwrap();
        let text = String::from_utf8(render_smaps(&map, &facts).unwrap()).unwrap();
        assert!(text.contains("THPeligible:           1\n"), "{text}");
        assert!(render_smaps(&map, &unasked).is_err());
    }

    /// The host's settings, as sysfs shows them: the global setting and each
    /// size's, an `always` order allowed whatever the global setting, an
    /// `inherit` one only under a global `always`; and for files and shared
    /// memory the union [`HostThp::file_orders`] gives under each top-level
    /// shared memory setting, from every per-size `shmem_enabled`, order 1's
    /// (`hugepages-8kB`, which has no `enabled`) among them.
    #[test]
    #[allow(clippy::disallowed_methods)] // test cleanup of its own temp directory
    fn host_thp_settings_are_read_as_linux_combines_them() {
        let root = std::env::temp_dir().join(format!(
            "smaps-host-thp-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("transparent_hugepage");
        assert_eq!(HostThp::read(&path), Ok(None));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&path).unwrap();
        let choose = |choices: &[&str], setting: &str| {
            choices
                .iter()
                .map(|choice| {
                    if *choice == setting {
                        format!("[{choice}]")
                    } else {
                        choice.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
                + "\n"
        };
        std::fs::write(path.join("enabled"), "always [madvise] never\n").unwrap();
        // No shared memory settings at all: a kernel before Linux 4.8.
        for (kilobytes, setting) in [(2048, "inherit"), (64, "always"), (8, "never")] {
            let size = path.join(format!("hugepages-{kilobytes}kB"));
            std::fs::create_dir(&size).unwrap();
            if kilobytes != 8 {
                std::fs::write(
                    size.join("enabled"),
                    choose(&["always", "inherit", "madvise", "never"], setting),
                )
                .unwrap();
            }
        }
        let thp = HostThp::read(&path).unwrap().unwrap();
        assert_eq!(
            thp,
            HostThp {
                global_always: false,
                always: 1 << 4,
                inherit: 1 << 9,
                shmem: ShmemHuge::Never,
                shmem_always: 0,
                shmem_within_size: 0,
                shmem_inherit: PMD_ORDER,
                shmem_advise: 0,
            }
        );
        assert_eq!(thp.anonymous_orders(), 1 << 4);
        assert_eq!(thp.file_orders(), PMD_ORDER);
        std::fs::write(path.join("enabled"), "[always] madvise never\n").unwrap();
        let thp = HostThp::read(&path).unwrap().unwrap();
        assert_eq!(thp.anonymous_orders(), 1 << 4 | 1 << 9);
        // A global `always` lets a regular file have every file order.
        assert_eq!(thp.file_orders(), THP_ORDERS_ALL_FILE_DEFAULT);
        std::fs::write(path.join("enabled"), "always [madvise] never\n").unwrap();
        // This host's shape: per-size shared memory settings for every size.
        let shmem_choices = ["always", "inherit", "within_size", "advise", "never"];
        for (kilobytes, setting) in [
            (2048, "inherit"),
            (64, "always"),
            (8, "within_size"),
            (16, "advise"),
            (32, "never"),
        ] {
            let size = path.join(format!("hugepages-{kilobytes}kB"));
            std::fs::create_dir_all(&size).unwrap();
            std::fs::write(size.join("shmem_enabled"), choose(&shmem_choices, setting)).unwrap();
        }
        let top = ["always", "within_size", "advise", "never", "deny", "force"];
        for (setting, shmem, orders) in [
            // The per-size `always`, `within_size` and `inherit` orders, and the
            // PMD order a linked tmpfs file may have.
            (
                "within_size",
                ShmemHuge::MountDecides,
                1 << 9 | 1 << 4 | 1 << 1,
            ),
            ("always", ShmemHuge::MountDecides, 1 << 9 | 1 << 4 | 1 << 1),
            ("advise", ShmemHuge::MountDecides, 1 << 9 | 1 << 4 | 1 << 1),
            ("never", ShmemHuge::Never, 1 << 9 | 1 << 4 | 1 << 1),
            // No shared memory has huge pages.
            ("deny", ShmemHuge::Deny, 0),
            // The per-size `inherit` orders alone, and the PMD order.
            ("force", ShmemHuge::Force, 1 << 9),
        ] {
            std::fs::write(path.join("shmem_enabled"), choose(&top, setting)).unwrap();
            let thp = HostThp::read(&path).unwrap().unwrap();
            assert_eq!(thp.shmem, shmem, "{setting}");
            assert_eq!(
                (
                    thp.shmem_always,
                    thp.shmem_within_size,
                    thp.shmem_inherit,
                    thp.shmem_advise
                ),
                (1 << 4, 1 << 1, 1 << 9, 1 << 2),
                "{setting}"
            );
            assert_eq!(thp.file_orders(), orders, "{setting}");
        }
        // A global `always` still adds every file order under `deny`.
        std::fs::write(path.join("enabled"), "[always] madvise never\n").unwrap();
        std::fs::write(path.join("shmem_enabled"), choose(&top, "deny")).unwrap();
        assert_eq!(
            HostThp::read(&path).unwrap().unwrap().file_orders(),
            THP_ORDERS_ALL_FILE_DEFAULT
        );
        // Settings Linux does not give.
        std::fs::write(path.join("shmem_enabled"), "always never\n").unwrap();
        assert!(HostThp::read(&path).is_err());
        std::fs::write(path.join("shmem_enabled"), choose(&top, "never")).unwrap();
        std::fs::write(path.join("hugepages-8kB/shmem_enabled"), "[madvise]\n").unwrap();
        assert!(HostThp::read(&path).is_err());
        std::fs::write(
            path.join("hugepages-8kB/shmem_enabled"),
            choose(&shmem_choices, "never"),
        )
        .unwrap();
        std::fs::write(path.join("enabled"), "always madvise never\n").unwrap();
        assert!(HostThp::read(&path).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A mapping whose flags the backend does not establish refuses the file
    /// by name, the same mapping with them renders; flags a call may have set
    /// that the backend does not report, a key in use, and a host that prints
    /// `sd` refuse it too.
    #[test]
    fn what_the_memory_map_cannot_tell_refuses_smaps() {
        let unflagged = MappingFact::new(
            0x7fff_ff7f_0000,
            0x7fff_ff80_0000,
            0,
            false,
            0,
            MappingBacking::Anonymous,
        )
        .unwrap();
        let window = |below: MappingFact| {
            MemoryMap::new(
                vec![
                    below,
                    stack(0x7fff_ff80_0000, 0x7fff_ffff_f000, read_write()),
                ],
                false,
            )
            .unwrap()
        };
        let refusal = render_smaps(&window(unflagged.clone()), &HOST).unwrap_err();
        assert!(
            refusal.contains("7fffff7f0000-7fffff800000 ---p")
                && refusal.contains("does not establish them"),
            "{refusal}"
        );
        assert!(render_numa_maps(&window(unflagged.clone()), false).is_err());
        let flagged = unflagged.with_vm_flags(MAY | reverie::VM_ACCOUNT).unwrap();
        let text = String::from_utf8(render_smaps(&window(flagged), &HOST).unwrap()).unwrap();
        assert!(
            text.starts_with("7fffff7f0000-7fffff800000 ---p 00000000 00:00 0 \nSize:"),
            "{text}"
        );
        assert!(
            text.contains("VmFlags: mr mw me ac \n7fffff800000-"),
            "{text}"
        );
        let lone = MemoryMap::new(
            vec![stack(0x7fff_ff80_0000, 0x7fff_ffff_f000, read_write())],
            false,
        )
        .unwrap();
        for (facts, why) in [
            (
                SmapsFacts {
                    mapping_flags_changed: true,
                    ..HOST
                },
                "madvise advice or mlock",
            ),
            (
                SmapsFacts {
                    protection_keys_used: true,
                    ..HOST
                },
                "protection key",
            ),
            (
                SmapsFacts {
                    soft_dirty_printed: true,
                    ..HOST
                },
                "soft-dirty flag (sd)",
            ),
        ] {
            let refusal = render_smaps(&lone, &facts).unwrap_err();
            assert!(
                refusal.contains("[stack]") && refusal.contains(why),
                "{refusal}"
            );
        }
        // A key matters only where Linux prints keys.
        render_smaps(
            &lone,
            &SmapsFacts {
                protection_keys_enabled: false,
                protection_keys_used: true,
                ..HOST
            },
        )
        .unwrap();
        // An incomplete list that lists nothing is no answer; a complete one
        // is the empty file of a task without an address space.
        let none = MemoryMap::new(Vec::new(), false).unwrap();
        assert!(render_smaps(&none, &HOST).is_err());
        assert!(render_numa_maps(&none, false).is_err());
        let empty = MemoryMap::new(Vec::new(), true).unwrap();
        assert_eq!(render_smaps(&empty, &HOST).unwrap(), b"");
        assert_eq!(render_numa_maps(&empty, false).unwrap(), b"");
    }

    /// The stack's row is Linux's, through the sanitizer too; once advice
    /// that discards pages reached the stack, whether it holds one, which
    /// decides `kernelpagesize_kB`, is not a fact, and the file is refused.
    #[test]
    fn the_stack_numa_maps_row_is_linuxs() {
        let map = MemoryMap::new(
            vec![stack(0x7ffc_ec80_f000, 0x7ffc_ec83_3000, read_write())],
            false,
        )
        .unwrap();
        let rendered = render_numa_maps(&map, false).unwrap();
        assert_eq!(
            rendered,
            b"7ffcec80f000 default stack kernelpagesize_kB=4\n"
        );
        let linux =
            b"7ffcec80f000 default stack anon=6 dirty=6 active=2 N0=6 kernelpagesize_kB=4\n";
        assert_eq!(
            crate::procfs::sanitize_numa_maps(&rendered),
            crate::procfs::sanitize_numa_maps(linux)
        );
        let refusal = render_numa_maps(&map, true).unwrap_err();
        assert!(
            refusal.contains("discards pages") && refusal.contains("[stack]"),
            "{refusal}"
        );
    }

    /// A complete map's rollup spans its first mapping to its last but the
    /// gate area, as Linux printed it; an incomplete one is refused.
    #[test]
    fn the_rollup_spans_a_complete_map_and_refuses_an_incomplete_one() {
        let maps = b"\
            00400000-00401000 r--p 00000000 00:2a 5352979878                         /p\n\
            7ffcec80f000-7ffcec833000 rw-p 00000000 00:00 0                          [stack]\n\
            ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]\n";
        let map = MemoryMap::from_maps(maps).unwrap();
        let rendered = render_smaps_rollup(&map).unwrap();
        let text = String::from_utf8(rendered.clone()).unwrap();
        assert!(
            text.starts_with(
                "00400000-7ffcec833000 ---p 00000000 00:00 0                              [rollup]\n\
                 Rss:                   0 kB\nPss:                   0 kB\nPss_Dirty:             0 kB\n\
                 Pss_Anon:              0 kB\n"
            ),
            "{text}"
        );
        let linux = b"\
            00400000-7ffcec833000 ---p 00000000 00:00 0                              [rollup]\n\
            Rss:                2528 kB\n\
            Pss:                 228 kB\n\
            Pss_Dirty:           176 kB\n\
            Pss_Anon:            176 kB\n\
            Pss_File:             52 kB\n\
            Pss_Shmem:             0 kB\n\
            Shared_Clean:       2348 kB\n\
            Shared_Dirty:          0 kB\n\
            Private_Clean:         4 kB\n\
            Private_Dirty:       176 kB\n\
            Referenced:         2528 kB\n\
            Anonymous:           176 kB\n\
            KSM:                   0 kB\n\
            LazyFree:              0 kB\n\
            AnonHugePages:         0 kB\n\
            ShmemPmdMapped:        0 kB\n\
            FilePmdMapped:         0 kB\n\
            Shared_Hugetlb:        0 kB\n\
            Private_Hugetlb:       0 kB\n\
            Swap:                  0 kB\n\
            SwapPss:               0 kB\n\
            Locked:                0 kB\n";
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_smaps_rollup(&rendered)).unwrap(),
            String::from_utf8(crate::procfs::sanitize_smaps_rollup(linux)).unwrap()
        );
        assert!(
            render_smaps_rollup(&MemoryMap::new(map.mappings.clone(), false).unwrap()).is_err()
        );
        assert!(
            String::from_utf8(render_smaps_rollup(&MemoryMap::new(Vec::new(), true).unwrap()).unwrap())
                .unwrap()
                .starts_with(
                    "00000000-00000000 ---p 00000000 00:00 0                                  [rollup]\n"
                )
        );
    }

    /// The mnemonic table is in the order of the bits, names every bit a
    /// fact may hold and no other, and gives Linux 7.1's mnemonics; a line
    /// lists the set bits in that order, each with its space, and a bit
    /// without a mnemonic is no line.
    #[test]
    fn vm_flags_print_in_bit_order_with_linuxs_mnemonics() {
        let bits: Vec<u64> = VM_FLAG_MNEMONICS.iter().map(|(bit, _)| *bit).collect();
        assert!(bits.windows(2).all(|pair| pair[0] < pair[1]), "{bits:?}");
        assert!(bits.iter().all(|bit| bit.is_power_of_two()));
        assert_eq!(
            bits.iter().fold(0, |all, bit| all | bit),
            reverie::VM_KNOWN_FLAGS
        );
        // `show_smap_vma_flags`' table, by bit number (Linux 7.1 x86-64).
        let linux = [
            (0, "rd"),
            (1, "wr"),
            (2, "ex"),
            (3, "sh"),
            (4, "mr"),
            (5, "mw"),
            (6, "me"),
            (7, "ms"),
            (8, "gd"),
            (11, "gu"),
            (13, "lo"),
            (15, "sr"),
            (16, "rr"),
            (20, "ac"),
            (21, "nr"),
            (23, "sf"),
            (29, "hg"),
            (30, "nh"),
            (31, "mg"),
        ];
        assert_eq!(
            VM_FLAG_MNEMONICS
                .iter()
                .map(|(bit, name)| (bit.trailing_zeros(), *name))
                .collect::<Vec<_>>(),
            linux
        );
        // The VmFlags lines Linux 7.1.3 printed (the design's host probes).
        let rw = reverie::VM_READ | reverie::VM_WRITE;
        for (flags, line) in [
            (
                rw | MAY | reverie::VM_ACCOUNT,
                "VmFlags: rd wr mr mw me ac \n",
            ),
            (
                rw | MAY | reverie::VM_ACCOUNT | reverie::VM_NOHUGEPAGE,
                "VmFlags: rd wr mr mw me ac nh \n",
            ),
            (
                rw | MAY | reverie::VM_MAYBE_GUARD | reverie::VM_ACCOUNT | reverie::VM_NOHUGEPAGE,
                "VmFlags: rd wr mr mw me gu ac nh \n",
            ),
            (
                reverie::VM_READ | MAY | reverie::VM_MAYBE_GUARD | reverie::VM_ACCOUNT,
                "VmFlags: rd mr mw me gu ac \n",
            ),
            (
                rw | MAY | reverie::VM_LOCKED | reverie::VM_ACCOUNT,
                "VmFlags: rd wr mr mw me lo ac \n",
            ),
            (
                rw | MAY | reverie::VM_ACCOUNT | reverie::VM_SYNC,
                "VmFlags: rd wr mr mw me ac sf \n",
            ),
            (
                rw | MAY | reverie::VM_NORESERVE,
                "VmFlags: rd wr mr mw me nr \n",
            ),
            (
                rw | reverie::VM_SHARED | MAY | reverie::VM_MAYSHARE,
                "VmFlags: rd wr sh mr mw me ms \n",
            ),
            (
                reverie::VM_READ | reverie::VM_MAYREAD | reverie::VM_MAYEXEC | reverie::VM_MAYSHARE,
                "VmFlags: rd mr me ms \n",
            ),
            (0, "VmFlags: \n"),
            (
                reverie::VM_KNOWN_FLAGS,
                "VmFlags: rd wr ex sh mr mw me ms gd gu lo sr rr ac nr sf hg nh mg \n",
            ),
        ] {
            assert_eq!(vm_flags_line(flags).as_deref(), Some(line));
        }
        // VM_HUGETLB (`ht`) and VM_SOFTDIRTY (`sd`) are no fact's.
        assert_eq!(vm_flags_line(rw | 1 << 22), None);
        assert_eq!(vm_flags_line(rw | 1 << 27), None);
    }

    /// The flags `VmFlags` names, as bits; `None` where it names one a fact
    /// cannot hold.
    fn parse_vm_flags(line: &str) -> Option<u64> {
        line.split_whitespace().try_fold(0, |flags, name| {
            VM_FLAG_MNEMONICS
                .iter()
                .find(|(_, mnemonic)| *mnemonic == name)
                .map(|(bit, _)| flags | bit)
        })
    }

    /// Every block of this test process's own `smaps`, as the host kernel
    /// prints it, whose flags a fact can hold, is what Detcore renders from
    /// its `maps` line and those flags with this host's facts, through the
    /// sanitizer, wherever the renderer gives an answer: the derived fields
    /// (`THPeligible`, `ProtectionKey`, `VmFlags`) are the kernel's for each
    /// kind of mapping. The stack, at least one file and at least one
    /// anonymous mapping render. A file whose `THPeligible` the renderer
    /// refuses names the reason. The file is read once, so every block comes
    /// from that read.
    #[test]
    fn this_process_mappings_render_as_the_host_kernel_prints_them() {
        let thp = HostThp::read(Path::new("/sys/kernel/mm/transparent_hugepage")).unwrap();
        // SAFETY: PR_GET_THP_DISABLE reads a flag of this process.
        let disabled = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
        assert!(disabled >= 0);
        let facts = SmapsFacts {
            protection_keys_enabled: host_protection_keys_enabled().unwrap(),
            protection_keys_used: false,
            mapping_flags_changed: false,
            thp_orders: thp.map_or(0, |thp| thp.anonymous_orders()),
            file_thp_orders: thp.map_or(0, |thp| thp.file_orders()),
            thp_disabled: Some(disabled != 0),
            soft_dirty_printed: soft_dirty_printed().unwrap(),
        };
        let text = String::from_utf8(std::fs::read("/proc/self/smaps").unwrap()).unwrap();
        let mut blocks: Vec<String> = Vec::new();
        for line in text.split_inclusive('\n') {
            if line
                .split_once(' ')
                .is_some_and(|(range, _)| range.contains('-') && !range.ends_with(':'))
            {
                blocks.push(String::new());
            }
            blocks.last_mut().expect("a header first").push_str(line);
        }
        let table = BTreeMap::new();
        let (mut stack_rendered, mut files, mut anonymous) = (false, 0, 0);
        for block in &blocks {
            let header = block.lines().next().unwrap();
            let flags = block
                .lines()
                .find_map(|line| line.strip_prefix("VmFlags:"))
                .expect("a VmFlags line");
            let Some(flags) = parse_vm_flags(flags) else {
                continue;
            };
            let Ok(map) = MemoryMap::from_maps(format!("{header}\n").as_bytes()) else {
                continue;
            };
            let backing = map.mappings[0].backing.clone();
            // The gate area's flags (`ex` without `me` under `vsyscall=xonly`)
            // are no set a mapping of the address space can have.
            let Ok(fact) = map.mappings[0].clone().with_vm_flags(flags) else {
                assert!(matches!(backing, MappingBacking::Special(_)), "{header}");
                continue;
            };
            let rendered = render_smaps(&MemoryMap::new(vec![fact], false).unwrap(), &facts);
            let rendered = match rendered {
                Ok(rendered) => rendered,
                Err(refusal) => {
                    assert!(
                        matches!(backing, MappingBacking::File { .. })
                            && refusal.contains("open for writing")
                            || matches!(backing, MappingBacking::Special(_)),
                        "{header}: {refusal}"
                    );
                    continue;
                }
            };
            assert_eq!(
                String::from_utf8(crate::procfs::sanitize_smaps(&rendered, &table)).unwrap(),
                String::from_utf8(crate::procfs::sanitize_smaps(block.as_bytes(), &table)).unwrap(),
                "{header}"
            );
            match backing {
                MappingBacking::Stack => stack_rendered = true,
                MappingBacking::File { .. } => files += 1,
                MappingBacking::Anonymous | MappingBacking::Heap => anonymous += 1,
                _ => {}
            }
        }
        assert!(stack_rendered && files > 0 && anonymous > 0, "{text}");
    }

    /// This test process's own `[stack]`, as the host kernel prints it in
    /// `numa_maps`, is what Detcore renders from its `maps` line, through the
    /// sanitizer. Another thread of the process may change its mappings
    /// while a file is read, so a read that finds no stack is read again.
    #[test]
    fn this_process_stack_numa_maps_row_is_the_host_kernels() {
        let header = (0..100)
            .find_map(|_| {
                let text = std::fs::read_to_string("/proc/self/maps").ok()?;
                text.lines()
                    .find(|line| line.ends_with(" [stack]"))
                    .map(str::to_owned)
            })
            .expect("a stack line");
        let fact = MemoryMap::from_maps(format!("{header}\n").as_bytes())
            .unwrap()
            .mappings
            .remove(0);
        let start = fact.start;
        let map = MemoryMap::new(vec![fact], false).unwrap();
        let row = std::fs::read_to_string("/proc/self/numa_maps")
            .unwrap()
            .lines()
            .find(|line| line.starts_with(&format!("{start:08x} ")))
            .map(|line| format!("{line}\n"))
            .expect("the stack's row");
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_numa_maps(
                &render_numa_maps(&map, false).unwrap()
            ))
            .unwrap(),
            String::from_utf8(crate::procfs::sanitize_numa_maps(row.as_bytes())).unwrap()
        );
    }
}
