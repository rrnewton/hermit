/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Experimental (provrepro, `--cpu-caches`): the cache sizes a witnessed
//! build machine had, given to every view a guest can read them from.
//!
//! A build that records the build host's cache sizes (m4ri's configure
//! writes them into `m4ri_config.h` from
//! `/sys/devices/system/cpu/cpu0/cache/index*/size`) produces output that
//! depends on the host. With this witness, Detcore answers CPUID leaves 2,
//! 4, 0x8000_0005 and 0x8000_0006, and the sysfs cache files of every
//! virtual CPU, from one description: four caches, in the order Linux lists
//! them from leaf 4 (`index0` L1 data, `index1` L1 instruction, `index2`
//! unified L2, `index3` unified L3), each with 64-byte lines and one
//! partition.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

/// The four cache sizes, in KiB.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CpuCacheSizes {
    pub l1d_kib: u32,
    pub l1i_kib: u32,
    pub l2_kib: u32,
    pub l3_kib: u32,
}

/// What kind of cache a [`Cache`] is, as leaf 4's type field numbers it and
/// sysfs's `type` file names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheType {
    Data,
    Instruction,
    Unified,
}

impl CacheType {
    /// Leaf 4's EAX bits 4:0.
    pub fn leaf4(self) -> u32 {
        match self {
            CacheType::Data => 1,
            CacheType::Instruction => 2,
            CacheType::Unified => 3,
        }
    }

    /// sysfs's `type` (drivers/base/cacheinfo.c, `type_show`).
    pub fn sysfs(self) -> &'static str {
        match self {
            CacheType::Data => "Data",
            CacheType::Instruction => "Instruction",
            CacheType::Unified => "Unified",
        }
    }
}

/// One cache's geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cache {
    pub level: u32,
    pub kind: CacheType,
    pub size_kib: u32,
    pub ways: u32,
    pub line: u32,
    pub partitions: u32,
}

impl Cache {
    pub fn sets(&self) -> u32 {
        self.size_kib * 1024 / (self.ways * self.line * self.partitions)
    }

    /// The four registers of leaf 4 for this cache: one core, one logical
    /// processor sharing it, self-initializing.
    pub fn leaf4(&self) -> [u32; 4] {
        let eax = self.kind.leaf4() | (self.level << 5) | (1 << 8);
        let ebx = (self.line - 1) | ((self.partitions - 1) << 12) | ((self.ways - 1) << 22);
        [eax, ebx, self.sets() - 1, 0]
    }

    /// The contents of sysfs's `cache/index<M>/<attribute>` for this cache,
    /// as drivers/base/cacheinfo.c prints them, for the attributes this
    /// description determines.
    pub fn sysfs(&self, attribute: &str) -> Option<String> {
        Some(match attribute {
            "level" => format!("{}\n", self.level),
            "type" => format!("{}\n", self.kind.sysfs()),
            "size" => format!("{}K\n", self.size_kib),
            "ways_of_associativity" => format!("{}\n", self.ways),
            "coherency_line_size" => format!("{}\n", self.line),
            "number_of_sets" => format!("{}\n", self.sets()),
            "physical_line_partition" => format!("{}\n", self.partitions),
            _ => return None,
        })
    }
}

/// Associativity by index: 8-way L1s and L2, 16-way L3.
const WAYS: [u32; 4] = [8, 8, 8, 16];
const LINE: u32 = 64;

impl CpuCacheSizes {
    /// Leaf 4 subleaf / sysfs `index` `n`, or `None` past the last cache.
    pub fn cache(&self, n: u32) -> Option<Cache> {
        let (level, kind, size_kib) = match n {
            0 => (1, CacheType::Data, self.l1d_kib),
            1 => (1, CacheType::Instruction, self.l1i_kib),
            2 => (2, CacheType::Unified, self.l2_kib),
            3 => (3, CacheType::Unified, self.l3_kib),
            _ => return None,
        };
        Some(Cache {
            level,
            kind,
            size_kib,
            ways: WAYS[n as usize],
            line: LINE,
            partitions: 1,
        })
    }

    /// `leaf`/`subleaf`'s answer with this description's caches in place of
    /// `table`'s: leaf 2 says "see leaf 4" (descriptor 0xff), leaf 4 lists
    /// the four caches, and the AMD-style extended leaves give the same
    /// sizes. Every other leaf is `table`'s.
    pub fn answer(&self, leaf: u32, subleaf: u32, table: [u32; 4]) -> [u32; 4] {
        let [_, _, ecx, edx] = table;
        match leaf {
            0x2 => [0x0000_ff01, 0, 0, 0],
            0x4 => self.cache(subleaf).map_or([0; 4], |cache| cache.leaf4()),
            // L1d and L1i: size in KiB (31:24), ways (23:16), one line per
            // tag (15:8), line size (7:0).
            0x8000_0005 => {
                let l1 =
                    |kib: u32, ways: u32| (kib.min(0xff) << 24) | (ways << 16) | (1 << 8) | LINE;
                [
                    table[0],
                    table[1],
                    l1(self.l1d_kib, WAYS[0]),
                    l1(self.l1i_kib, WAYS[1]),
                ]
            }
            // L2: size in KiB (31:16), 8-way (encoding 6), one line per tag,
            // line size; L3: size in 512 KiB units (31:18), 16-way
            // (encoding 8), one line per tag, line size.
            0x8000_0006 => {
                let _ = (ecx, edx);
                [
                    table[0],
                    table[1],
                    (self.l2_kib.min(0xffff) << 16) | (6 << 12) | (1 << 8) | LINE,
                    ((self.l3_kib / 512).min(0x3fff) << 18) | (8 << 12) | (1 << 8) | LINE,
                ]
            }
            _ => table,
        }
    }
}

impl FromStr for CpuCacheSizes {
    type Err = String;

    /// `L1D,L1I,L2,L3`, each a number of bytes with an optional `K` or `M`
    /// (binary) suffix, such as `32K,64K,512K,16M`. Each must be a whole
    /// number of its cache's ways times 64-byte lines, and the L3 a whole
    /// number of 512 KiB, which leaf 0x8000_0006 counts it in.
    fn from_str(text: &str) -> Result<Self, String> {
        let sizes = text
            .split(',')
            .map(|size| {
                let size = size.trim();
                let (digits, unit) = match size.as_bytes().last() {
                    Some(b'K' | b'k') => (&size[..size.len() - 1], 1024u64),
                    Some(b'M' | b'm') => (&size[..size.len() - 1], 1024 * 1024),
                    _ => (size, 1),
                };
                digits
                    .parse::<u64>()
                    .map(|n| n * unit)
                    .map_err(|e| format!("{size:?}: {e}"))
            })
            .collect::<Result<Vec<u64>, String>>()?;
        let [l1d, l1i, l2, l3] = sizes[..] else {
            return Err(format!(
                "{text:?}: expected four sizes, L1D,L1I,L2,L3 (such as 32K,64K,512K,16M)"
            ));
        };
        for (n, bytes) in [l1d, l1i, l2, l3].into_iter().enumerate() {
            let unit = u64::from(WAYS[n] * LINE).max(1024);
            if bytes == 0 || bytes % unit != 0 || bytes / 1024 > u64::from(u32::MAX) {
                return Err(format!(
                    "{text:?}: size {} must be a positive multiple of {unit} bytes",
                    n + 1
                ));
            }
        }
        if l3 % (512 * 1024) != 0 {
            return Err(format!("{text:?}: the L3 must be a multiple of 512K"));
        }
        let kib = |bytes: u64| (bytes / 1024) as u32;
        Ok(CpuCacheSizes {
            l1d_kib: kib(l1d),
            l1i_kib: kib(l1i),
            l2_kib: kib(l2),
            l3_kib: kib(l3),
        })
    }
}

impl fmt::Display for CpuCacheSizes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}K,{}K,{}K,{}K",
            self.l1d_kib, self.l1i_kib, self.l2_kib, self.l3_kib
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_describes_four_caches() {
        let sizes: CpuCacheSizes = "32K,64K,512K,16M".parse().unwrap();
        assert_eq!(sizes.to_string(), "32K,64K,512K,16384K");
        assert_eq!(sizes.to_string().parse::<CpuCacheSizes>().unwrap(), sizes);
        let l1i = sizes.cache(1).unwrap();
        assert_eq!(l1i.sysfs("size").as_deref(), Some("64K\n"));
        assert_eq!(l1i.sysfs("type").as_deref(), Some("Instruction\n"));
        assert_eq!(l1i.sets(), 128);
        // Leaf 4's size formula gives the size back.
        for n in 0..4 {
            let [_, ebx, ecx, _] = sizes.cache(n).unwrap().leaf4();
            let ways = (ebx >> 22) + 1;
            let parts = ((ebx >> 12) & 0x3ff) + 1;
            let line = (ebx & 0xfff) + 1;
            assert_eq!(
                ways * parts * line * (ecx + 1),
                sizes.cache(n).unwrap().size_kib * 1024
            );
        }
        assert_eq!(sizes.answer(4, 4, [1, 2, 3, 4]), [0; 4]);
        assert_eq!(sizes.answer(0x8000_0006, 0, [0; 4])[2] >> 16, 512);
        assert_eq!(sizes.answer(0x8000_0006, 0, [0; 4])[3] >> 18, 32);
        assert!("32K,64K,512K".parse::<CpuCacheSizes>().is_err());
        assert!("32K,64K,512K,1000K".parse::<CpuCacheSizes>().is_err());
    }
}
