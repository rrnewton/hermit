---
name: ux-tester
description: "Test a frontend as a real user with browser-driven workflows, desktop and mobile visual inspection, accessibility checks, and reproducible defect evidence. Use whenever reviewing, validating, or finishing a web UI, app, dashboard, game, or interactive tool."
---

# UX Tester

Test the running product, not only its source. Exercise the workflows a target
user came to complete, inspect what the browser actually rendered, and bind
every conclusion to reproducible evidence.

## Establish The Test Contract

- Identify the audience, primary workflows, supported browsers, and expected
  responsive range from the task and product context.
- Record the exact build SHA, URL, fixture or account, browser, viewport, and
  relevant feature flags. Use deterministic local fixtures where possible.
- Start the repository's normal development or preview server. Confirm the page
  is the intended product screen rather than a landing page, error page, stale
  build, or blank canvas.
- Prefer the project's existing browser framework. Use Playwright when one is
  available; do not replace an established test harness with ad hoc automation.

## Exercise Real Workflows

For each primary workflow:

1. begin from a clean, documented state;
2. perform the actions through visible controls as a user would;
3. verify intermediate, success, empty, loading, validation, and failure states;
4. reload or revisit when persistence and navigation are part of the contract;
5. check browser console errors and failed network requests;
6. repeat the critical path using keyboard navigation.

Interact with every control introduced or changed by the work. Verify that
buttons, menus, tabs, dialogs, forms, drag interactions, and back navigation do
what their labels and placement imply. Do not count DOM presence as usability.

## Inspect Every Viewport

Test at least one desktop viewport and one narrow mobile viewport. Use stable
dimensions such as `1440x900` and `390x844`, plus any product-specific boundary
where layout changes.

- Capture a screenshot after initial load and after important state changes.
- Inspect screenshots directly, including below-the-fold content. For canvas or
  WebGL experiences, also check canvas pixels so a nonempty DOM cannot mask a
  blank render.
- Look for overlap, clipping, horizontal scrolling, unreadable text, unstable
  control sizes, hidden actions, broken stacking, and content obscured by fixed
  headers, keyboards, dialogs, or toasts.
- Exercise long labels, long values, empty collections, errors, and the largest
  realistic repeated-data set. Dynamic content must not shift fixed controls or
  escape its container.
- Confirm the first viewport communicates the actual product, object, place, or
  gameplay state and leaves navigation and primary actions usable.

## Check Accessibility And Interaction Quality

- Tab through the interface in visual order. Focus must remain visible and must
  enter, stay within, and leave dialogs predictably.
- Verify interactive elements have accessible names, correct roles, useful
  labels, and adequate hit targets. Familiar icon-only controls need tooltips or
  equivalent accessible names.
- Check heading order, form labels, validation association, selected/expanded
  state, reduced-motion behavior, and contrast for text and controls.
- Confirm hover-only information has a keyboard and touch path. Ensure loading
  and disabled states explain themselves without blocking unrelated work.

## Report Evidence, Not Impressions

Report each defect with:

- severity and affected workflow;
- exact reproduction steps;
- expected and observed behavior;
- build SHA, browser, viewport, and fixture;
- screenshot, trace, console message, or failed request when relevant.

Separate observed facts from hypotheses about the cause. When reporting counts,
rates, or timings, also apply
[presenting-quantitative-data](presenting-quantitative-data.md). Scrub internal
FQDNs, credentials, tokens, and personal data from durable artifacts.

## Completion Gate

- Re-run every affected workflow after a fix, then run the surrounding smoke
  paths to catch regressions.
- Confirm desktop and mobile screenshots are coherent and no console or network
  errors were introduced.
- State exactly what passed, what failed, and what could not be exercised. Never
  call a UI polished or complete based only on code review or a single happy-path
  screenshot.
