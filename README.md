# Newport patches for GPUI Kit and GPUI

This branch holds the patches Newport applies to [GPUI Kit](https://github.com/longbridge/gpui-kit) and to the GPUI snapshot under it. It has no GPUI Kit or GPUI source.

Newport builds against branches generated from a base tag and a series. Never edit a generated branch by hand; change a patch and generate it again.

| Series | Base | Generated branch |
| --- | --- | --- |
| GPUI Kit: `series`, `patches/` | upstream release `v0.8.1` | `newport/v0.8.1` |
| GPUI: `gpui/series`, `gpui/patches/` | `gpui-pre-v0.3.10` | `newport/gpui-pre-v0.3.10` |

## Files

| File | Purpose |
| --- | --- |
| `series` | GPUI Kit patch order |
| `patches/` | One GPUI Kit patch per change, in `git format-patch` format |
| `gpui/series`, `gpui/patches/` | The same for GPUI |
| `deferred/` | Patches kept for later; not in `series` |
| `apply.sh` | Generate `newport/<tag>` from a tag and a series (`./apply.sh gpui <tag>` for GPUI) |
| `export.sh` | Write a series from a branch (`./export.sh gpui <tag>` for GPUI) |

## Patches

| Patch | What it does | Newport needs it for |
| --- | --- | --- |
| 0001 gpui | Pins `newport/gpui-pre-v0.3.10`; initializes the key event fields it adds | Terminal key encoding; Remote apps on Windows |
| 0002 dialog | `DialogHandle`, `close_dialog_if`, one owner per focus restoration | Closing its own dialog |
| 0003 menu | Menus dismiss before running an item; dropdowns return focus to their trigger; `DropdownMenuPopover::open`, `DropdownButton::menu_trigger` | Focus after menu actions that open dialogs |
| 0004 select | Select and Combobox confirm by value, not row; `ComboboxEvent::Activate`, `ComboboxState::close`, `Select::is_open`, `menu_scroll_handle`, `menu_item_min_height` | Paged and filtered pickers |
| 0005 controls | Explicit sizes: `Button::content_style`, Pagination styles, Switch track and label size, `Size::Size` for Checkbox and Tab, `TabBar::label_size`, input `content_padding` | Pixel-sized controls |
| 0006 text | Copies selections without trimming; keeps fractional line heights; public `SelectableText::paint_selection` | Logs, diffs and the code editor |
| 0007 search | Fixes a bug since 0.8.0: the first character typed into the search panel was replaced by the next one | Editor search |

Each patch says in its message when it can be removed. Send 0007 upstream first; it fixes an upstream regression.

### GPUI

GPUI Kit depends on `gpui-pre`, GPUI Kit's snapshot of Zed's GPUI crates. Its base tag `gpui-pre-v0.3.10` is that snapshot unmodified: Zed `08cbb88` staged by GPUI Kit v0.8.1's `script/bump-gpui.ts 0.3.10 --rev <rev> --zed <checkout> --stage-only`, matching the published crates. 0001 above pins the branch generated from it.

| Patch | What it does | Newport needs it for |
| --- | --- | --- |
| 0001 keyboard | Physical key and native layout metadata on key events; `Window::numlock()` | Terminal key encoding |
| 0002 tests | GPUI's own tests build inside the snapshot | Testing these patches |
| 0003 windows | Window control, popups, focus, UTF-16 text input and file drops on Windows | Remote apps on Windows |
| 0004 close | Drops a closed window's activation report, which otherwise logs "window not found" | Remote apps on Windows |
| 0005 dpi | Keeps a window's client size across DPI changes (`WM_GETDPISCALEDSIZE`) | Remote apps on Windows; mixed-DPI monitors |

To move to a newer snapshot, stage it with the generator, commit it as a root commit and tag it `gpui-pre-v<version>`, then follow "Move to a new release" with `gpui` before the tag, and update the pin in 0001.

### Deferred

`deferred/a11y.patch` holds the accessibility work from `newport/compat-main`: names for dialogs, tab lists, combobox, list rows, pagination, search and notifications, and menu trigger states. It applies on top of the v0.8.0 series, on a branch of its own:

```sh
git switch -c a11y newport/v0.8.0 && git am deferred/a11y.patch
```

On v0.8.1 it conflicts in the dialog and dropdown menu. To bring it back, rebase it onto `newport/v0.8.1`, keep only what upstream's own accessibility work lacks, move it to `patches/`, add it to `series`, and update Newport's calls.

## Generate the branch

```sh
./apply.sh gpui gpui-pre-v0.3.10
git fetch upstream --tags
./apply.sh v0.8.1
```

Generate GPUI first: GPUI Kit's 0001 pins the commit the GPUI series produces. Building either before both branches are pushed needs cargo to fetch this fork from the local clone (`CARGO_NET_GIT_FETCH_WITH_CLI=true` and a `url.<clone>.insteadOf` for the fork's URL), so lockfiles still record the GitHub URL.

Commits take their author and date from the patch, so the same series on the same tag always gives the same commit IDs. The script writes only the branch ref; it refuses to run while the branch is checked out.

## Change a patch

1. Check out the generated branch in a worktree and edit it there with `git commit --fixup` and `git rebase -i --autosquash v0.8.1`, or rebuild a commit.
2. Run `./export.sh v0.8.1` to rewrite `patches/` and `series`.
3. Run `./apply.sh v0.8.1` and check the branch tree matches what you tested.
4. Commit the changed patch files here.

For a GPUI patch, do the same with `gpui gpui-pre-v0.3.10`, then update the pin in GPUI Kit's 0001 and regenerate `newport/v0.8.1`.

## Move to a new release

1. `git fetch upstream --tags`, then `./apply.sh <new tag>`.
2. If a patch does not apply, finish the branch by hand: check out the commits that applied, `git am -3` the rest, resolve, and drop any change upstream now has.
3. `./export.sh <new tag>`, then `./apply.sh <new tag>` to confirm.
4. Point Newport at `newport/<new tag>` and run its tests.
