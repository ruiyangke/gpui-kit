# GPUI snapshot for Newport

GPUI pre 0.3.10: Zed `08cbb88a152c3751a43e853425a70e9f631a5918`, staged by GPUI Kit v0.8.1's generator:

```sh
script/bump-gpui.ts 0.3.10 --rev 08cbb88a152c3751a43e853425a70e9f631a5918 --zed PATH --stage-only
```

This commit is unmodified: every crate's sources and manifest match the `gpui-pre-*` 0.3.10 crates on crates.io. `snapshot.json` is the generator's record of crate transformations. Keep the per-crate license files and notices.

Newport's changes are a patch series on this fork's `newport/patches` branch, in `gpui/`. Its `apply.sh` builds `newport/gpui-pre-v0.3.10` from this commit; Newport builds against that branch.
