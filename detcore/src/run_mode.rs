/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The run mode: Detcore's type parameter, composing the choices a run makes
//! once, when it is built.
//!
//! [`RunMode`] is type composition only: the record-or-replay sub-tool
//! ([`RunMode::RecordReplay`], with its existing bound), the scheduling-choice
//! policy ([`RunMode::Choices`]) and the choice boundary
//! ([`RunMode::Boundary`]). It carries no runtime value, flag or backend
//! selection. [`Mode`] composes a sub-tool and a policy, with no boundary;
//! [`SeededMode`] is the ordinary run and [`ControlledMode`] a run whose
//! scheduling choices a choice source answers. A mode with a boundary of its
//! own is its own type.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use crate::choice::ChoiceSite;
use crate::choice::ControlledChoices;
use crate::choice::SchedulingChoices;
use crate::choice::SeededChoices;
use crate::record_or_replay::NoopTool;
use crate::record_or_replay::RecordOrReplay;

/// A run's mode: see the module documentation.
pub trait RunMode: Send + Sync {
    /// The record-or-replay sub-tool that Detcore forwards to.
    type RecordReplay: RecordOrReplay;
    /// How the run resolves its scheduling choices.
    type Choices: SchedulingChoices;
    /// What runs just before each of the run's scheduling choice points is
    /// drawn.
    type Boundary: ChoiceBoundary<Self::Choices>;
}

/// One of a run's scheduling draws, handed whole to the run mode's choice
/// boundary, which makes it ([`ChoiceBoundary::choose`]): the operation that
/// draws a choice point, owning what it needs.
///
/// Whether the operation takes a point now is [`Self::draws`]: the condition
/// under which the run's policy makes a request
/// ([`SchedulingChoices::ANNOUNCES`]). The draw itself is [`Self::draw`],
/// made with the [`Announcement`] that only [`announce`] makes, so no
/// boundary can make a draw it has not announced.
pub trait ChoiceDraw<M: RunMode, G: reverie::Guest<crate::Detcore<M>>>: Send {
    /// What the operation yields. `Default` so that a boundary that waits for
    /// nothing hands it back in a future that holds only it, and is
    /// zero-sized where it is.
    type Output: Default + Send;

    /// Whether the drawing thread `guest` takes a choice point in this draw.
    fn draws(&self, guest: &G) -> bool;

    /// Make the draw on the drawing thread `guest`, immediately after its
    /// `announcement`.
    fn draw(self, guest: &mut G, announcement: Announcement<M::Choices>) -> Self::Output;
}

/// A draw's announcement to the run's choice policy, made by [`announce`]
/// immediately before the draw, and consumed by it ([`ChoiceDraw::draw`]).
///
/// Only [`announce`] makes one:
/// ```compile_fail,E0451
/// let _ = detcore::run_mode::Announcement::<detcore::choice::SeededChoices> { announced: None };
/// ```
#[must_use = "a draw is made with its announcement"]
pub struct Announcement<C: SchedulingChoices> {
    announced: Option<C::Announced>,
}

impl<C: SchedulingChoices> Announcement<C> {
    /// What the policy announced of the point the draw takes (under
    /// [`ControlledChoices`], the index of the request it takes). `None`
    /// where the draw takes no point, or where the policy announces nothing
    /// ([`SchedulingChoices::ANNOUNCES`]).
    pub fn announced(&self) -> Option<C::Announced> {
        self.announced
    }
}

/// Announce `draw`, the drawing thread `guest`'s next draw, at `site`, to the
/// run's policy ([`SchedulingChoices::announce`]), when the policy announces
/// its draws and this one takes a point ([`ChoiceDraw::draws`]). Every
/// boundary calls this immediately before it makes the draw, with nothing
/// else in between, and makes the draw with what it returns. A policy that
/// announces nothing (the ordinary run's) is never asked, and the draw's
/// condition is not evaluated here.
pub fn announce<M, G, D>(guest: &G, site: ChoiceSite, draw: &D) -> Announcement<M::Choices>
where
    M: RunMode,
    G: reverie::Guest<crate::Detcore<M>>,
    D: ChoiceDraw<M, G>,
{
    let announced =
        (<M::Choices as SchedulingChoices>::ANNOUNCES && draw.draws(guest)).then(|| {
            let thread = guest.thread_state();
            thread.choices.announce(thread.draw_context(), site)
        });
    Announcement { announced }
}

/// A run mode's choice boundary, for runs whose choice policy is `C`: what
/// runs on the drawing thread's own task immediately before one of its
/// scheduling choice points is drawn, with that thread's guest.
///
/// Detcore hands every draw to the boundary as an owned operation
/// ([`ChoiceDraw`]) and awaits the future [`Self::choose`] returns, at every
/// draw site and nowhere else, uniformly: it never asks which boundary a run
/// has. A run mode that needs one names its own type, implemented for the
/// policy whose announcement it reads, so the capability is opt-in by type.
/// The ordinary modes' boundary is [`NoBoundary`], which makes the draw at
/// once.
///
/// The boundary announces the draw ([`announce`]), does what it does before
/// the draw, and makes it, with no other await in between, so nothing else
/// runs between the announcement and the draw. Under [`ControlledChoices`]
/// the draw checks that it takes the request announced
/// (`ChoiceSession::choose`), and a later announcement checks that the one
/// before it was drawn; a run that finishes with an announcement undrawn is
/// caught when its session finishes (`ChoiceSession::finish`).
///
/// The sites ([`ChoiceSite`]):
/// - `ChaosSlice`: a thread's slice, at its start and at its slice ends.
/// - `Priority`: a thread's priority redraw, at a slice end or a
///   `sched_yield`.
/// - `ChildPriority`: a new thread's starting priority, drawn by its parent
///   after the kernel clone made the thread (or by the run's first thread,
///   for itself, as it registers). A boundary that copies the drawing thread
///   would copy a parent without that thread, so it refuses a capture there
///   by name.
/// - `VforkChildPriority`: a vfork child's starting priority, drawn by its
///   parent before the clone.
pub trait ChoiceBoundary<C: SchedulingChoices>: Send + Sync + 'static {
    /// Make `draw`, the drawing thread `guest`'s next scheduling draw, a
    /// choice point at `site` when it takes one ([`ChoiceDraw::draws`]), with
    /// this boundary before it. A boundary that cannot do what a point needs
    /// ends the run by name rather than let the draw happen without it.
    ///
    /// Only a boundary that waits for something keeps the draw in its
    /// future: one that waits for nothing makes the draw before it returns.
    fn choose<M, G, D>(
        guest: &mut G,
        site: ChoiceSite,
        draw: D,
    ) -> impl Future<Output = D::Output> + Send
    where
        M: RunMode<Choices = C, Boundary = Self>,
        G: reverie::Guest<crate::Detcore<M>>,
        D: ChoiceDraw<M, G>;
}

/// The ordinary run's choice boundary: nothing happens at a choice point.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoBoundary;

impl<C: SchedulingChoices> ChoiceBoundary<C> for NoBoundary {
    /// Announces the draw, as every boundary does (nothing, for seeded
    /// choices), and makes it before returning: the future it returns is
    /// ready at once and holds only the draw's result.
    #[inline(always)]
    fn choose<M, G, D>(
        guest: &mut G,
        site: ChoiceSite,
        draw: D,
    ) -> impl Future<Output = D::Output> + Send
    where
        M: RunMode<Choices = C, Boundary = Self>,
        G: reverie::Guest<crate::Detcore<M>>,
        D: ChoiceDraw<M, G>,
    {
        let announcement = announce::<M, G, D>(guest, site, &draw);
        Drawn(draw.draw(guest, announcement))
    }
}

/// The future of a draw made before its boundary returned
/// ([`NoBoundary::choose`]): ready at its first poll with the draw's result,
/// the only thing it holds. A poll after that yields `T::default()`.
struct Drawn<T>(T);

// Never pinned structurally: the result is moved out, not pinned in place.
impl<T> Unpin for Drawn<T> {}

impl<T: Default> Future for Drawn<T> {
    type Output = T;

    #[inline(always)]
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        Poll::Ready(std::mem::take(&mut self.get_mut().0))
    }
}

// A draw with no result (a thread's startup reset) leaves nothing in the
// future of the ordinary run's boundary.
const _: () = assert!(std::mem::size_of::<Drawn<()>>() == 0);

/// The run mode of sub-tool `R` and choice policy `C`. A type only; it is
/// never constructed. Its choice boundary is [`NoBoundary`].
pub struct Mode<R, C>(PhantomData<fn() -> (R, C)>);

impl<R: RecordOrReplay, C: SchedulingChoices> RunMode for Mode<R, C> {
    type RecordReplay = R;
    type Choices = C;
    type Boundary = NoBoundary;
}

/// The ordinary run of sub-tool `R`: Detcore's seeded draws decide.
pub type SeededMode<R = NoopTool> = Mode<R, SeededChoices>;

/// A run of sub-tool `R` whose scheduling choices a choice source answers.
pub type ControlledMode<R = NoopTool> = Mode<R, ControlledChoices>;

/// The thread state of the record-or-replay sub-tool of mode `M`.
pub type RecordReplayState<M> = <<M as RunMode>::RecordReplay as reverie::Tool>::ThreadState;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::SeqCst;

    use reverie::Guest;

    use super::*;
    use crate::choice::ChoiceHandle;
    use crate::choice::ChoiceKind;
    use crate::choice::ChoicePoint;
    use crate::choice::ChoiceSession;
    use crate::choice::TapeSource;
    use crate::syscall_hot_path_tests::InjectGuest;

    /// A draw that records that it was made and what was announced before
    /// it. Its condition is `condition`; `None` means it must not be read.
    struct Probe<'a> {
        condition: Option<bool>,
        made: &'a AtomicBool,
        announced: &'a Mutex<Option<String>>,
    }

    impl<M: RunMode, G: reverie::Guest<crate::Detcore<M>>> ChoiceDraw<M, G> for Probe<'_> {
        type Output = ();

        fn draws(&self, _guest: &G) -> bool {
            self.condition
                .expect("a run whose policy announces nothing read a draw's condition")
        }

        fn draw(self, _guest: &mut G, announcement: Announcement<M::Choices>) {
            *self.announced.lock().unwrap() = announcement.announced().map(|a| format!("{a:?}"));
            self.made.store(true, SeqCst);
        }
    }

    /// The ordinary run's boundary makes the draw before it returns, never
    /// reads the draw's condition, announces nothing, and returns a future
    /// that holds nothing and is ready at its first poll.
    #[test]
    fn the_ordinary_runs_boundary_draws_at_once_and_holds_nothing() {
        let mut guest = InjectGuest::<SeededMode>::new(Ok(0));
        let made = AtomicBool::new(false);
        let announced = Mutex::new(Some("unset".to_owned()));
        let probe = Probe {
            condition: None,
            made: &made,
            announced: &announced,
        };
        let future = <NoBoundary as ChoiceBoundary<SeededChoices>>::choose::<SeededMode, _, _>(
            &mut guest,
            ChoiceSite::ChaosSlice,
            probe,
        );
        assert!(
            made.load(SeqCst),
            "the draw is made before the future is polled"
        );
        assert_eq!(*announced.lock().unwrap(), None);
        assert_eq!(std::mem::size_of_val(&future), 0);
        let mut future = std::pin::pin!(future);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_ready());
    }

    /// Under controlled choices the boundary announces a draw that takes a
    /// point, then makes it with that announcement: the index of the request
    /// the draw takes. A draw that takes no point is made with nothing
    /// announced, and the session's next request does not move.
    #[test]
    fn the_controlled_runs_boundary_announces_a_draw_that_takes_a_point() {
        let mut guest = InjectGuest::<ControlledMode>::new(Ok(0));
        let handle = ChoiceHandle::new(ChoiceSession::new(Box::new(TapeSource::new(vec![])), None));
        let dettid = guest.thread_state().dettid;
        guest
            .thread_state_mut()
            .choices
            .attach(Some(handle.clone()), dettid);

        let made = AtomicBool::new(false);
        let announced = Mutex::new(None);
        let probe = Probe {
            condition: Some(false),
            made: &made,
            announced: &announced,
        };
        let future = <NoBoundary as ChoiceBoundary<ControlledChoices>>::choose::<
            ControlledMode,
            _,
            _,
        >(&mut guest, ChoiceSite::Priority, probe);
        drop(future);
        assert!(made.load(SeqCst));
        assert_eq!(*announced.lock().unwrap(), None);
        assert_eq!(handle.next_index(), 0, "nothing was announced");

        made.store(false, SeqCst);
        let probe = Probe {
            condition: Some(true),
            made: &made,
            announced: &announced,
        };
        let future = <NoBoundary as ChoiceBoundary<ControlledChoices>>::choose::<
            ControlledMode,
            _,
            _,
        >(&mut guest, ChoiceSite::Priority, probe);
        drop(future);
        assert!(made.load(SeqCst));
        assert_eq!(announced.lock().unwrap().as_deref(), Some("0"));
        // The draw the session was told of takes the request announced.
        let point = ChoicePoint {
            kind: ChoiceKind::SchedPriority,
            lo: 1,
            hi: 9,
            default: 5,
            scale: 0,
            dtid: dettid,
            thread_rcbs: guest.thread_state().draw_context().thread_rcbs,
            site: ChoiceSite::Priority,
        };
        assert_eq!(handle.choose(point), 5);
        assert_eq!(handle.next_index(), 1);
    }
}
