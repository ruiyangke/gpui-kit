# Newport terminal keyboard compatibility

The newport/compat-main branch tracks upstream main and retains Newport's compatibility fixes. GPUI Kit 0.7.3 uses GPUI pre 0.3.9. Its six GPUI snapshot dependencies are pinned to commit ab40fa8bbac4114552547153f1df88fab550289e on this fork's codex/gpui-terminal-snapshot branch, instead of relying on local Cargo overrides. Internal GPUI dependencies use paths within that same Git revision, preserving one set of GPUI types. The separate gpui-pre-reqwest package is unchanged.

The snapshot derives from Zed 93d745668805029476e6d7fbb19652116fed51de using upstream's snapshot generator. Its source patch, generator, license files and crate transformation manifest are retained on the snapshot branch. No crates.io publication is needed.

PhysicalKey identifies a native key independently of the shortcut text. KeyEventMetadata carries original layout text, raw modifiers and lock states. KeyDownEvent and KeyUpEvent gain optional physical_key and key_metadata fields. Window::numlock() reads current backend state for focus-loss handling. macOS and Windows implement native layout metadata; synthetic input leaves it absent. GPUI Kit event constructors initialize the optional fields without changing shortcut dispatch. The optional GPUI Fast backend keeps its existing event constructors.

Terminal encoding remains Newport's responsibility. The snapshot changes event data only; it does not depend on libghostty. Native Windows runtime acceptance remains separate from compilation checks.
