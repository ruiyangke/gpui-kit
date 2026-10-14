# Newport GPUI snapshot

GPUI pre 0.3.9 from Zed 93d745668805029476e6d7fbb19652116fed51de, with native keyboard metadata for terminal applications. This branch supplies Cargo Git dependencies; it does not publish packages to crates.io.

Generated with GPUI Kit's script/bump-gpui.ts from upstream c5f8eca4, preserved at patches/bump-gpui.ts. Apply patches/terminal-keyboard.patch to the pinned Zed source, then run the generator with 0.3.9 --zed PATH --stage-only. Its Apache-2.0 license audit passes for all 23 staged crates and the resolved dependency graph. snapshot.json records crate transformations. Keep the per-crate license files and notices.

The patch exposes physical key identity and original layout/modifier metadata without changing shortcut matching. macOS and Windows supply layout metadata; Linux/web retain optional fallbacks. Window::numlock() queries current platform lock state, with Windows implementation. Synthetic events carry no native metadata. Newport owns terminal-specific key encoding.

Adding optional fields to KeyDownEvent and KeyUpEvent requires downstream struct literals to initialize them. GPUI Kit's newport/compat-main branch provides those adaptations. Native Windows acceptance is still pending; compilation alone is not runtime acceptance.
