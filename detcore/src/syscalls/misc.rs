/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Miscellaneous virtualized syscalls.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use rand::RngExt as _;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;

use crate::consts::DEFAULT_HOSTNAME;
use crate::detlog;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

fn from_str(s: &str) -> [i8; 65] {
    let mut ret: [i8; 65] = [0; 65];
    for (i, ch) in s.bytes().take(64).enumerate() {
        ret[i] = ch as i8;
    }
    ret
}

const GETRANDOM_ALLOWED_FLAGS: u32 = libc::GRND_NONBLOCK | libc::GRND_RANDOM | libc::GRND_INSECURE;

// AUTONOMOUS-BOT-IMPLEMENTED
fn validate_getrandom_flags(flags: usize) -> Result<(), Errno> {
    let flags = flags as u32;
    let random = flags & libc::GRND_RANDOM != 0;
    let insecure = flags & libc::GRND_INSECURE != 0;

    if flags & !GETRANDOM_ALLOWED_FLAGS != 0 || (random && insecure) {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

const RANDOM_FILL_CHUNK_BYTES: usize = 4096;
// Linux's import_ubuf clamps getrandom requests to MAX_RW_COUNT on x86_64.
const GETRANDOM_MAX_BYTES: usize = (i32::MAX as usize) & !4095;

fn getrandom_request_len(requested: usize) -> usize {
    requested.min(GETRANDOM_MAX_BYTES)
}

fn write_random_chunk(
    mut memory: impl MemoryAccess,
    remote_buf: AddrMut<u8>,
    local_buf: &[u8],
) -> Result<usize, Errno> {
    const PTRACE_WORD_SPLIT: usize = std::mem::size_of::<u64>() / 2;

    if local_buf.len() != std::mem::size_of::<u64>() {
        return memory.write(remote_buf, local_buf);
    }

    // safeptrace uses PTRACE_POKEDATA for exactly eight bytes, which bypasses guest page
    // protections. Split that case so getrandom observes the same EFAULT boundary as Linux.
    let first = memory.write(remote_buf, &local_buf[..PTRACE_WORD_SPLIT])?;
    if first < PTRACE_WORD_SPLIT {
        return Ok(first);
    }
    let Some(second_buf) = remote_buf
        .as_raw()
        .checked_add(PTRACE_WORD_SPLIT)
        .and_then(AddrMut::<u8>::from_raw)
    else {
        return Ok(first);
    };
    match memory.write(second_buf, &local_buf[PTRACE_WORD_SPLIT..]) {
        Ok(second) => Ok(first + second),
        Err(_) => Ok(first),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Fill guest memory from the deterministic PRNG owned by the current thread.
    pub(super) fn fill_random_bytes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        remote_buf: AddrMut<u8>,
        len: usize,
        source: &str,
    ) -> Result<usize, Error> {
        let mut local_words = [0_u64; RANDOM_FILL_CHUNK_BYTES / std::mem::size_of::<u64>()];
        let mut hasher = DefaultHasher::new();
        let mut written = 0;

        while written < len {
            let remote_chunk = match remote_buf
                .as_raw()
                .checked_add(written)
                .and_then(AddrMut::<u8>::from_raw)
            {
                Some(address) => address,
                None if written == 0 => return Err(Errno::EFAULT.into()),
                None => break,
            };
            let chunk_len = (len - written).min(RANDOM_FILL_CHUNK_BYTES);
            // safeptrace's 8-byte write fast path currently requires an aligned source buffer.
            let local_buf = unsafe {
                std::slice::from_raw_parts_mut(local_words.as_mut_ptr().cast::<u8>(), chunk_len)
            };
            guest.thread_state_mut().thread_prng().fill(local_buf);
            let n = match write_random_chunk(guest.memory(), remote_chunk, local_buf) {
                Ok(n) => n,
                Err(_) if written > 0 => break,
                Err(error) => return Err(error.into()),
            };
            if n == 0 {
                if written == 0 {
                    return Err(Errno::EFAULT.into());
                }
                break;
            }
            if cfg!(debug_assertions) {
                Hash::hash_slice(&local_buf[..n], &mut hasher);
            }
            written += n;
            if n < chunk_len {
                break;
            }
        }

        if cfg!(debug_assertions) {
            detlog!(
                "[dtid {}] USER RAND [{}] Filled guest memory with {} random bytes, hash of bytes: {}",
                guest.thread_state().dettid,
                source,
                written,
                hasher.finish()
            );
        }
        Ok(written)
    }

    /// uname syscall
    pub async fn handle_uname<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Uname,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        if let Some(buf) = call.buf() {
            let mut un = guest.memory().read_value(buf)?;
            // Keep this in configured UTC: `Local` initializes libc TLS, which is unavailable
            // while a DynamoRIO application thread is executing a client callback.
            let epoch = guest.config().epoch;

            if !guest.config().has_uts_namespace {
                // FIXME: It should be possible to remove this once all tests
                // are also using namespaces.
                un.nodename = from_str(DEFAULT_HOSTNAME);
                un.domainname = from_str(DEFAULT_HOSTNAME.split('.').next_back().unwrap_or(""));
            }

            un.release = from_str("5.2.0");
            un.version = from_str(&format!("#1 SMP {}", epoch.format("%a %b %d %T %Z %Y")));
            guest.memory().write_value(buf, &un)?;
        }

        Ok(ret)
    }

    /// Fill `getrandom(2)` requests from the current thread's seeded deterministic PRNG.
    /// Supported blocking/source-selection flags share that always-ready stream; invalid Linux
    /// flag combinations are rejected before guest memory is touched.
    pub async fn handle_getrandom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrandom,
    ) -> Result<i64, Error> {
        validate_getrandom_flags(call.flags())?;
        let len = getrandom_request_len(call.buflen());
        if len == 0 {
            return Ok(0);
        }

        let buf = call.buf().ok_or(Errno::EFAULT)?;

        let n = self.fill_random_bytes(guest, buf, len, "getrandom")?;
        Ok(n as i64)
    }

    /// setsid system call
    pub async fn handle_setsid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsid,
    ) -> Result<i64, Error> {
        let res = guest.inject(call).await?;

        // task is trying to become a daemon process. for more details
        // see: https://notes.shichao.io/apue/ch13/
        if guest.config().kill_daemons {
            guest.daemonize().await;
        }
        Ok(res)
    }

    /// membarrier (system call).
    ///
    /// `membarrier(2)` issues process-wide memory barriers so that userspace can
    /// use asymmetric fences (e.g. CPython's QSBR, RCU-style reclamation).
    /// Detcore serializes all guest threads onto a single logical CPU with a
    /// total memory order, so any requested barrier is *already* satisfied and
    /// every command is a deterministic no-op. For `MEMBARRIER_CMD_QUERY` we
    /// report the set of commands we emulate so the guest stays on this
    /// controlled path instead of a host-dependent fallback; every other command
    /// returns success without doing anything.
    pub async fn handle_membarrier<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Membarrier,
    ) -> Result<i64, Error> {
        // Values from <linux/membarrier.h>.
        const MEMBARRIER_CMD_QUERY: i32 = 0;
        const MEMBARRIER_CMD_GLOBAL: i32 = 1 << 0;
        const MEMBARRIER_CMD_GLOBAL_EXPEDITED: i32 = 1 << 1;
        const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: i32 = 1 << 2;
        const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
        const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
        const SUPPORTED: i32 = MEMBARRIER_CMD_GLOBAL
            | MEMBARRIER_CMD_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_PRIVATE_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED;

        let cmd = call.cmd();
        if cmd == MEMBARRIER_CMD_QUERY {
            detlog!(
                "[dtid {}] membarrier(QUERY) => reporting emulated commands {:#x}",
                guest.thread_state().dettid,
                SUPPORTED,
            );
            Ok(SUPPORTED as i64)
        } else {
            detlog!(
                "[dtid {}] membarrier(cmd={}) no-op (threads are serialized on one CPU)",
                guest.thread_state().dettid,
                cmd,
            );
            Ok(0)
        }
    }

    /// getcpu system call
    pub async fn handle_getcpu<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getcpu,
    ) -> Result<i64, Error> {
        // Always set the CPU to 0.
        if let Some(cpu) = call.cpu() {
            guest.memory().write_value(cpu, &0)?;
        }

        // Always set the NUMA node to 0.
        if let Some(node) = call.node() {
            guest.memory().write_value(node, &0)?;
        }

        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getrandom_accepts_linux_flags() {
        for flags in [
            0,
            libc::GRND_NONBLOCK as usize,
            libc::GRND_RANDOM as usize,
            (libc::GRND_NONBLOCK | libc::GRND_RANDOM) as usize,
            libc::GRND_INSECURE as usize,
            (libc::GRND_NONBLOCK | libc::GRND_INSECURE) as usize,
            1_usize << 32,
        ] {
            assert!(
                validate_getrandom_flags(flags).is_ok(),
                "valid flags rejected: {flags:#x}"
            );
        }
    }

    #[test]
    fn getrandom_rejects_invalid_flags() {
        for flags in [
            0x8000_0000,
            (1_usize << 32) | 0x8000_0000,
            (libc::GRND_RANDOM | libc::GRND_INSECURE) as usize,
        ] {
            assert_eq!(validate_getrandom_flags(flags), Err(Errno::EINVAL));
        }
    }

    #[test]
    fn getrandom_caps_requests_at_linux_max_rw_count() {
        assert_eq!(getrandom_request_len(16), 16);
        assert_eq!(getrandom_request_len(usize::MAX), GETRANDOM_MAX_BYTES);
    }
}
