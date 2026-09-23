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

use super::Replayer;
use crate::clock_output;

impl Replayer {
    pub(super) async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGettime,
    ) -> Result<i64, Error> {
        // Consume the event even when the destination is NULL. The recorded
        // errno and any partial copyout belong to this call, not the next one.
        let event = next_event!(guest, ClockGettimeV2)?;
        clock_output::replay(
            &mut guest.memory(),
            syscall.tp().map(|pointer| pointer.0),
            event.result,
            event.output,
        )?;
        event.result.map_err(Error::from)
    }

    pub(super) async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Time,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, TimeV2)?;
        clock_output::replay(
            &mut guest.memory(),
            syscall.tloc(),
            event.result,
            event.output,
        )?;
        event.result.map_err(Error::from)
    }

    pub(super) async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Gettimeofday,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, GettimeofdayV2)?;
        // Linux copies timeval before timezone. The snapshots describe final
        // bytes, so the same order also preserves overlapping destinations.
        clock_output::replay(
            &mut guest.memory(),
            syscall.tv().map(|pointer| pointer.0),
            event.result,
            event.timeval,
        )?;
        clock_output::replay(
            &mut guest.memory(),
            syscall.tz(),
            event.result,
            event.timezone,
        )?;
        event.result.map_err(Error::from)
    }
}
