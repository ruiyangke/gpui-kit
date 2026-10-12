# Local gpui-base patch

`Switch` now amends its existing AccessKit node with the disabled flag and
removes activation/focus actions while disabled. An element adapter forwards
layout, painting, identity and synthetic accessibility children unchanged; it
does not create another node or focus stop. Its accessibility regression now
checks the flag, absent disabled actions and preserved supplementary description.

Source: the published crates.io `gpui-base` 0.7.1 package from
https://github.com/longbridge/gpui-kit. The original commit is recorded in
`.cargo_vcs_info.json`; `LICENSE-APACHE` retains the upstream Apache-2.0 license.
Registry bookkeeping and the package's standalone lockfile are omitted.

`src/root.rs` replaces the 100-attempt limit in modal Tab/Shift-Tab traversal
with termination when the finite rendered tab order repeats a focus handle.
An empty trap restores focus to its container rather than leaving it on a
background control. Ordinary navigation outside an active trap is unchanged.

GPUI 0.3.8 does not expose its rendered `TabStopMap` or an API to enumerate
candidate handles without navigating. This patch therefore uses synchronous
`Window::focus_next` / `focus_prev`. Those functions only update the pending
focus and request a refresh; GPUI publishes focus changes after the next draw
using the final focus path. The scan never yields or paints between candidates.
Regression tests include 256 background controls and check forward/backward
wrapping, empty-trap fallback, ordinary background navigation, and absence of
background focus notifications or activations during trapped traversal.

`src/focus_trap.rs` selects the innermost rendered container containing focus.
Previously, the first matching entry in a `HashMap` could select an outer trap
and let Tab escape an inner scope. The lookup now compares containment in the
rendered focus tree, independently of registration or hash iteration order.
The rendered regression exercises Tab and Shift-Tab while closing and reopening
six nested scopes, retaining their handles throughout. This covers traversal
scope selection; dialog lifecycle focus restoration is owned by the component
layer and is not changed by this patch.

The package is a member of Newport's experiment workspace so tests share its
lockfile. The ignored package-level resolver setting was removed. Benchmark
sources remain intact: `text_view_scroll` requires the explicit `bench-support`
feature, keeping benchmarking instrumentation out of ordinary unit-test builds.
Unused `criterion` and `reqwest_client` dev dependencies were removed, and the
native shaping test's `gpui_platform` dependency is scoped to Linux, where that
test is compiled.

```sh
cargo test -p gpui-base --lib root::tests
cargo bench -p gpui-base --bench motion
cargo bench -p gpui-base --bench text_view_scroll --features bench-support
cargo check --locked
```

Remove this focus-trap patch when an upstream release provides equivalent
containment without a fixed global-tab-stop limit and deterministic innermost
scope selection.

`Dialog::aria_label` and `AlertDialog::aria_label` expose a name on the existing
semantic modal host without adding another focus scope or accessibility node.
This lets application-supplied visual headers name their dialog. The Component
adapter forwards this property; its rendered regression checks both Dialog and
AlertDialog roles with their labels. Remove when upstream exposes an equivalent
named modal host API.

`src/select.rs` binds Space to the existing Confirm action outside text inputs.
This opens a focused trigger and confirms a highlighted option, matching the
reference Radix Select behavior. The `Select && !Input` context excludes a
search input anywhere on the focus path, so spaces remain editable text there.
Rendered regressions verify trigger opening, one confirmation, disabled
inactivity, and typing a multiword search without closing or confirming.
Run `cargo test -p gpui-base --lib select::tests`. Remove when upstream provides
equivalent Space activation without intercepting search text.

Select also consumes a handled Confirm action. Previously its unconditional
propagation both opened the dropdown and submitted an enclosing dialog, for
Enter as well as Space. Component-level rendered tests place a real Select
inside a real Dialog, open and choose an option using each key, and assert the
dialog's `on_ok` is never invoked. Run
`cargo test -p gpui-component --lib select::tests`. Remove this part when upstream
contains Select activation within the control.

`FocusTrapContainer` forwards `a11y_role`, `write_a11y_info`, and
`a11y_synthetic_children` to its wrapped element. Previously the wrapper dropped
the dialog/sheet semantics before GPUI built the native accessibility tree,
although test-support snapshots of the inner div still showed those semantics.
The regression checks the outer Element hooks consumed by GPUI for Dialog and
AlertDialog roles and their names/descriptions. It does not claim a platform
adapter test: GPUI's test windows do not activate that adapter. Remove when
upstream's focus-trap wrapper preserves the wrapped accessibility hooks.

`SelectableText::paint_selection` is exposed as a documented renderer utility so
virtualized plain-text adapters can reuse its wrapped-line selection quads while
retaining UTF-8 document endpoints through the existing `TextSelectionHandle`.
Window selection copy now preserves selected whitespace: `Root` no longer trims
text and participant aggregation excludes only empty strings. Rendered Newport
log regressions cover literal indentation, spaces-only selection, Unicode,
wrapped and split-column text, offscreen copying and dataset replacement.
Run `cargo test --bin newport-gpui selectable_log::tests` from `crates/newport-gpui`.
Remove these changes when upstream offers equivalent rendering reuse and exact
selection copying.
