# Bridge guidance and status panel behavior

Reference for the first-run walkthrough, the inline glossary, and the
always-on-top status panel. Written for whoever changes this behavior next.

## Division of responsibilities

The walkthrough exists because this is the one thing a new user cannot infer
from the UI:

- **Logic 2 owns** enabled channels, sample rate, trigger, filters, analyzers,
  and capture start/stop.
- **Bridge owns** which PXLogic is used, the MCU firmware image, and the
  hardware comparator threshold.

Logic 2 shows `Demo Logic Pro 16` as the session device because Bridge presents
that identity; the samples come from PXLogic.

## First-run walkthrough

Four steps, shown once. `guidance.onboardingCompletedVersion` records
completion; `0` means never completed. Skipping counts as answered — re-opening
uninvited is worse than trusting the user, and the header button `使用引导`
brings it back at any time.

Raising `ONBOARDING_VERSION` in `main.rs` replays the walkthrough once for
existing users after a major UI change. That is the only reason the field is an
integer rather than a boolean.

No step duplicates a control. Each one reads the live value from the settings
sections and offers a button that jumps to the real control. A wizard with its
own copies of the device picker or the threshold field would eventually
disagree with the settings it describes.

Hosts that do not persist `guidance` — the legacy Electron launcher — never see
the walkthrough and have the header button hidden.

## Inline glossary

`?` affordances resolve against `PXLOGIC_HINTS` in
`client/renderer/hints.js`, shared by both windows. A delivery test asserts the
key set and the set of `data-hint` attributes across `index.html` and
`status-panel.html` are **equal in both directions**, so a reworded hint cannot
orphan an affordance and a new affordance cannot render an empty bubble.

The bubble is `role="tooltip"` and never receives focus, because its content is
not interactive. `Escape` dismisses it and returns focus to the trigger.

## Status panel

Separate always-on-top window, label `status`, hidden at startup.

**Reveals itself** when the Bridge reaches `phase == "running"`, which is the
moment focus is handed to Logic 2 and the main window disappears behind it. The
automatic path uses `show_status_panel_without_activating`, which deliberately
omits the `set_activation_policy(Regular)` call the manual path makes —
activating there would snatch focus straight back from Logic 2. A delivery test
asserts that function body stays free of the call.

The first automatic reveal shows a one-time banner explaining what the panel is,
with an opt-out. `guidance.statusPanelIntroSeen` and
`statusPanel.autoShow` record those choices.

**Position** is remembered in `statusPanel.position` as physical pixels in
virtual-desktop space. With no remembered value the panel opens at the primary
work area's top-right corner rather than wherever the OS would put it, which is
often underneath the Logic 2 window.

`clamp_panel_position` keeps a remembered position reachable: a position saved
while an external display was attached, or one dragged almost entirely
off-screen, resolves back onto a live work area. Positions that already show at
least 32 px in both axes are returned untouched so the panel never drifts on its
own. `panel_work_areas` hoists the primary monitor to the front of the list
because `available_monitors()` makes no ordering promise and the first entry is
the preferred fallback.

**Edge snapping** happens only after a drag settles (`Moved` debounced by 150 ms),
never during the gesture, so it cannot fight the pointer. Repositioning emits
another `Moved`; the next settle computes the same coordinates and stops, so it
converges after one idle pass.

**Expanded shape** is 340×340 and draws its own chrome. The window is
undecorated in both shapes: a native titlebar would sit above the panel's own
collapse and hide controls and duplicate them, and it would dwarf the chip. The
header is therefore both the live readout and the drag handle.

The layout answers questions rather than spending lines on chrome:

- The header's first line is the connection state, its second the device
  identity. "Am I connected" and "to what" are one question, so a static
  `PXLOGIC BRIDGE / 采集状态` title and a separate device section were both
  removed. Model and serial outgrow 340 px, so hovering reveals them.
- Six metrics sit in a two-column grid.
- The data link is one card of two tight lines: label and verdict, then the bar
  and the counts.

**Collapsed shape** is a single floating button, 168×44, carrying the state dot
and one string: injected bytes while capturing, the phase message otherwise. The
whole window is the button, because the previous design — a small icon inside a
bar — was too fiddly to hit and defeated the point of collapsing.

Two platform details make the interaction work:

- `acceptFirstMouse` is set on the window. The panel is deliberately shown
  without focus while Logic 2 owns it, and macOS otherwise swallows the first
  click on an inactive window just to activate it — the chip would need two
  clicks.
- Cocoa anchors a resize at the bottom-left, so `apply_status_panel_collapsed`
  restores the pre-resize origin. Without it the visible top edge slides down the
  screen every time the shape changes and the user loses track of the chip.

Expanding hangs off the button's own `click` event so keyboard activation and
assistive technology work. `bindDragHandle` serves both shapes: a press becomes a
drag only once the pointer has travelled 6 px **with the button still held**,
because the system delivers stray moves around window activation, and the click
that ends a drag is suppressed. Nested buttons never start a drag; the chip
itself does, because it is the handle.

Dropping the decorations has one known cost: the panel no longer appears in the
macOS accessibility window list. Its contents remain reachable once focused, and
the main window carries the same information.

The window config's minimums are the chip dimensions, so the expanded minimum is
re-applied in code on every expand — a Rust test asserts the config floor still
permits the chip and that `acceptFirstMouse` and undecorated stay set.

## Settings ownership

`guidance` and `statusPanel` are **backend-owned**. The main window does not
render them and its save payload omits them, so every entry point that accepts a
`ClientSettings` from the UI must persist it through `store_renderer_settings`,
which merges those sections back from disk. `client_save_settings` and
`start_bridge_inner` both do. Writing renderer settings with `store_settings`
directly resets the panel position and the walkthrough flags to their defaults —
that bug shipped once through `start_bridge_inner`, so a delivery test now
enumerates the settings entry points and fails when a new one appears.

Adding another backend-owned field means adding it to
`merge_backend_owned_settings`.
