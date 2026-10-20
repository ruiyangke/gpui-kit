# Newport patches for GPUI Kit

This branch holds the patches Newport applies to [GPUI Kit](https://github.com/longbridge/gpui-kit). It has no GPUI Kit source.

Newport builds against `newport/<tag>`, a branch generated from an upstream release tag and this series. Never edit a generated branch by hand; change a patch and generate it again.

Current base: `v0.8.0` → branch `newport/v0.8.0`.

## Files

| File | Purpose |
| --- | --- |
| `series` | Patch order |
| `patches/` | One patch per change, in `git format-patch` format |
| `deferred/` | Patches kept for later; not in `series` |
| `apply.sh` | Generate `newport/<tag>` from a tag and the series |
| `export.sh` | Write `patches/` and `series` from a branch |

## Patches

| Patch | What it does | Newport needs it for |
| --- | --- | --- |
| 0001 gpui | Pins the GPUI snapshot with physical keys and layout metadata on key events | Terminal key encoding |
| 0002 dialog | `DialogHandle`, `close_dialog_if`, one owner per focus restoration, `Dialog::centered` | Closing its own dialog; centered dialogs |
| 0003 menu | Menus dismiss before running an item; dropdowns return focus to their trigger; `Button::track_focus`, `DropdownMenuPopover::open`, `DropdownButton::menu_trigger` | Focus after menu actions that open dialogs |
| 0004 select | Select and Combobox confirm by value, not row; `ComboboxEvent::Activate`, `ComboboxState::close`, `Select::is_open`, `menu_scroll_handle`, `menu_item_min_height` | Paged and filtered pickers |
| 0005 controls | Explicit sizes: `Button::content_style`, Pagination styles, Switch track and label size, `Size::Size` for Checkbox and Tab, `TabBar::label_size`, input `content_padding` | Pixel-sized controls |
| 0006 text | Copies selections without trimming; lays out tabs at tab stops; keeps fractional line heights; public `SelectableText::paint_selection` | Logs, diffs and the code editor |
| 0007 search | Fixes a 0.8.0 bug: the first character typed into the search panel was replaced by the next one | Editor search |

Each patch says in its message when it can be removed. Send 0007 upstream first; it fixes an upstream regression.

### GPUI snapshot

0001 pins the six `gpui-pre` 0.3.9 crates to commit `ab40fa8` on this fork's `codex/gpui-terminal-snapshot` branch. That snapshot is Zed `93d7456` (the same revision as `gpui-pre` 0.3.9), built with upstream's snapshot generator, plus `terminal-keyboard.patch`. When upstream moves to a newer snapshot, rebuild that branch from the new revision with the same patch, and update the pins in 0001.

### Deferred

`deferred/a11y.patch` holds the accessibility work from `newport/compat-main`: names for dialogs, tab lists, combobox, list rows, pagination, search and notifications, and menu trigger states. It applies on top of the series, on a branch of its own:

```sh
git switch -c a11y newport/v0.8.0 && git am deferred/a11y.patch
```

To bring it back, move it to `patches/`, add it to `series`, and update Newport's calls.

## Generate the branch

```sh
git fetch upstream --tags
./apply.sh v0.8.0
```

Commits take their author and date from the patch, so the same series on the same tag always gives the same commit IDs. The script writes only the branch ref; it refuses to run while the branch is checked out.

## Change a patch

1. Check out the generated branch in a worktree and edit it there with `git commit --fixup` and `git rebase -i --autosquash v0.8.0`, or rebuild a commit.
2. Run `./export.sh v0.8.0` to rewrite `patches/` and `series`.
3. Run `./apply.sh v0.8.0` and check the branch tree matches what you tested.
4. Commit the changed patch files here.

## Move to a new release

1. `git fetch upstream --tags`, then `./apply.sh <new tag>`.
2. If a patch does not apply, finish the branch by hand: check out the commits that applied, `git am -3` the rest, resolve, and drop any change upstream now has.
3. `./export.sh <new tag>`, then `./apply.sh <new tag>` to confirm.
4. Point Newport at `newport/<new tag>` and run its tests.
