/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Rewriting canonical load/store-exclusive retry loops into single FEAT_LSE
//! atomics, in place and at the same length.
//!
//! Why in place: every instruction keeps its address, so every PC-relative
//! branch, return address, unwind table, symbol and signal PC in the image
//! stays valid, and the rewrite needs no trampoline, no scratch page and no
//! free register beyond the ones the loop itself already destroys.
//!
//! The scratch registers are the loop's own. A retry loop writes its status
//! register unconditionally before anything can read it, and its load
//! destination likewise, so both hold no live value at the loop's entry: the
//! replacement may use them to materialise an operand, and restores the exact
//! value the original left in each one on every exit path.
//!
//! Ordering is carried across exactly, pair to atomic (Arm ARM DDI 0487 L.a,
//! C3.2.6 and C3.2.12):
//!
//! | load | store | atomic |
//! |---|---|---|
//! | LDAXR | STLXR | `AL` |
//! | LDAXR | STXR | `A` |
//! | LDXR | STLXR | `L` |
//! | LDXR | STXR | plain |
//!
//! Only register-only instructions are allowed between the pair, so an
//! acquire taken at the atomic rather than at the load, and a release taken at
//! the atomic rather than at the store, order the same memory accesses. The
//! one place the rewrite is strictly stronger is a comparison that fails:
//! `CASAL` keeps its release semantics where the original simply never
//! executed its `STLXR`. Extra ordering cannot create an execution the
//! original could not have, but it can hide a weak-memory race, which is why
//! this is opt-in.

use std::collections::HashMap;
use std::collections::HashSet;

use super::insn::AluOp;
use super::insn::AtomicOp;
use super::insn::Insn;
use super::insn::Operand;
use super::insn::Ordering;
use super::insn::Reg;
use super::insn::Size;
use super::insn::ZR;
use super::insn::decode;
use super::insn::{self};

/// The loop shapes this rewriter recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Idiom {
    /// `LDxXR; <alu>; STxXR; CBNZ` - read-modify-write, the ALU operation
    /// possibly setting the flags (`SUBS` in Rust's `Arc` drop).
    Rmw,
    /// `LDxXR; STxXR; CBNZ` - unconditional store, that is, a swap.
    Swap,
    /// `LDxXR; CMP; B.NE out; STxXR; CBNZ` - compare-and-swap.
    CasCompare,
    /// `LDxXR; CBNZ out; STxXR; CBNZ` - store only into a zero location.
    CasZero,
}

/// Why a store-exclusive was left alone. Every exclusive instruction in the
/// scanned range ends up either rewritten or carrying one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A load-exclusive or store-exclusive pair instruction (LDXP/STXP).
    ExclusivePair,
    /// No load-exclusive on the same address immediately before the store.
    NoPairedLoad,
    /// An instruction in the window is outside the decoded subset.
    Undecoded,
    /// The window is decoded but is not one of the four idioms.
    NotCanonical,
    /// A direct branch elsewhere in the scanned range targets the inside of
    /// the sequence, so the replacement could be entered part-way. Only
    /// direct branches are checked: an indirect branch's target (a jump
    /// table, a landing pad) is not known here.
    BranchIntoSequence,
    /// The idiom matched but its registers break an assumption the
    /// replacement needs; the string names which one.
    Registers(&'static str),
    /// The operation has no single-instruction LSE equivalent.
    UnsupportedOperation,
    /// A branch the replacement must move no longer reaches its target.
    BranchOutOfRange,
    /// A relocation points into the sequence, so the loader writes those
    /// bytes itself, for the instruction that used to be there.
    Relocated,
    /// `LDxXR Rt; CMP Rt, Re; B.NE out; <alu> Rv, ...; STxXR Ws, Rv; CBNZ`: a
    /// compare-and-swap that computes the value it stores after the compare
    /// (Rust's `futex_rwlock` read path and panic count). A CAS needs that
    /// value in a register before it runs, and the only registers the
    /// original changes on its mismatch path are Rt and the flags, both still
    /// needed there; with six instructions to replace, there is no room to
    /// restore another. Rewriting it needs a seventh instruction, or proof
    /// that the failure path overwrites Rv before reading it.
    NewValueAfterCompare,
}

/// A sequence the rewriter replaces, and its replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Virtual address of the first instruction of the sequence.
    pub addr: u64,
    /// Which canonical loop it is.
    pub idiom: Idiom,
    /// The replacement words, one per original instruction.
    pub words: Vec<u32>,
}

/// An exclusive the rewriter leaves alone, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedSite {
    /// Virtual address of the store-exclusive (or pair instruction).
    pub addr: u64,
    /// Why it is left alone.
    pub reason: Refusal,
}

/// What planning one run of instruction words found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The sequences to replace.
    pub patched: Vec<Site>,
    /// The exclusives left alone.
    pub refused: Vec<RefusedSite>,
    /// Exclusive instructions seen, whether or not they were rewritten.
    pub exclusive_instructions: usize,
}

impl Report {
    /// The replaced sequences, counted by idiom.
    pub fn by_idiom(&self) -> HashMap<Idiom, usize> {
        let mut counts = HashMap::new();
        for site in &self.patched {
            *counts.entry(site.idiom).or_default() += 1;
        }
        counts
    }
}

/// A code range to scan: `words` are the instructions at `base`.
pub fn plan(words: &[u32], base: u64) -> Report {
    plan_with_external_targets(words, base, &HashSet::new())
}

/// The same, told the direct branch targets of the code OUTSIDE this range:
/// an image's other executable sections can branch into this one, and a
/// sequence those branches reach is refused exactly as one this range reaches
/// would be.
pub fn plan_with_external_targets(
    words: &[u32],
    base: u64,
    external_targets: &HashSet<u64>,
) -> Report {
    let decoded: Vec<Insn> = words.iter().copied().map(decode).collect();
    let branches = direct_branches(&decoded, base);
    let mut targets: HashSet<u64> = branches.iter().map(|(_, target)| *target).collect();
    targets.extend(external_targets.iter().copied());
    let addr = |index: usize| base + 4 * index as u64;
    let mut report = Report::default();
    for (index, insn) in decoded.iter().enumerate() {
        match insn {
            Insn::ExclPair => {
                report.exclusive_instructions += 1;
                // Report the store side only, so a pair loop counts once, as
                // a single-register loop does.
                if (words[index] >> 22) & 1 == 0 {
                    report.refused.push(RefusedSite {
                        addr: addr(index),
                        reason: Refusal::ExclusivePair,
                    });
                }
            }
            Insn::LoadExcl { .. } => report.exclusive_instructions += 1,
            Insn::StoreExcl { .. } => {
                report.exclusive_instructions += 1;
                match match_sequence(words, &decoded, index, base, &branches, external_targets) {
                    Ok(site) => {
                        let start = ((site.addr - base) / 4) as usize;
                        let interior_entered = (start + 1..start + site.words.len())
                            .any(|i| targets.contains(&addr(i)));
                        if interior_entered {
                            report.refused.push(RefusedSite {
                                addr: addr(index),
                                reason: Refusal::BranchIntoSequence,
                            });
                        } else {
                            report.patched.push(site);
                        }
                    }
                    Err(reason) => report.refused.push(RefusedSite {
                        addr: addr(index),
                        reason,
                    }),
                }
            }
            _ => {}
        }
    }
    report
}

/// Apply a plan to the same words it was computed from.
pub fn apply(words: &mut [u32], base: u64, report: &Report) {
    for site in &report.patched {
        let index = ((site.addr - base) / 4) as usize;
        words[index..index + site.words.len()].copy_from_slice(&site.words);
    }
}

/// The addresses every direct branch in the range targets.
pub fn direct_branch_targets(words: &[u32], base: u64) -> HashSet<u64> {
    let decoded: Vec<Insn> = words.iter().copied().map(decode).collect();
    direct_branches(&decoded, base)
        .into_iter()
        .map(|(_, target)| target)
        .collect()
}

/// Every direct branch in the range, as (index of the branch, target address).
fn direct_branches(decoded: &[Insn], base: u64) -> Vec<(usize, u64)> {
    let mut branches = Vec::new();
    for (index, insn) in decoded.iter().enumerate() {
        let here = base + 4 * index as u64;
        let offset = match *insn {
            Insn::Branch { offset, .. }
            | Insn::CondBranch { offset, .. }
            | Insn::CompareBranch { offset, .. }
            | Insn::TestBranch { offset } => offset,
            _ => continue,
        };
        branches.push((index, here.wrapping_add(offset as u64)));
    }
    branches
}

/// Match the sequence ending at the store-exclusive `store`, returning the
/// index of its first instruction, the replacement words and the idiom.
#[allow(clippy::too_many_arguments)]
fn match_sequence(
    words: &[u32],
    decoded: &[Insn],
    store: usize,
    base: u64,
    branches: &[(usize, u64)],
    external_targets: &HashSet<u64>,
) -> Result<Site, Refusal> {
    let (size, store_ord, rs, rv, rn) = match decoded[store] {
        Insn::StoreExcl {
            size,
            ord,
            rs,
            rt,
            rn,
        } => (size, ord, rs, rt, rn),
        _ => unreachable!("caller matched a store-exclusive"),
    };
    // A compare-and-swap that computes its new value after the compare has
    // no load within the three instructions any idiom spans, so nothing
    // below could match it; name why it is left alone, wherever it retries.
    if store >= 4
        && matches!(decoded[store - 4], Insn::LoadExcl { rn: load_rn, .. } if load_rn == rn)
        && matches!(decoded[store - 3], Insn::Cmp { .. })
        && matches!(decoded[store - 2], Insn::CondBranch { cond: 1, .. })
        && matches!(decoded[store - 1], Insn::Alu { rd, .. } if rd == rv)
    {
        return Err(Refusal::NewValueAfterCompare);
    }
    // The store must be followed by `CBNZ Rs, <retry>`, its failure path.
    let close = store + 1;
    let retry = match decoded.get(close) {
        Some(&Insn::CompareBranch {
            nonzero: true,
            sf: false,
            rt,
            offset,
        }) if rt == rs => close as i64 + offset / 4,
        _ => return Err(Refusal::NotCanonical),
    };
    // The canonical loop retries at its own load. A loop that retries
    // elsewhere, such as Rust's `fetch_update` around a weak
    // compare-exchange (which recomputes the new value from the one it read
    // before trying again), starts at the nearest load-exclusive before the
    // store, at most three instructions back, the longest idiom. Where the
    // retry goes does not matter to the replacement: the retry runs only
    // when the store-exclusive fails, and the atomic never does, so every
    // execution of the replacement is one in which the original's first
    // store-exclusive succeeded. A retry into the sequence after its first
    // instruction would be entered part-way, and is refused.
    let start = if (0..store as i64).contains(&retry)
        && matches!(decoded[retry as usize], Insn::LoadExcl { .. })
    {
        retry as usize
    } else {
        let start = (store.saturating_sub(3)..store)
            .rev()
            .find(|&i| matches!(decoded[i], Insn::LoadExcl { .. }))
            .ok_or(Refusal::NoPairedLoad)?;
        if (start as i64 + 1..=close as i64).contains(&retry) {
            return Err(Refusal::NotCanonical);
        }
        start
    };
    let (load_size, load_ord, rt) = match decoded[start] {
        Insn::LoadExcl {
            size,
            ord,
            rt,
            rn: load_rn,
        } if load_rn == rn => (size, ord, rt),
        Insn::LoadExcl { .. } => return Err(Refusal::NotCanonical),
        _ => return Err(Refusal::NoPairedLoad),
    };
    if load_size != size {
        return Err(Refusal::NotCanonical);
    }
    if decoded[start + 1..store].contains(&Insn::Other) {
        return Err(Refusal::Undecoded);
    }
    // The status register must be a register the loop itself destroys and
    // nothing else needs: the architecture already requires it to differ from
    // the stored value and the base. It MAY be the load destination, which is
    // what LLVM emits for a compare-and-swap, and each idiom below says
    // whether its replacement can still restore both.
    if rs == ZR || rs == rn || rs == rv {
        return Err(Refusal::Registers("the status register is not free"));
    }
    if rt == rn || rv == rn {
        return Err(Refusal::Registers("a value register is the base register"));
    }
    // Register 31 is SP in the base position of both the exclusives and the
    // atomics. An atomic on the stack pointer is not something a compiler
    // emits, and it is not worth reasoning about.
    if rn == ZR {
        return Err(Refusal::Registers("the stack pointer is the base register"));
    }
    let ord = Ordering {
        acquire: load_ord.acquire,
        release: store_ord.release,
    };
    let sf = size.is_64();
    let zero_status = insn::mov_reg(false, rs, ZR);
    // Whether anything outside the sequence can jump straight to its first
    // instruction. The loop's own backward branch does, and that is the only
    // entry an argument about what ran BEFORE the loop may ignore.
    let head = base + 4 * start as u64;
    let entered_from_outside = external_targets.contains(&head)
        || branches
            .iter()
            .any(|(index, target)| *target == head && (*index < start || *index > close));
    let (idiom, words) = match (store - start, decoded[start + 1]) {
        (1, _) => (Idiom::Swap, swap(size, ord, rs, rt, rv, rn, zero_status)?),
        (2, alu @ Insn::Alu { .. }) => (
            Idiom::Rmw,
            rmw(
                alu,
                words[start + 1],
                size,
                ord,
                rs,
                rt,
                rv,
                rn,
                sf,
                zero_status,
            )?,
        ),
        (
            2,
            Insn::CompareBranch {
                nonzero: true,
                sf: branch_sf,
                rt: branch_rt,
                offset: branch_offset,
            },
        ) if branch_rt == rt && branch_sf == sf => (
            Idiom::CasZero,
            cas_zero(size, ord, rt, rv, rn, sf, branch_offset, zero_status)?,
        ),
        (3, cmp) => (
            Idiom::CasCompare,
            cas_compare(
                cmp,
                decoded[start + 2],
                words[start + 1],
                size,
                ord,
                rt,
                rv,
                rn,
                sf,
                zero_status,
                (!entered_from_outside)
                    .then(|| start.checked_sub(1).map(|i| decoded[i]))
                    .flatten(),
            )?,
        ),
        _ => return Err(Refusal::NotCanonical),
    };
    Ok(Site {
        addr: base + 4 * start as u64,
        idiom,
        words,
    })
}

/// `LDxXR Rt, [Rn]; STxXR Rs, Rv, [Rn]; CBNZ Rs` becomes one SWP.
fn swap(
    size: Size,
    ord: Ordering,
    rs: Reg,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    zero_status: u32,
) -> Result<Vec<u32>, Refusal> {
    let _ = rs;
    // `SWPA`/`SWPAL` acquire only when their destination is not the zero
    // register, where `LDAXR WZR` acquires regardless (Arm ARM C6.2, the
    // "Operation" of each; the alias is `STxxx`). A loop that discards its
    // loaded value is rare, so refuse rather than special-case it.
    if rt == ZR {
        return Err(Refusal::Registers("the loaded value is discarded"));
    }
    if rv == rt {
        return Err(Refusal::Registers(
            "the stored register is the load destination",
        ));
    }
    Ok(vec![
        insn::atomic(AtomicOp::Swp, size, ord, rv, rt, rn),
        zero_status,
        insn::NOP,
    ])
}

/// `LDxXR Rt, [Rn]; CBNZ Rt, out; STxXR Rs, Rv, [Rn]; CBNZ Rs` stores only
/// into a zero location, which is a compare-and-swap against zero.
#[allow(clippy::too_many_arguments)]
fn cas_zero(
    size: Size,
    ord: Ordering,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    sf: bool,
    branch_offset: i64,
    zero_status: u32,
) -> Result<Vec<u32>, Refusal> {
    if rt == ZR || rv == rt {
        return Err(Refusal::Registers(
            "the load destination must carry the expected value",
        ));
    }
    // The kept branch sits one instruction later than it did, so its
    // displacement shrinks by that instruction to reach the same target.
    let branch =
        insn::compare_branch(true, sf, rt, branch_offset - 4).ok_or(Refusal::BranchOutOfRange)?;
    Ok(vec![
        insn::mov_reg(sf, rt, ZR),
        insn::cas(size, ord, rt, rv, rn),
        branch,
        zero_status,
    ])
}

#[allow(clippy::too_many_arguments)]
fn rmw(
    alu: Insn,
    alu_word: u32,
    size: Size,
    ord: Ordering,
    rs: Reg,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    sf: bool,
    zero_status: u32,
) -> Result<Vec<u32>, Refusal> {
    // A flag-setting ADDS or SUBS sets the flags from the loaded value. The
    // replacement runs the same instruction on the value the atomic returns,
    // which is the value the original's successful store-exclusive replaced,
    // and nothing after it in the replacement or the original sets flags, so
    // the flags at the exit are the original's. The materialising move and
    // the atomic set none either, and the original sets them before any read.
    let (op, alu_sf, rd, alu_rn, operand) = match alu {
        Insn::Alu {
            op,
            sets_flags: _,
            sf,
            rd,
            rn,
            operand,
        } => (op, sf, rd, rn, operand),
        _ => return Err(Refusal::NotCanonical),
    };
    // The stored value must be what the operation computed from the loaded one.
    if rd != rv || alu_rn != rt || alu_sf != sf {
        return Err(Refusal::NotCanonical);
    }
    // `LD<op>` with the zero register as its destination is the `ST<op>`
    // alias, which performs no load and, in the A and AL forms, takes no
    // acquire (Arm ARM C6.2). An immediate add or subtract also encodes
    // register 31 as SP rather than the zero register, so a loop that loads
    // into the zero register could match an operation on SP.
    if rt == ZR {
        return Err(Refusal::Registers("the loaded value is discarded"));
    }
    // The atomic carries the operand in Rs and the loaded value in Rt. Arm
    // ARM DDI 0487 L.a, C6.2 (the LD<op> instructions, "Constrained
    // unpredictable behavior"): when Rs and Rt are the same register the
    // behaviour is CONSTRAINED UNPREDICTABLE.
    if rs == rt {
        return Err(Refusal::Registers(
            "the status register is the load destination",
        ));
    }
    if let Operand::Reg(rm) = operand {
        if rm == rs {
            return Err(Refusal::Registers(
                "the second operand is the status register",
            ));
        }
        if rm == rt {
            return Err(Refusal::Registers("the second operand is the loaded value"));
        }
    }
    // Materialise the atomic's operand in the status register, which the
    // original loop also writes before anything reads it.
    let (materialise, atomic_op) = match (op, operand) {
        (AluOp::Add, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Add),
        // A plain 12-bit immediate, or one the instruction shifted left by 12,
        // whenever a single MOVZ can build it.
        (AluOp::Add, Operand::Imm(k)) => (
            insn::movz_for(sf, rs, u64::from(k)).ok_or(Refusal::UnsupportedOperation)?,
            AtomicOp::Add,
        ),
        (AluOp::Sub, Operand::Reg(rm)) => (insn::neg(sf, rs, rm), AtomicOp::Add),
        // MOVN materialises -k as !(k - 1), exactly, for every k up to
        // 0x10000, the shifted immediates in that range included. A larger
        // one has no single-instruction negation, because MOVN at a
        // multiple-of-16 shift leaves the bits below the shift set.
        (AluOp::Sub, Operand::Imm(k)) if (1..=0x10000).contains(&k) => {
            (insn::movn(sf, rs, k - 1), AtomicOp::Add)
        }
        (AluOp::Orr, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Set),
        (AluOp::Eor, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Eor),
        (AluOp::Bic, Operand::Reg(rm)) => (insn::mov_reg(sf, rs, rm), AtomicOp::Clr),
        // AND clears the bits that are *not* set in the operand.
        (AluOp::And, Operand::Reg(rm)) => (insn::mvn(sf, rs, rm), AtomicOp::Clr),
        _ => return Err(Refusal::UnsupportedOperation),
    };
    Ok(vec![
        materialise,
        insn::atomic(atomic_op, size, ord, rs, rt, rn),
        alu_word,
        zero_status,
    ])
}

#[allow(clippy::too_many_arguments)]
fn cas_compare(
    cmp: Insn,
    branch: Insn,
    cmp_word: u32,
    size: Size,
    ord: Ordering,
    rt: Reg,
    rv: Reg,
    rn: Reg,
    sf: bool,
    zero_status: u32,
    before: Option<Insn>,
) -> Result<Vec<u32>, Refusal> {
    let (cmp_sf, cmp_rn, operand) = match cmp {
        Insn::Cmp { sf, rn, operand } => (sf, rn, operand),
        _ => return Err(Refusal::NotCanonical),
    };
    // B.NE, condition 0b0001.
    let Insn::CondBranch { cond: 1, offset } = branch else {
        return Err(Refusal::NotCanonical);
    };
    // The kept branch sits one instruction later than it did.
    let branch_word = insn::cond_branch(1, offset - 4).ok_or(Refusal::BranchOutOfRange)?;
    if cmp_rn != rt || cmp_sf != sf {
        return Err(Refusal::NotCanonical);
    }
    if rt == ZR || rv == rt {
        return Err(Refusal::Registers(
            "the load destination must carry the expected value",
        ));
    }
    let expected = match operand {
        Operand::Reg(rm) if rm == rt => {
            return Err(Refusal::Registers("the compared value is the loaded value"));
        }
        // A sub-word compare against a register compares the zero-extended
        // loaded value over the full register, while CASB/CASH compare only
        // the low bits, so the two disagree whenever the register's high bits
        // are set -- unless the instruction just before the loop zero-extended
        // that very register to the access width, which is what the compiler's
        // own outline-atomics helpers do. That argument needs the extension to
        // have RUN, so the caller passes it only when nothing outside the
        // sequence can branch straight to the loop head.
        Operand::Reg(rm)
            if !matches!(size, Size::W | Size::X)
                && !matches!(
                    before,
                    Some(Insn::ZeroExtend { bits, rd, .. })
                        if rd == rm && bits == if matches!(size, Size::B) { 8 } else { 16 }
                ) =>
        {
            let _ = rm;
            return Err(Refusal::Registers("a sub-word compare against a register"));
        }
        Operand::Reg(rm) => insn::mov_reg(sf, rt, rm),
        Operand::Imm(k) if k >= width_limit(size) => {
            return Err(Refusal::Registers(
                "the compared immediate exceeds the access width",
            ));
        }
        Operand::Imm(k) => insn::movz(sf, rt, k),
    };
    Ok(vec![
        expected,
        insn::cas(size, ord, rt, rv, rn),
        cmp_word,
        branch_word,
        zero_status,
    ])
}

fn width_limit(size: Size) -> u32 {
    match size {
        Size::B => 1 << 8,
        Size::H => 1 << 16,
        _ => u32::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // @generated by the arm64 harness probe/mktests.py: every word below is
    // GNU as's own encoding of the sequence named in the first column, so the
    // expectations are the assembler's and not hand-computed. A replacement
    // keeps every instruction at the address the original one had, so a
    // branch the rewrite moves within its sequence is re-encoded to reach the
    // same absolute target.
    const CASES: &[(&str, Idiom, &[u32], &[u32])] = &[
        (
            "rmw_add_imm_x_al",
            Idiom::Rmw,
            &[0xc85ffc64, 0x91000484, 0xc805fc64, 0x35ffffa5],
            &[0xd2800025, 0xf8e50064, 0x91000484, 0x2a1f03e5],
        ),
        (
            "rmw_sub_imm_x_plain",
            Idiom::Rmw,
            &[0xc85f7c64, 0xd1002084, 0xc8057c64, 0x35ffffa5],
            &[0x928000e5, 0xf8250064, 0xd1002084, 0x2a1f03e5],
        ),
        (
            "rmw_and_reg_w_al",
            Idiom::Rmw,
            &[0x885ffc64, 0x0a060084, 0x8805fc64, 0x35ffffa5],
            &[0x2a2603e5, 0xb8e51064, 0x0a060084, 0x2a1f03e5],
        ),
        (
            "rmw_eor_reg_x_release",
            Idiom::Rmw,
            &[0xc85f7c64, 0xca060084, 0xc805fc64, 0x35ffffa5],
            &[0xaa0603e5, 0xf8652064, 0xca060084, 0x2a1f03e5],
        ),
        (
            "rmw_orr_byte_acquire",
            Idiom::Rmw,
            &[0x085ffc20, 0x2a030002, 0x08047c22, 0x35ffffa4],
            &[0x2a0303e4, 0x38a43020, 0x2a030002, 0x2a1f03e4],
        ),
        (
            "rmw_sub_reg_x_al",
            Idiom::Rmw,
            &[0xc85ffc64, 0xcb060084, 0xc805fc64, 0x35ffffa5],
            &[0xcb0603e5, 0xf8e50064, 0xcb060084, 0x2a1f03e5],
        ),
        (
            "swap_w_al",
            Idiom::Swap,
            &[0x885ffc20, 0x8802fc23, 0x35ffffc2],
            &[0xb8e38020, 0x2a1f03e2, 0xd503201f],
        ),
        (
            "swap_byte_acquire_only",
            Idiom::Swap,
            &[0x085ffc20, 0x08027c23, 0x35ffffc2],
            &[0x38a38020, 0x2a1f03e2, 0xd503201f],
        ),
        (
            "cas_compare_gcc_x_al",
            Idiom::CasCompare,
            &[0xc85ffc66, 0xeb0400df, 0x54000081, 0xc807fc62, 0x35ffff87],
            &[0xaa0403e6, 0xc8e6fc62, 0xeb0400df, 0x54000061, 0x2a1f03e7],
        ),
        (
            "cas_compare_llvm_status_is_destination",
            Idiom::CasCompare,
            &[0xc85ffd4c, 0xeb0b019f, 0x540000a1, 0xc80cfd48, 0x35ffff8c],
            &[0xaa0b03ec, 0xc8ecfd48, 0xeb0b019f, 0x54000081, 0x2a1f03ec],
        ),
        (
            "cas_compare_immediate_halfword",
            Idiom::CasCompare,
            &[0x485ffc20, 0x71001c1f, 0x54000081, 0x4802fc23, 0x35ffff82],
            &[0x528000e0, 0x48e0fc23, 0x71001c1f, 0x54000061, 0x2a1f03e2],
        ),
        (
            "cas_zero_x_al",
            Idiom::CasZero,
            &[0xc85ffc20, 0xb5000080, 0xc802fc23, 0x35ffffa2],
            &[0xaa1f03e0, 0xc8e0fc23, 0xb5000060, 0x2a1f03e2],
        ),
        // The two below are an Android linker's own words (Rust's std, linked
        // into it), and their replacements are llvm-mc's encodings.
        // `Arc` drop: ldxr x9,[x8]; subs x9,x9,#1; stlxr w10,x9,[x8]; cbnz w10,
        // whose flags a `b.ne` after the loop reads. It becomes
        // mov x10,#-1; ldaddl x10,x9,[x8]; subs x9,x9,#1; mov w10,wzr.
        (
            "rmw_subs_x_release_arc_drop",
            Idiom::Rmw,
            &[0xc85f7d09, 0xf1000529, 0xc80afd09, 0x35ffffaa],
            &[0x9280000a, 0xf86a0109, 0xf1000529, 0x2a1f03ea],
        ),
        // `ThreadId::new`'s weak compare-exchange: ldxr x10,[x9]; cmp x10,x11;
        // b.ne out; stxr w11,x8,[x9]; cbnz w11,<recompute> - the retry goes
        // back before the load, and the status register is the expected
        // value's. It becomes mov x10,x11; cas x10,x8,[x9]; cmp x10,x11;
        // b.ne out; mov w11,wzr.
        (
            "cas_compare_retry_elsewhere_status_is_expected",
            Idiom::CasCompare,
            &[0xc85f7d2a, 0xeb0b015f, 0x54ffff01, 0xc80b7d28, 0x35fffeeb],
            &[0xaa0b03ea, 0xc8aa7d28, 0xeb0b015f, 0x54fffee1, 0x2a1f03eb],
        ),
    ];

    /// Sequences that must survive untouched, with the reason they do.
    const REFUSALS: &[(&str, &[u32], Refusal)] = &[
        (
            "pair",
            &[0xc87f8440, 0xc8238440, 0x35ffffc3],
            Refusal::ExclusivePair,
        ),
        (
            "compare_branches_on_equal",
            &[0xc85ffc66, 0xeb0400df, 0x54000080, 0xc807fc62, 0x35ffff87],
            Refusal::NotCanonical,
        ),
        (
            "undecoded_instruction_between_the_pair",
            &[0xc85ffc64, 0x9bc67c84, 0xc805fc64, 0x35ffffa5],
            Refusal::Undecoded,
        ),
        (
            "no_loop_branch",
            &[0xc85ffc64, 0x91000484, 0xc805fc64, 0xd503201f],
            Refusal::NotCanonical,
        ),
        (
            "subword_compare_against_a_register",
            &[0x085ffc20, 0x6b04001f, 0x54000081, 0x0802fc23, 0x35ffff82],
            Refusal::Registers("a sub-word compare against a register"),
        ),
        (
            "memory_access_between_the_pair",
            &[0xc85ffc64, 0xf94000e6, 0xc805fc64, 0x35ffffa5],
            Refusal::Undecoded,
        ),
        // The compare-exchange above with its retry at the CMP (cbnz w11,.-12):
        // the replacement would be entered after its first instruction.
        (
            "retry_into_the_sequence",
            &[0xc85f7d2a, 0xeb0b015f, 0x54ffff01, 0xc80b7d28, 0x35ffffab],
            Refusal::NotCanonical,
        ),
        // The linker's `futex_rwlock::read_contended`: ldaxr w9,[x19];
        // cmp w9,w8; b.ne out; add w8,w8,#1; stxr w10,w8,[x19]; cbnz w10,.-52.
        (
            "new_value_after_the_compare",
            &[
                0x885f_fe69,
                0x6b08_013f,
                0x54ff_fe81,
                0x1100_0508,
                0x880a_7e68,
                0x35ff_fe6a,
            ],
            Refusal::NewValueAfterCompare,
        ),
        // The same loop retrying at its own load (cbnz w10,.-16).
        (
            "new_value_after_the_compare_retrying_at_the_load",
            &[
                0x885f_fe69,
                0x6b08_013f,
                0x54ff_fe81,
                0x1100_0508,
                0x880a_7e68,
                0x35ff_ff8a,
            ],
            Refusal::NewValueAfterCompare,
        ),
        // A retry elsewhere with no load-exclusive within three instructions of
        // the store: nop; nop; nop; stxr w11,x8,[x9]; cbnz w11,.-36.
        (
            "retry_elsewhere_without_a_load",
            &[0xd503201f, 0xd503201f, 0xd503201f, 0xc80b7d28, 0x35fffeeb],
            Refusal::NoPairedLoad,
        ),
    ];

    const BASE: u64 = 0x40_0000;

    #[test]
    fn each_canonical_loop_becomes_the_assembler_s_lse_encoding() {
        for (name, idiom, original, expected) in CASES {
            let report = plan(original, BASE);
            assert_eq!(report.refused, vec![], "{name} was refused");
            assert_eq!(report.patched.len(), 1, "{name} matched {report:?}");
            let site = &report.patched[0];
            assert_eq!(site.idiom, *idiom, "{name}");
            assert_eq!(site.addr, BASE, "{name}");
            assert_eq!(
                site.words,
                expected.to_vec(),
                "{name}: got [{}], want [{}]",
                site.words
                    .iter()
                    .map(|w| format!("{w:#010x}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                expected
                    .iter()
                    .map(|w| format!("{w:#010x}"))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            let mut words = original.to_vec();
            apply(&mut words, BASE, &report);
            assert_eq!(
                &words[..expected.len()],
                expected.to_vec(),
                "{name} applied"
            );
            // The rewritten code holds no exclusive instruction, so nothing in
            // it can retry.
            let after = plan(&words, BASE);
            assert_eq!(after.exclusive_instructions, 0, "{name} after rewriting");
        }
    }

    #[test]
    fn a_sequence_outside_the_idiom_set_is_left_alone_with_a_reason() {
        for (name, words, reason) in REFUSALS {
            let report = plan(words, BASE);
            assert_eq!(report.patched, vec![], "{name} was rewritten");
            assert_eq!(
                report.refused.iter().map(|r| &r.reason).collect::<Vec<_>>(),
                vec![reason],
                "{name}"
            );
        }
    }

    #[test]
    fn a_branch_into_the_middle_of_a_sequence_refuses_it() {
        let (_, _, original, _) = CASES[0];
        // `b .+8` lands on the second instruction of the loop that follows it.
        let mut words = vec![0x1400_0002];
        words.extend_from_slice(original);
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::BranchIntoSequence)
        );

        // The loop's own backward branch targets its first instruction, which
        // is where the replacement starts too, so it does not refuse it.
        let report = plan(original, BASE);
        assert_eq!(report.patched.len(), 1);
    }

    #[test]
    fn a_loop_inside_a_larger_region_is_found_at_its_own_address() {
        let (_, _, original, expected) = CASES[8];
        let mut words = vec![insn::NOP, insn::NOP];
        words.extend_from_slice(original);
        words.push(insn::NOP);
        let report = plan(&words, BASE);
        assert_eq!(report.patched.len(), 1, "{report:?}");
        assert_eq!(report.patched[0].addr, BASE + 8);
        apply(&mut words, BASE, &report);
        assert_eq!(
            &words[2..2 + expected.len()],
            expected.to_vec(),
            "applied at an offset"
        );
        assert_eq!(words[0], insn::NOP);
        assert_eq!(*words.last().unwrap(), insn::NOP);
    }

    #[test]
    fn an_immediate_the_instruction_shifted_by_twelve_is_materialised() {
        // ldxr x9,[x8]; add x9,x9,#0x100,lsl #12; stxr w10,x9,[x8]; cbnz w10
        // becomes movz x10,#0x10,lsl #16; ldadd x10,x9,[x8]; <the add>; mov w10,wzr.
        let words = [0xc85f_7d09, 0x9144_0129, 0xc80a_7d09, 0x35ff_ffaa];
        let report = plan(&words, BASE);
        assert_eq!(report.refused, vec![]);
        assert_eq!(
            report.patched[0].words,
            vec![0xd2a0_020a, 0xf82a_0109, 0x9144_0129, 0x2a1f_03ea]
        );

        // The same subtraction has no one-instruction negation: MOVN yields one
        // less than a multiple of 4096.
        let words = [0xc85f_7d09, 0xd160_0129, 0xc80a_7d09, 0x35ff_ffaa];
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::UnsupportedOperation)
        );
    }

    #[test]
    fn a_zero_extension_before_the_loop_admits_a_sub_word_register_compare() {
        // uxtb w16,w0; ldaxrb w0,[x2]; cmp w0,w16; b.ne out; stlxrb w17,w1,[x2]; cbnz w17
        // is the compiler's own byte compare-and-swap helper: the zero extension
        // proves the high bits of w16 are clear, so CASALB compares what CMP does.
        let loop_words = [
            0x085f_fc40,
            0x6b10_001f,
            0x5400_0081,
            0x0811_fc41,
            0x35ff_ff91,
        ];
        let mut words = vec![0x5300_1c10];
        words.extend_from_slice(&loop_words);
        let report = plan(&words, BASE);
        assert_eq!(report.refused, vec![], "{report:?}");
        assert_eq!(
            report.patched[0].words,
            vec![
                0x2a10_03e0,
                0x08e0_fc41,
                0x6b10_001f,
                0x5400_0061,
                0x2a1f_03f1
            ]
        );

        // Without the zero extension the same loop stays refused.
        let report = plan(&loop_words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::Registers("a sub-word compare against a register"))
        );

        // The reviewer's repro: a direct branch to the loop head skips the
        // zero extension, so the proof it carries does not hold and the loop
        // must stay refused. With w16 = 0x141 and the byte in memory 0x41,
        // the original branches out and leaves memory alone, while a CASALB
        // comparing only the low byte would store.
        let mut words = vec![0x1400_0002u32, 0x5300_1c10];
        words.extend_from_slice(&loop_words);
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![], "{report:?}");
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::Registers("a sub-word compare against a register"))
        );

        // A branch to the zero extension itself is what the real helpers do,
        // and it keeps the proof: the extension still runs.
        let mut words = vec![0x1400_0001u32, 0x5300_1c10];
        words.extend_from_slice(&loop_words);
        let report = plan(&words, BASE);
        assert_eq!(report.patched.len(), 1, "{report:?}");

        // An outside branch to the head also refuses it when it comes from
        // another executable section.
        let mut head_targets = HashSet::new();
        head_targets.insert(BASE + 4);
        let mut words = vec![0x5300_1c10u32];
        words.extend_from_slice(&loop_words);
        let report = plan_with_external_targets(&words, BASE, &head_targets);
        assert_eq!(report.patched, vec![], "{report:?}");

        // A zero extension of the wrong width, or of another register, does not.
        for before in [0x5300_3c10u32, 0x5300_1c13] {
            let mut words = vec![before];
            words.extend_from_slice(&loop_words);
            let report = plan(&words, BASE);
            assert_eq!(report.patched, vec![], "{before:#010x}");
        }
    }

    #[test]
    fn the_flags_an_arc_drop_leaves_are_kept_for_the_branch_after_it() {
        // The linker's `Arc` drop and the `b.ne` after it that reads the flags
        // the loop's SUBS set: only the loop is replaced, and the replacement's
        // SUBS is the original instruction, so the branch reads the same flags.
        let words = [
            0xc85f_7d09,
            0xf100_0529,
            0xc80a_fd09,
            0x35ff_ffaa,
            0x5400_00a1,
        ];
        let report = plan(&words, BASE);
        assert_eq!(report.refused, vec![], "{report:?}");
        assert_eq!(report.patched.len(), 1);
        let site = &report.patched[0];
        assert_eq!(site.words.len(), 4);
        assert_eq!(site.words[2], words[1], "the SUBS runs again, unchanged");
        let mut rewritten = words;
        apply(&mut rewritten, BASE, &report);
        assert_eq!(rewritten[4], words[4], "the flag consumer is untouched");
    }

    #[test]
    fn a_compare_exchange_that_retries_elsewhere_keeps_its_exits() {
        // `ThreadId::new`'s loop in a region with its recompute block before
        // it, as the linker lays it out: the retry target and the B.NE target
        // stay outside the replacement, so nothing enters it part-way.
        // mov w12,wzr; mov x11,x10; adds x8,x11,#1; <the loop>; mov w12,#1.
        let mut words = vec![0x2a1f_03ecu32, 0xaa0a_03eb, 0xb100_0568];
        // ldxr x10,[x9]; cmp x10,x11; b.ne <before the region>;
        // stxr w11,x8,[x9]; cbnz w11,<the recompute block's first instruction>.
        words.extend_from_slice(&[
            0xc85f_7d2a,
            0xeb0b_015f,
            0x54ff_ff41,
            0xc80b_7d28,
            0x35ff_ff2b,
        ]);
        words.push(0x5280_002c);
        let report = plan(&words, BASE);
        assert_eq!(report.refused, vec![], "{report:?}");
        assert_eq!(report.patched.len(), 1);
        assert_eq!(report.patched[0].addr, BASE + 12);
        assert_eq!(report.patched[0].idiom, Idiom::CasCompare);
    }

    #[test]
    fn a_loop_that_discards_its_loaded_value_is_refused() {
        // ldaxr xzr, [x3]; add x4, x31, #1; stlxr w5, x4, [x3]; cbnz w5.
        // Register 31 is SP in the immediate add and the zero register in the
        // exclusive, and `LDADD` into the zero register neither loads nor
        // acquires, so this must not be rewritten.
        let words = [0xc85f_fc7f, 0x9100_07e4, 0xc805_fc64, 0x35ff_ffa5];
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::Registers("the loaded value is discarded"))
        );

        // ldaxr xzr, [x1]; stlxr w2, x3, [x1]; cbnz w2: the swap form.
        let words = [0xc85f_fc3f, 0xc802_fc23, 0x35ff_ffc2];
        let report = plan(&words, BASE);
        assert_eq!(report.patched, vec![]);
        assert_eq!(
            report.refused.first().map(|r| r.reason.clone()),
            Some(Refusal::Registers("the loaded value is discarded"))
        );
    }

    #[test]
    fn every_ordering_combination_maps_to_the_matching_atomic() {
        // ldxr/ldaxr x4,[x3]; add x4,x4,#1; stxr/stlxr w5,x4,[x3]; cbnz w5.
        let orderings = [
            (0xc85f7c64u32, 0xc8057c64u32, 0xf8250064u32), // plain
            (0xc85ffc64, 0xc8057c64, 0xf8a50064),          // acquire only
            (0xc85f7c64, 0xc805fc64, 0xf8650064),          // release only
            (0xc85ffc64, 0xc805fc64, 0xf8e50064),          // acquire-release
        ];
        for (load, store, atomic) in orderings {
            let words = [load, 0x9100_0484, store, 0x35ff_ffa5];
            let report = plan(&words, BASE);
            assert_eq!(report.patched.len(), 1, "{load:#010x}/{store:#010x}");
            assert_eq!(
                report.patched[0].words[1], atomic,
                "{load:#010x}/{store:#010x} produced {:#010x}",
                report.patched[0].words[1]
            );
        }
    }
}
