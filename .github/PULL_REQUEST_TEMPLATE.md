<!--
Thanks for the PR.

Quick checklist before submitting:

- [ ] `cargo fmt --all -- --check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test --all-targets` green on your platform
- [ ] If this touches the durability path (WAL / redb / HNSW
      snapshot+delta / backup / restore / schema migration),
      `scripts/manual_test.sh --stub` end-to-end run is green
- [ ] CHANGELOG / docs updated if user-visible
-->

## Summary

<!-- 1–3 sentences: what changes, why. The `why` is more important
than the `what` (the diff covers the what). -->

## Approach

<!-- A short paragraph on the design choice and what alternatives
you considered, especially if you touched a seam (Storage / Embedder
/ VectorIndex), a scheduler, or the MCP surface. -->

## Test coverage

<!-- Inline tests added? Integration tests? Manual smoke (which
script, on what platform)? Be honest about what isn't covered yet. -->

## Risk

<!-- One sentence on what would break if this lands and the user
hits an unforeseen edge case. "Low" / "Medium" / "High" alone is not
useful — name the failure mode. -->

## Related

<!-- Linked issue / discussion / preceding PR. If this closes an
issue, write `Closes #N` so GitHub auto-links it. -->
