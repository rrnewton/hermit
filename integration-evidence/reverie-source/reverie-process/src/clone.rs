/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use syscalls::Errno;

use super::Pid;

pub(super) const CHILD_STACK_SIZE: usize = 2 * 1024 * 1024;

pub(super) fn child_stack() -> Vec<u8> {
    vec![0u8; CHILD_STACK_SIZE]
}

pub fn clone<F>(cb: F, flags: libc::c_int) -> Result<Pid, Errno>
where
    F: FnMut() -> i32,
{
    // The child runs container setup and libc's exec path on this stack. In an
    // optimized build, Mount::mount alone can reserve PATH_MAX bytes in its
    // frame, so one page cannot hold that call plus its callers. Match the
    // stack size Container::run provides for the same setup path, and allocate
    // it before clone so the child remains allocation-free before exec.
    let mut stack = child_stack();
    clone_with_stack(cb, flags, &mut stack)
}

pub fn clone_with_stack<F>(cb: F, flags: libc::c_int, stack: &mut [u8]) -> Result<Pid, Errno>
where
    F: FnMut() -> i32,
{
    type CloneCb<'a> = Box<dyn FnMut() -> i32 + 'a>;

    extern "C" fn callback(data: *mut CloneCb) -> libc::c_int {
        let cb: &mut CloneCb = unsafe { &mut *data };
        (*cb)() as libc::c_int
    }

    let mut cb: CloneCb = Box::new(cb);

    let res = unsafe {
        let stack = stack.as_mut_ptr().add(stack.len());
        let stack = stack.sub(stack as usize % 16);

        libc::clone(
            core::mem::transmute::<
                extern "C" fn(*mut Box<dyn FnMut() -> i32>) -> i32,
                extern "C" fn(*mut libc::c_void) -> libc::c_int,
            >(callback as extern "C" fn(*mut Box<dyn FnMut() -> i32>) -> i32),
            stack as *mut libc::c_void,
            flags,
            &mut cb as *mut _ as *mut libc::c_void,
        )
    };

    Errno::result(res).map(Pid::from_raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_child_stack_keeps_the_container_run_minimum() {
        assert!(
            child_stack().len() >= 2 * 1024 * 1024,
            "the cloned child runs container setup before exec and needs at least 2 MiB"
        );
    }
}
