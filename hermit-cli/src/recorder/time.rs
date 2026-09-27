/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Error;
use reverie::Guest;
use reverie::syscalls::ClockGettime;
use reverie::syscalls::Gettimeofday;
use reverie::syscalls::Time;

use super::Recorder;
use crate::clock_output::Destination;
use crate::event::ClockEvent;
use crate::event::GettimeofdayEvent;
use crate::event::SyscallEvent;

// Each handler reads its destinations before injecting the call, because an
// EFAULT event needs the pre-call bytes to prove which bytes Linux stored.
// `Destination::pre_call` states the cost of that read.
impl Recorder {
    pub(super) async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGettime,
    ) -> Result<i64, Error> {
        let tp = Destination::new("clock_gettime", "tp", syscall.tp().map(|pointer| pointer.0));
        let tp_before = tp.pre_call(&guest.memory())?;
        let result = guest.inject(syscall).await;
        let output = tp.capture(&guest.memory(), result, tp_before)?;
        self.record_event(
            guest,
            Ok(SyscallEvent::ClockGettimeV2(ClockEvent { result, output })),
        );
        result.map_err(Error::from)
    }

    pub(super) async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Time,
    ) -> Result<i64, Error> {
        let tloc = Destination::new("time", "tloc", syscall.tloc());
        let tloc_before = tloc.pre_call(&guest.memory())?;
        let result = guest.inject(syscall).await;
        let output = tloc.capture(&guest.memory(), result, tloc_before)?;
        self.record_event(
            guest,
            Ok(SyscallEvent::TimeV2(ClockEvent { result, output })),
        );
        result.map_err(Error::from)
    }

    pub(super) async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Gettimeofday,
    ) -> Result<i64, Error> {
        let tv = Destination::new("gettimeofday", "tv", syscall.tv().map(|pointer| pointer.0));
        let tz = Destination::new("gettimeofday", "tz", syscall.tz());
        let tv_before = tv.pre_call(&guest.memory())?;
        let tz_before = tz.pre_call(&guest.memory())?;
        let result = guest.inject(syscall).await;
        let timeval = tv.capture(&guest.memory(), result, tv_before)?;
        let timezone = tz.capture(&guest.memory(), result, tz_before)?;
        self.record_event(
            guest,
            Ok(SyscallEvent::GettimeofdayV2(GettimeofdayEvent {
                result,
                timeval,
                timezone,
            })),
        );
        result.map_err(Error::from)
    }
}
