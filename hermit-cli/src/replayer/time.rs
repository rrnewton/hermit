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
use crate::clock_output::Destination;

impl Replayer {
    pub(super) async fn handle_clock_gettime<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: ClockGettime,
    ) -> Result<i64, Error> {
        // Consume the event even when the destination is NULL. The recorded
        // errno and any partial copyout belong to this call, not the next one.
        let event = next_event!(guest, ClockGettimeV2)?;
        let tp = Destination::new("clock_gettime", "tp", syscall.tp().map(|pointer| pointer.0));
        clock_output::replay(&mut guest.memory(), event.result, &[(tp, &event.output)])?;
        event.result.map_err(Error::from)
    }

    pub(super) async fn handle_time<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Time,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, TimeV2)?;
        let tloc = Destination::new("time", "tloc", syscall.tloc());
        clock_output::replay(&mut guest.memory(), event.result, &[(tloc, &event.output)])?;
        event.result.map_err(Error::from)
    }

    pub(super) async fn handle_gettimeofday<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Gettimeofday,
    ) -> Result<i64, Error> {
        let event = next_event!(guest, GettimeofdayV2)?;
        // Linux stores tv_sec, then tv_usec, then the timezone. Replay both
        // destinations together, in that order: an aliased timezone must be
        // validated against its pre-call bytes before the timeval is written.
        let tv = Destination::new("gettimeofday", "tv", syscall.tv().map(|pointer| pointer.0));
        let tz = Destination::new("gettimeofday", "tz", syscall.tz());
        clock_output::replay(
            &mut guest.memory(),
            event.result,
            &[(tv, &event.timeval), (tz, &event.timezone)],
        )?;
        event.result.map_err(Error::from)
    }
}
