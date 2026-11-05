/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Precommit argument accounting for Linux x86-64 exec and script rewrites.
//!
//! Linux computes its pointer allowance from the original argument counts
//! exactly once. Script rewrites consume the remaining string allowance
//! without recomputing that original pointer allowance. Executing a launcher
//! with the rewritten arguments requires a separate, potentially tighter
//! allowance. These two limits have deliberately different error classes.
//! A separate ordered page model enforces the nascent stack's growth limit;
//! the byte allowance's `ARG_MAX` floor does not exempt that VMA from
//! `RLIMIT_STACK` when another argument page is needed.

use std::ffi::CStr;
use std::ffi::CString;
use std::fmt;

/// Linux `MAX_ARG_STRLEN`, including the terminating NUL, on 4 KiB pages.
pub const MAX_ARG_STRLEN: usize = 32 * 4096;
/// Linux `MAX_ARG_STRINGS`.
pub const MAX_ARG_STRINGS: usize = 0x7fff_ffff;
/// Historical minimum string/pointer budget (`ARG_MAX`).
pub const MIN_ARGUMENT_BUDGET: u64 = 32 * 4096;
/// Three quarters of Linux's 8 MiB `_STK_LIM`.
pub const MAX_ARGUMENT_BUDGET: u64 = 6 * 1024 * 1024;

/// Audited Linux x86-64 argument-stack page size.
pub const ARGUMENT_PAGE_SIZE: u64 = 4096;
const INITIAL_STACK_PADDING: u64 = 8;

/// Ordered argument-page growth before Linux commits an exec.
///
/// Linux starts with one VMA page and `bprm->p = vm_end - sizeof(void *)`.
/// Each string copy grows that VMA downwards as needed. `remove_arg_zero`
/// advances `p` without shrinking the VMA. A growth attempt whose complete
/// VMA size exceeds `RLIMIT_STACK` makes `get_arg_page` fail and returns E2BIG.
/// This is independent of [`ArgumentPlan`]'s string/pointer allowance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgumentPages {
    stack_limit: u64,
    string_bytes: u64,
    pages: u64,
}

impl ArgumentPages {
    /// Replay the original filename, reversed environment, reversed argv,
    /// and empty-argv normalization in `do_execveat_common` order.
    ///
    /// This models only argument VMA growth under `RLIMIT_STACK`. Other
    /// resource failures remain the actual original CHECK's responsibility.
    pub fn new(
        argv: &[CString],
        envp: &[CString],
        execfn: &CStr,
        stack_limit: u64,
    ) -> Result<Self, ArgumentError> {
        let mut state = Self {
            stack_limit,
            string_bytes: 0,
            pages: 1,
        };
        state.copy_string(execfn)?;
        for string in envp.iter().rev().chain(argv.iter().rev()) {
            state.copy_string(string)?;
        }
        if argv.is_empty() {
            state.copy_string(c"")?;
        }
        Ok(state)
    }

    /// Apply `remove_arg_zero` and each subsequent kernel string copy before
    /// a script interpreter is opened. The state is unchanged on failure.
    ///
    /// `argv_zero` must be the current argument plan's first string, including
    /// the normalized empty string when original argv was empty.
    pub fn rewrite_script(
        &mut self,
        argv_zero: Option<&CStr>,
        interpreter: &CStr,
        optional: Option<&CStr>,
        current_script_name: &CStr,
    ) -> Result<(), ArgumentError> {
        let mut next = self.clone();
        if let Some(first) = argv_zero {
            next.string_bytes = next
                .string_bytes
                .checked_sub(first.to_bytes_with_nul().len() as u64)
                .ok_or(ArgumentError::NativeE2big)?;
        }
        next.copy_string(current_script_name)?;
        if let Some(argument) = optional {
            next.copy_string(argument)?;
        }
        next.copy_string(interpreter)?;
        *self = next;
        Ok(())
    }

    /// Pages retained by the temporary argument VMA.
    pub fn pages(&self) -> u64 {
        self.pages
    }

    /// Current string bytes below the initial `bprm->p`.
    pub fn string_bytes(&self) -> u64 {
        self.string_bytes
    }

    fn copy_string(&mut self, string: &CStr) -> Result<(), ArgumentError> {
        let length = checked_string_length(string).ok_or(ArgumentError::NativeE2big)?;
        let used = self
            .string_bytes
            .checked_add(length)
            .ok_or(ArgumentError::NativeE2big)?;
        let span = used
            .checked_add(INITIAL_STACK_PADDING)
            .ok_or(ArgumentError::NativeE2big)?;
        let pages = span.div_ceil(ARGUMENT_PAGE_SIZE);
        if pages > self.pages {
            let size = pages
                .checked_mul(ARGUMENT_PAGE_SIZE)
                .ok_or(ArgumentError::NativeE2big)?;
            if size > self.stack_limit {
                return Err(ArgumentError::NativeE2big);
            }
            self.pages = pages;
        }
        self.string_bytes = used;
        Ok(())
    }
}

/// Argument failure before any exec commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentError {
    /// The native byte allowance, argument-page growth, or string limit fails.
    NativeE2big,
    /// The native request fits but its rewritten launcher invocation does not.
    LauncherArgumentBudget,
}

impl fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeE2big => f.write_str("native exec argument limit (E2BIG)"),
            Self::LauncherArgumentBudget => {
                f.write_str("rewritten launcher argument budget is insufficient")
            }
        }
    }
}

impl std::error::Error for ArgumentError {}

/// The allowance for one original or launcher exec call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgumentBudget {
    /// Total pointer/string allowance derived from `RLIMIT_STACK`.
    pub limit: u64,
    /// Pointer allowance, excluding the argv/envp terminating NULLs.
    pub pointer_bytes: u64,
    /// Bytes available to string copies, including every terminating NUL.
    pub string_limit: u64,
    /// String bytes actually required, including the saved exec filename.
    pub string_bytes: u64,
}

/// A native string budget and the current arguments after zero or more scripts.
#[derive(Clone, Debug)]
pub struct ArgumentPlan {
    /// Current argv; an originally empty argv has a single empty string.
    pub argv: Vec<CString>,
    /// Original environment, preserved through script rewriting.
    pub envp: Vec<CString>,
    /// Original F, retained independently of each script interpreter name.
    pub execfn: CString,
    /// Original user argument count, before empty-argv normalization.
    pub original_argc: usize,
    /// Original environment count.
    pub original_envc: usize,
    /// Native total pointer/string allowance.
    pub native_limit: u64,
    /// Native pointer allowance; never increased by a script rewrite.
    pub native_pointer_bytes: u64,
    /// Native allowance available to string copies.
    pub native_string_limit: u64,
    /// Current native string usage, including the original F and its NUL.
    pub string_bytes: u64,
}

impl ArgumentPlan {
    /// Account for the original user argv/envp and kernel filename copy.
    ///
    /// `execfn` is Linux's original `bprm->filename`, including a synthesized
    /// `/dev/fd/N/` prefix for a relative execveat call. It is copied in addition
    /// to argv, even when `argv[0]` has the same bytes. `stack_limit` is the soft
    /// `RLIMIT_STACK` in the process performing the exec.
    pub fn new(
        argv: &[CString],
        envp: &[CString],
        execfn: &CStr,
        stack_limit: u64,
    ) -> Result<Self, ArgumentError> {
        let native_limit = argument_limit(stack_limit);
        let native_pointer_bytes =
            pointer_bytes(argv.len(), envp.len()).ok_or(ArgumentError::NativeE2big)?;
        let native_string_limit = native_limit
            .checked_sub(native_pointer_bytes)
            .filter(|remaining| *remaining != 0)
            .ok_or(ArgumentError::NativeE2big)?;
        // do_execveat_common copies F, then reversed envp, then reversed argv.
        // For owned strings only the accumulated usage can fail; preserve that
        // order here so later additions do not invent a different native rule.
        let mut string_bytes = 0;
        add_native_string(&mut string_bytes, execfn, native_string_limit)?;
        for string in envp.iter().rev().chain(argv.iter().rev()) {
            add_native_string(&mut string_bytes, string, native_string_limit)?;
        }
        let mut normalized_argv = argv.to_vec();
        if normalized_argv.is_empty() {
            let empty = CString::default();
            add_native_string(&mut string_bytes, &empty, native_string_limit)?;
            normalized_argv.push(empty);
        }
        Ok(Self {
            argv: normalized_argv,
            envp: envp.to_vec(),
            execfn: execfn.to_owned(),
            original_argc: argv.len(),
            original_envc: envp.len(),
            native_limit,
            native_pointer_bytes,
            native_string_limit,
            string_bytes,
        })
    }

    /// Apply one `binfmt_script` argument rewrite using the original allowance.
    ///
    /// The kernel first removes `argv[0]`, then copies the current script name,
    /// the optional argument as one whole string, and the interpreter name.
    /// Check each of those copies before publishing the rewritten vectors. On
    /// failure this plan is unchanged. Parsing and the inaccessible-CLOEXEC
    /// script-name check belong to the caller and precede this method.
    pub fn rewrite_script(
        &mut self,
        interpreter: &CStr,
        optional: Option<&CStr>,
        current_script_name: &CStr,
    ) -> Result<(), ArgumentError> {
        let mut string_bytes = self.string_bytes;
        if let Some(first) = self.argv.first() {
            string_bytes = string_bytes
                .checked_sub(first.as_bytes_with_nul().len() as u64)
                .ok_or(ArgumentError::NativeE2big)?;
        }
        add_native_string(
            &mut string_bytes,
            current_script_name,
            self.native_string_limit,
        )?;
        if let Some(argument) = optional {
            add_native_string(&mut string_bytes, argument, self.native_string_limit)?;
        }
        add_native_string(&mut string_bytes, interpreter, self.native_string_limit)?;

        let removed = usize::from(!self.argv.is_empty());
        let added = 2 + usize::from(optional.is_some());
        let argc = self
            .argv
            .len()
            .checked_sub(removed)
            .and_then(|remaining| remaining.checked_add(added))
            .ok_or(ArgumentError::NativeE2big)?;
        let mut argv = Vec::with_capacity(argc);
        argv.push(interpreter.to_owned());
        if let Some(argument) = optional {
            argv.push(argument.to_owned());
        }
        argv.push(current_script_name.to_owned());
        argv.extend(self.argv.iter().skip(removed).cloned());
        self.argv = argv;
        self.string_bytes = string_bytes;
        Ok(())
    }

    /// Check the real launcher's rewritten argv/envp pointer/string allowance.
    ///
    /// A launcher filename padded to F's exact length uses the same filename
    /// string bytes. Future added arguments/environment consume both their
    /// string bytes and an additional pointer. Their order does not affect the
    /// size check. This never changes the original native pointer allowance.
    pub fn launcher_budget(
        &self,
        additional_argv: &[CString],
        additional_envp: &[CString],
    ) -> Result<ArgumentBudget, ArgumentError> {
        let failure = ArgumentError::LauncherArgumentBudget;
        let argc = self
            .argv
            .len()
            .checked_add(additional_argv.len())
            .ok_or(failure)?;
        let envc = self
            .envp
            .len()
            .checked_add(additional_envp.len())
            .ok_or(failure)?;
        let pointer_bytes = pointer_bytes(argc, envc).ok_or(failure)?;
        let string_limit = self
            .native_limit
            .checked_sub(pointer_bytes)
            .filter(|remaining| *remaining != 0)
            .ok_or(failure)?;
        let mut string_bytes = self.string_bytes;
        if string_bytes > string_limit {
            return Err(failure);
        }
        for string in additional_argv.iter().chain(additional_envp.iter()) {
            let length = checked_string_length(string).ok_or(failure)?;
            string_bytes = string_bytes.checked_add(length).ok_or(failure)?;
            if string_bytes > string_limit {
                return Err(failure);
            }
        }
        Ok(ArgumentBudget {
            limit: self.native_limit,
            pointer_bytes,
            string_limit,
            string_bytes,
        })
    }
}

/// Linux `bprm_stack_limits`: max(ARG_MAX, min(3/4 _STK_LIM, stack/4)).
pub fn argument_limit(stack_limit: u64) -> u64 {
    (stack_limit / 4).clamp(MIN_ARGUMENT_BUDGET, MAX_ARGUMENT_BUDGET)
}

fn pointer_bytes(argc: usize, envc: usize) -> Option<u64> {
    if argc > MAX_ARG_STRINGS || envc > MAX_ARG_STRINGS {
        return None;
    }
    let pointers = argc.max(1).checked_add(envc)?;
    u64::try_from(pointers).ok()?.checked_mul(8)
}

fn checked_string_length(string: &CStr) -> Option<u64> {
    let length = string.to_bytes_with_nul().len();
    if length > MAX_ARG_STRLEN {
        return None;
    }
    u64::try_from(length).ok()
}

fn add_native_string(used: &mut u64, string: &CStr, limit: u64) -> Result<(), ArgumentError> {
    let length = checked_string_length(string).ok_or(ArgumentError::NativeE2big)?;
    let next = used
        .checked_add(length)
        .filter(|next| *next <= limit)
        .ok_or(ArgumentError::NativeE2big)?;
    *used = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(bytes: &[u8]) -> CString {
        CString::new(bytes).unwrap()
    }

    #[test]
    fn stack_limit_floor_quarter_and_cap() {
        assert_eq!(argument_limit(0), MIN_ARGUMENT_BUDGET);
        assert_eq!(argument_limit(512 * 1024), MIN_ARGUMENT_BUDGET);
        assert_eq!(argument_limit(512 * 1024 + 3), MIN_ARGUMENT_BUDGET);
        assert_eq!(argument_limit(512 * 1024 + 4), MIN_ARGUMENT_BUDGET + 1);
        assert_eq!(argument_limit(8 * 1024 * 1024), 2 * 1024 * 1024);
        assert_eq!(argument_limit(24 * 1024 * 1024), MAX_ARGUMENT_BUDGET);
        assert_eq!(argument_limit(libc::RLIM_INFINITY), MAX_ARGUMENT_BUDGET);
    }

    #[test]
    fn script_growth_needs_a_page_beyond_original_check() {
        let execfn = string(&[b's'; 199]);
        let argv = [string(b"a"), string(&vec![b'x'; 65325])];
        let mut plan = ArgumentPlan::new(&argv, &[], &execfn, 65536).unwrap();
        let mut pages = ArgumentPages::new(&argv, &[], &execfn, 65536).unwrap();
        assert_eq!(pages.string_bytes(), 65528);
        assert_eq!(pages.pages(), 16);
        let before = pages.clone();
        assert_eq!(
            pages.rewrite_script(plan.argv.first().map(|s| s.as_c_str()), c"i", None, &execfn),
            Err(ArgumentError::NativeE2big)
        );
        assert_eq!(
            pages, before,
            "failed growth leaves the page model unchanged"
        );
        plan.rewrite_script(c"i", None, &execfn).unwrap();
        assert!(plan.launcher_budget(&[], &[]).is_ok());

        let mut admitted = ArgumentPages::new(&argv, &[], &execfn, 17 * 4096).unwrap();
        admitted
            .rewrite_script(Some(c"a"), c"i", None, &execfn)
            .unwrap();
        assert_eq!(admitted.pages(), 17);
        assert_eq!(admitted.string_bytes(), plan.string_bytes);
    }

    #[test]
    fn argument_pages_include_top_word_and_round_growth_to_pages() {
        let initial = [string(&vec![b'x'; 65524])];
        let mut pages = ArgumentPages::new(&initial, &[], c"F", 65536).unwrap();
        assert_eq!(pages.string_bytes(), 65527);
        assert_eq!(pages.pages(), 16);
        pages
            .rewrite_script(Some(&initial[0]), c"i", None, c"F")
            .unwrap();
        assert_eq!(pages.pages(), 16, "removing argv0 does not shrink the VMA");

        // A byte below the next page-size boundary still needs another page
        // because create_init_stack_vma reserves sizeof(void *) at the top.
        let crossing = [string(&vec![b'x'; 65526])];
        assert_eq!(
            ArgumentPages::new(&crossing, &[], c"F", 65536).unwrap_err(),
            ArgumentError::NativeE2big
        );
        assert!(ArgumentPages::new(&crossing, &[], c"F", 17 * 4096 - 1).is_err());
        assert!(ArgumentPages::new(&crossing, &[], c"F", 17 * 4096).is_ok());
    }

    #[test]
    fn initial_argument_page_exists_without_growth_limit_check() {
        let argv = [string(&vec![b'x'; 4085])];
        let pages = ArgumentPages::new(&argv, &[], c"F", 0).unwrap();
        assert_eq!(pages.string_bytes(), 4088);
        assert_eq!(pages.pages(), 1);
        let argv = [string(&vec![b'x'; 4086])];
        assert_eq!(
            ArgumentPages::new(&argv, &[], c"F", 0).unwrap_err(),
            ArgumentError::NativeE2big
        );
    }

    #[test]
    fn original_pointer_allowance_and_empty_argv() {
        let plan = ArgumentPlan::new(&[], &[string(b"A=B")], c"F", 0).unwrap();
        assert_eq!(plan.original_argc, 0);
        assert_eq!(plan.original_envc, 1);
        assert_eq!(plan.native_pointer_bytes, 16);
        assert_eq!(plan.argv, [CString::default()]);
        assert_eq!(plan.string_bytes, 2 + 4 + 1);
        assert_eq!(plan.execfn, c"F".to_owned());
        assert_eq!(plan.launcher_budget(&[], &[]).unwrap().pointer_bytes, 16);
        assert_eq!(pointer_bytes(MAX_ARG_STRINGS + 1, 0), None);
        assert_eq!(pointer_bytes(0, MAX_ARG_STRINGS + 1), None);
        assert_eq!(pointer_bytes(usize::MAX, usize::MAX), None);
    }

    #[test]
    fn exact_native_string_band_accepts_equality() {
        // Two original argv pointers; F and argv[0] each need two bytes.
        let available = MIN_ARGUMENT_BUDGET as usize - 16 - 4;
        let argv = [string(b"a"), string(&vec![b'x'; available - 1])];
        let plan = ArgumentPlan::new(&argv, &[], c"F", 0).unwrap();
        assert_eq!(plan.string_bytes, plan.native_string_limit);
        assert_eq!(
            plan.launcher_budget(&[], &[]).unwrap().string_limit,
            plan.string_bytes
        );
        let argv = [string(b"a"), string(&vec![b'x'; available])];
        assert_eq!(
            ArgumentPlan::new(&argv, &[], c"F", 0).unwrap_err(),
            ArgumentError::NativeE2big
        );
    }

    #[test]
    fn nested_rewrites_preserve_original_f_and_pointer_allowance() {
        let mut plan = ArgumentPlan::new(
            &[string(b"old-zero"), string(b"tail")],
            &[string(b"E=V")],
            c"/original-script",
            8 * 1024 * 1024,
        )
        .unwrap();
        let allowance = plan.native_pointer_bytes;
        plan.rewrite_script(
            c"/script-two",
            Some(c"one argument with spaces"),
            c"/original-script",
        )
        .unwrap();
        plan.rewrite_script(c"/final-elf", Some(c"second"), c"/script-two")
            .unwrap();
        assert_eq!(
            plan.argv,
            [
                string(b"/final-elf"),
                string(b"second"),
                string(b"/script-two"),
                string(b"one argument with spaces"),
                string(b"/original-script"),
                string(b"tail"),
            ]
        );
        assert_eq!(plan.execfn, c"/original-script".to_owned());
        assert_eq!(plan.native_pointer_bytes, allowance);
        assert_eq!(allowance, 24);
        let bytes = plan.execfn.as_bytes_with_nul().len()
            + plan
                .argv
                .iter()
                .map(|value| value.as_bytes_with_nul().len())
                .sum::<usize>()
            + plan
                .envp
                .iter()
                .map(|value| value.as_bytes_with_nul().len())
                .sum::<usize>();
        assert_eq!(plan.string_bytes, bytes as u64);
        assert_eq!(plan.launcher_budget(&[], &[]).unwrap().pointer_bytes, 56);
    }

    #[test]
    fn script_accepts_native_boundary_but_refuses_launcher_pointer_growth() {
        // At the original two-pointer string boundary, replacing "a" with
        // interpreter "i" and script "F" adds exactly two string bytes.
        let tail_bytes = MIN_ARGUMENT_BUDGET as usize - 16 - 6;
        let mut plan = ArgumentPlan::new(
            &[string(b"a"), string(&vec![b'x'; tail_bytes - 1])],
            &[],
            c"F",
            0,
        )
        .unwrap();
        plan.rewrite_script(c"i", None, c"F").unwrap();
        assert_eq!(plan.argv.len(), 3);
        assert_eq!(plan.native_pointer_bytes, 16);
        assert_eq!(plan.string_bytes, plan.native_string_limit);
        assert_eq!(
            plan.launcher_budget(&[], &[]),
            Err(ArgumentError::LauncherArgumentBudget)
        );
        // Recomputing pointers inside rewrite_script would wrongly reject
        // this native-valid request as E2BIG instead of the named refusal.
    }

    #[test]
    fn script_failure_is_atomic_and_checks_copy_lengths() {
        let mut plan = ArgumentPlan::new(&[], &[], c"F", 0).unwrap();
        let before = plan.clone();
        let oversized = string(&vec![b'x'; MAX_ARG_STRLEN]);
        assert_eq!(
            plan.rewrite_script(c"i", Some(&oversized), c"F"),
            Err(ArgumentError::NativeE2big)
        );
        assert_eq!(plan.argv, before.argv);
        assert_eq!(plan.string_bytes, before.string_bytes);
        plan.rewrite_script(c"i", None, c"F").unwrap();
        assert_eq!(plan.argv, [string(b"i"), string(b"F")]);
    }

    #[test]
    fn augmentation_has_its_own_failure_class() {
        let plan = ArgumentPlan::new(&[string(b"a")], &[], c"F", 0).unwrap();
        let budget = plan
            .launcher_budget(&[string(b"extra")], &[string(b"KEY=VALUE")])
            .unwrap();
        assert_eq!(budget.pointer_bytes, 24);
        assert_eq!(budget.string_bytes, plan.string_bytes + 6 + 10);
        let oversized = string(&vec![b'x'; MAX_ARG_STRLEN]);
        assert_eq!(
            plan.launcher_budget(&[], &[oversized]),
            Err(ArgumentError::LauncherArgumentBudget)
        );
    }

    #[test]
    fn single_argument_length_boundary() {
        let valid = [
            string(b"/bin/true"),
            string(&vec![b'x'; MAX_ARG_STRLEN - 1]),
        ];
        assert!(ArgumentPlan::new(&valid, &[], c"/bin/true", 8 * 1024 * 1024).is_ok());
        let invalid = [string(b"/bin/true"), string(&vec![b'x'; MAX_ARG_STRLEN])];
        assert_eq!(
            ArgumentPlan::new(&invalid, &[], c"/bin/true", 8 * 1024 * 1024).unwrap_err(),
            ArgumentError::NativeE2big
        );
    }
}
