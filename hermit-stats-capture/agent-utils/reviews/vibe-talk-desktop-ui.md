# vibe-talk `/voice` — the desktop composition

**Reviewed 2026-08-19 · `#55 voice-desktop-app` · reviewer: opus 5 (agent)**

Two cold-context rounds, each one a real capture run followed by looking at the frames. This
document is the deliverable the issue asks for, and it exists because the page's whole test suite
— 120 behavioural tests — lays nothing out and therefore has no opinion at all about whether a
composition reads as an application. The only thing in this repository that can answer that is a
photograph and somebody looking at it.

Reproduce either round with:

```sh
cd vibe-talk
scripts/run.sh --screenshots --theme dark
```

Frames land in `vibe-talk/debug/screenshots/<timestamp>/`, which is gitignored, so the pictures
themselves are not in the repository; the run is free, offline, and needs no vendor conversation.
The states that matter here are `14-desktop-narrow-column` and `15-desktop-wide-column`, which
exist only on the `desktop` (1440x900) and `laptop-1280` (1280x800) profiles, plus every other
state at those two widths.

## What was there before

`debug/screenshots/20260819T182637Z/dark--desktop--10-discord-view.png`, the capture that opened
the issue, is the phone layout stretched to 1440 pixels: roughly 180-character lines, a "Start a
new call" button about 1350 pixels wide, and a dock scaled for a thumb nobody is using. Every
element was correct and the composition was not a desktop application.

## Round one — after the first implementation

Captured `20260820T032201Z`. What worked immediately:

- Both panes held to `var(--reading-width)` and centred. At the 72-character default the line
  length reads properly; at 48 and at 112 (states 14 and 15) the control visibly moves it, and the
  two ends differ by several hundred pixels of measured `getBoundingClientRect().width`.
- The control pane follows the column, so Hang up and Talk sit under the transcript rather than
  spanning the desk.
- The handle renders as a hairline on the column's right edge and moves with it.
- The phone frames are byte-for-byte the same composition they were: nothing in the new block
  matches below 900 pixels or on a coarse pointer.

Three defects, all of them the same defect wearing different clothes — **something that belongs to
the column was still anchored to the window**:

1. **`#connection-banner` and `#error` spanned the full 1440.** A full-width notice strip over a
   narrow centred column is exactly the "phone layout stretched sideways" the issue is about.
2. **`#scroll-tools` — the "Newest" and "Collapse all" chips — sat in the far bottom-right corner
   of the window**, half a screen away from the list they act on. Worse than ugly: the chip is an
   offer to scroll a list, and it was pointing at a corner the reader is not looking at.
3. **The status line did not line up with anything.** `#status-line` was capped in `ch` while
   carrying `font-size: 0.8rem`, and `ch` resolves against the element's OWN font size — so its
   column came out a fifth narrower than the control pane directly beneath it and the two edges
   visibly disagreed. `#connection-banner` had the same fault at `0.85rem`.

Finding 3 is the one worth recording for the next person, because it is not obvious from the CSS:
**a `ch` cap is only comparable between elements that share a font size.** The fix is to put the
small type on the text child rather than on the box, which is where it belonged anyway — the boxes
in question contain exactly one text node each, so nothing changes on a phone.

## Round two — after the fixes

Captured `20260820T032522Z`. All three are gone:

- The banner and the error panel are capped and centred on the same column as the transcript.
- The chips are pinned to the column's right edge (`left: 50%` plus a translate back by half the
  column, the chip's own width and a gutter), so they sit just inside the list they belong to.
  Confirmed in `dark--desktop--13-jump-to-newest.png`.
- The status line, the control pane and the panes now share one left edge, because the small type
  moved off the containers and onto `#status` and `#connection-detail`.

Both desktop widths were reviewed. 1280 is the more useful of the two: at 1440 a 72-character
column leaves a lot of margin, and it is at 1280 that the composition has to still look
deliberate rather than merely narrow. It does.

## What these rounds did NOT establish

- **Nothing here says the phone did not regress on a notched device.** Chromium reports zero
  safe-area insets under automation and renders square corners, so `env(safe-area-inset-*)` is
  unverifiable by any capture. It is asserted as a declaration by the page suite and nowhere else.
- **Light theme was not reviewed by eye in either round**; both runs were `--theme dark`, which is
  the theme the owner's devices are in. `--theme both` captures it.
- **The drag itself was not photographed.** Both desktop states set the width through the settings
  slider, because a synthetic drag is three events whose coordinates would have to be right for the
  picture to mean anything. The drag arithmetic is covered by the page suite instead, against
  stated geometry; that a real pointer feels right on a real trackpad is unverified.
- **A touch-screen laptop is untested.** It would report `pointer: coarse` and get the phone
  composition on a large screen. That is the conservative answer and it is deliberate — a
  thumb-sized dock on a big screen is merely odd, whereas a nine-pixel drag handle under a finger
  is unusable — but nobody has looked at it.
