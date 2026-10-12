# Local gpui-component patch

Disabled switches blend a solid thumb against the theme surface at 50% opacity
before painting it opaque. Fading only the track was correct for the default
surface-colored thumb, but left Newport's custom white thumb fully bright in
dark mode. Fading both primitives independently would instead leak the track
color through the thumb. Gradient thumbs retain the previous behavior; this
correction targets solid theme surfaces used by Newport. Thumb disabled state
is explicitly projected through the Base SwitchThumb styles.

`Switch::accessibility_description` forwards supplementary help to the base
switch's native description property, preserving its concise accessible name.

`PopupMenu::confirm` dismisses the menu before activating its item. Activation
runs after the dismissal subscribers in the same effect flush, so a dialog
opened by a handler or action records a stable opener rather than the released
menu. Actions retain their original dispatch path while the focus is restored;
context-menu actions still reach the trigger's ancestors, and explicit action
contexts keep their existing routing.

Dropdowns now share a stable focus handle with their actual trigger, rather than
registering a second handle on an ancestor solely for shortcut hints. Dismissal
returns to this trigger even if another control had focus before a pointer
opened the menu. The generic trigger hook defaults to `InteractiveElement`
tracking; the Kit Button override forwards to a new inherent `track_focus` that
honors a supplied handle, matching Base Button. Buttons without a supplied handle
retain keyed focus state, and disabled/loading behavior and AX state are unchanged.

Rendered regressions exercise keyboard handler/action activation, a pointer-opened
custom item while another control has focus, and a submenu. Each opens a real
dialog, checks its initial input focus and menu release, presses Escape, verifies
the originating trigger's focus, and reopens the menu with Enter. Button tests
cover supplied/default handles, Enter/Space activation, and disabled transitions.

```sh
cargo test -p gpui-component --lib menu::
cargo test -p gpui-component --lib button::button::tests
```

`src/menu/context_menu.rs` transfers focus in the deferred opening callback,
before frame construction, rather than during DeferredMenu prepaint. The old
timing crashes GPUI's accessibility builder when a focused splitter is painted
before the menu: two nodes claim focus in one frame. Four context-menu tests
pass, including a pre-draw focus assertion. Live reproduction now opens the menu
without crashing and Escape restores the previous splitter's keyboard control.

`src/combobox_semantics.rs` forwards the rendered BaseSelect element lifecycle
and augments its existing accessibility node with the disabled flag. The root
identity, layout, focus actions and synthetic children are retained. A rendered
semantic regression test checks name/value/role/ID and disabled on/off. All 23
combobox tests pass. Live AX verifies disabled during editing, recovery after
Cancel, opening and Escape dismissal. Independent source review found no issue.

`src/button/button.rs` exposes the computed disabled state on the existing
accessibility node through a forwarding Element adapter.
The base button already suppresses input, but 0.7.1 does not set the AccessKit
disabled flag. All 18 button tests pass; live AX confirms disabled shell actions
while editing, re-enabled actions after Cancel, and disabled Launch at login.

Dropdown menu triggers expose Click, their actual expanded state, and
`HasPopup::Menu` on that same Button node. Previously the popover's wrapper
opened on mouse-down and Confirm, but a handler-less Button advertised no
accessible Click action. The menu adapter prepares its trigger before type
erasure through the component Popover's trigger-builder seam. A no-op listener
is appended without replacing facade or base handlers, so GPUI's accessible
Click fallback reaches the existing mouse-down opener and cannot toggle twice
on mouse-up. Disabled/loading menu triggers expose no Click action. A local
Confirm guard also prevents a loading trigger from opening by keyboard before
its normal key-up click guard runs; pointer blocking stays unchanged. The forwarding
Element retains the original ID, role, layout, synthetic children, and focus
hooks without adding another accessibility node or focus stop.

Regression coverage reads the actual rendered Button Element's accessibility
hooks for enabled/disabled/loading and expanded states. A rendered menu test
then exercises GPUI's synthetic left-down/up Click fallback, one opening per
activation, Escape, Enter/Space reopening, and inactive disabled/loading
triggers. GPUI's OS action dispatcher is crate-private, so this is not an
AXPress platform-adapter test. Run
`cargo test -p gpui-component --features test-support --lib menu_trigger` and
`cargo test -p gpui-component --features test-support --lib menu::`.
Remove when upstream exposes equivalent menu-trigger semantics and activation.

Source: the published crates.io `gpui-component` 0.7.1 package, from
https://github.com/longbridge/gpui-kit. The upstream commit is recorded in
`.cargo_vcs_info.json`. The original Apache-2.0 license is in `LICENSE-APACHE`.
Registry bookkeeping and the dependency crate's standalone lockfile are omitted.

`src/combobox.rs` compares selected item values before and after
`on_will_change`, in both the deferred list-confirm callback and direct item
selection handler. Filtered row indices are positions, not item identities:
replacing A at row 0 with filtered B at row 0 must emit Change and Confirm and
close a single-select combobox. Value comparison also avoids treating a change
of row position alone as a value change.

The published crate's unit tests reference five fixtures outside the package.
Copies now live in `tests/fixtures` and the test-only `include_*` paths point
there: SVG icons and their Lucide license come from `gpui-kit-assets` 0.7.1,
the dock module comes from `gpui-base` 0.7.1, and the Markdown example and Aurora
theme come from the upstream commit above (`examples/fixtures/test.md`
and `themes/aurora.json`). The ignored package-level resolver setting was
removed because this crate is now a member of the app's workspace.

Workspace membership lets the regression tests share the app's lockfile; the
app remains the default member. Cargo also records the vendor's optional and
dev dependencies in that lockfile. In particular, optional `tree-sitter-sequel`
0.3.11 requires `cc ~1.2.1`, resolving `cc` to 1.2.67 instead of 1.6.0. This
changes a build dependency; no additional app runtime feature is enabled.

The regression tests exercise both selection paths and verify event payloads
and popover closure. Run from `crates/newport-gpui`:

```sh
cargo test -p gpui-component --lib combobox::tests
cargo check --locked
```

`ComboboxState::close` also exposes focus-neutral dismissal for application-owned
transitions. Newport calls it before opening the server editor, releasing the
deferred popup context without relying on a later blur event or committing a
selection. Its regression test checks idempotency, preserved selection, no emitted
selection events and released popup context.

The combobox builder also accepts an accessible name and projects selected item
titles as the accessible value. It uses BaseSelect with the Combobox key context,
exactly as upstream BaseCombobox delegates internally, because that wrapper does
not expose the label/value setters in 0.7.1. The existing keyboard/focus callbacks
remain unchanged. A regression checks that filtering does not change the value
until selection changes, and clearing removes the accessible value.

Remove these combobox patches when upstream provides equivalent behavior.

`Dialog::aria_label` is stored with dialog props so imperative and trigger-based
opening preserve it, then forwarded to the existing Base semantic modal host.
`AlertDialog` exposes the same option. Custom visual title elements previously
left the modal host unnamed. Rendered coverage verifies labels and Dialog /
AlertDialog roles with `--features test-support --lib dialog::dialog::tests`.
Remove when upstream offers equivalent named-modal semantics.

Single-select activation also closes an already-selected item without emitting
a false Change event. An Activate event identifies accepted item activation
separately from Confirm, which also fires on dismissal. Newport uses Activate
to leave Settings for the saved workspace destination; Escape/outside dismissal
must not navigate. A delegate veto still leaves the menu open. Regression tests
cover direct/deferred activation of the current item and outside dismissal.

`src/root.rs` gives each delayed dialog focus restoration its own identity.
The previous dialog-count check let an old timer focus a replaced dialog at the
same stack depth, and an old timer could also clear a newer pending restoration.
Only the exact pending request can now restore or clear focus. Opening a
replacement still inherits the original opener; explicitly closing its parent
or all dialogs retires stale pending returns. Targets stay weak so timers do
not keep removed controls alive. Four rendered regressions cover both races,
normal nested closure/replacement, and closing all during a pending return.
Run `cargo test -p gpui-component --lib root::tests`. Remove when upstream offers
equivalent restoration ownership and preserved opener chains.

`ListDelegate::item_accessibility_label` optionally supplies the name of the
existing ListItem host. SearchableListAdapter uses each item's textual title;
ordinary list delegates retain their current behavior by default. AccessKit
does not infer a ListItem name from visual descendants, which previously left
Select/Combobox options unnamed. The row host also participates in test-support
snapshots for geometry checks. Native-hook regression coverage verifies names
before and after the row indices change during filtering.

`Select::menu_item_min_height` sets an optional minimum on both default and
custom searchable-list rows, independently of trigger size. The default remains
unchanged. This lets Newport keep its compact 34px editor fields while matching
the original 28px option hit targets. The list measures the final row, retaining
correct virtual-list geometry. The application regression opens the actual
authentication and SSH-key dropdowns and checks all visible row bounds. Remove
when upstream offers independent menu-row sizing with equivalent measurement.

## Explicit control sizing

`Button::expanded` forwards disclosure state to Base's accessibility node without
changing selected/open presentation. `TabBar::accessibility_label` forwards a
collection name independently of individual tab labels. Services uses these for
the System disclosure and Details tab list. Regression tests verify forwarded
native accessibility metadata, including an absent disclosure state on ordinary
buttons. Remove these forwarding methods when upstream exposes equivalent APIs.

Newport's `src/controls.rs` defines logical-pixel sizes rather than relying on
Kit's rem-based named sizes. The following optional presentation seams keep
native controls aligned without replacing their behavior:

- `Button::content_style` is public, allowing label/icon sizing independently of
  the button hit target. Pagination forwards optional item and content styles
  to every page, previous/next, and ellipsis button.
- Switch accepts an explicit track size and label size; the thumb follows the
  track with a 2px inset. Existing named-size defaults are unchanged.
- Checkbox honors `Size::Size` for both indicator and check mark. Named sizes
  retain rem-based geometry and multiline label alignment.
- Tabs honor `Size::Size` for the outer/inner height and inset; TabBar can set a
  shared label size. The terminal's existing subtle 38px bar remains separate.

The native rendered control gallery checks 28px standard, 24px compact, 34px
form controls and 16px checkboxes at 14px and 16px rem scales, including pending
and disabled states. A Kit regression checks the 36×22 switch track in both
states. Existing component interaction tests continue to cover semantics.
