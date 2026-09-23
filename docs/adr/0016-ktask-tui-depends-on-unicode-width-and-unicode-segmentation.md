# 0016. `ktask-tui` depends on `unicode-width` and `unicode-segmentation`

- **Status:** accepted
- **Date:** 2026-09-23

## Context

T138 makes wide and combining characters safe in the TUI's layout. Task and
gate names, paths and agent output can contain CJK text, emoji and combining
marks, none of which is one column per `char`. `unicode-width` and
`unicode-segmentation` are already fixed workspace dependencies (`Cargo.toml`,
`[workspace.dependencies]`; docs/DESIGN.md "Dependencies") and are already in
`Cargo.lock` through `ratatui`; no crate outside that set is added.

## Decision

`ktask-tui/Cargo.toml` adds both with `workspace = true`.

- `unicode-width` measures the columns a string occupies.
- `unicode-segmentation` supplies the grapheme-cluster boundaries, so
  truncation never separates a base character from its combining marks or cuts
  an emoji sequence.

`text::display_width` and `text::truncate_to_width` are the only places the
crate measures or shortens text for display.

## Alternatives considered

- **`unicode-truncate`.** Rejected: it is not in the fixed dependency set, and
  the function is a few lines over the two crates that are.
- **Counting `chars()`.** Rejected: wrong for wide and zero-width characters.

## Consequences

`Cargo.lock` records `ktask-tui`'s two new edges; both crates were already
resolved, so no new package is downloaded.
