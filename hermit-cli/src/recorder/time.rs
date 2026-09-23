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
use crate::clock_output;
use crate::event::ClockEvent;
use crate::event::GettimeofdayEvent;
use crate::event::SyscallEvent;

impl Recorder {
    pub(super) async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGettime,
    ) -> Result<i64, Error> {
        let result = guest.inject(syscall).await;
        let output = clock_output::capture(
            &guest.memory(),
            syscall.tp().map(|pointer| pointer.0),
            result,
        )?;
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
        let result = guest.inject(syscall).await;
        let output = clock_output::capture(&guest.memory(), syscall.tloc(), result)?;
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
        let result = guest.inject(syscall).await;
        let timeval = clock_output::capture(
            &guest.memory(),
            syscall.tv().map(|pointer| pointer.0),
            result,
        )?;
        let timezone = clock_output::capture(&guest.memory(), syscall.tz(), result)?;
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
