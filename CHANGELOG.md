# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-09-19

### Changed

- README: added *Sponsorship & support*, *Acknowledgements*, and *AI disclosure* sections.
- README and TEST_LONG: generalized the URL-fetch guidance (removed an internal tool reference).

## [0.1.0] - 2026-09-19

Initial public release.

### Added

- MCP server exposing TypeSafe's System One API over `stdio` and `SSE` transports, built on [rmcp](https://crates.io/crates/rmcp).
- Core tools: `systemone` (parallel `choice`/`score`/`noul` questions against a shared state) and `list_models`.
- Bulk sifting tools with server-side chunking and merging: `filter_items`, `rank_items`, `classify_items`.
- Selection and stability tools: `choose_one` (two-pass skim → verify with calibrated rejection) and `stability_check` (N-sample aggregation: mean/std/min/max, modal share).
- Verification tools: `verify_output` (share-safety, groundedness, consistency, compliance, custom checks), `verify_tool_calls` (schema conformance + warnings), `check_claims` (supported/contradicted/unverified).
- Structure tools: `dedupe_items` (pairwise judgments → clusters) and `extract_fields` (typed extraction into fixed option sets).
- CLI configuration for transport mode, bind address, SSE paths, logging destination/level, API key, and API base URL.
- Documentation: `README.md`, `TEST_SUITE_SIMPLE.md` (15-test coverage suite), `TEST_LONG.md` (end-to-end pipeline test).

[0.1.1]: https://github.com/sf-stav/mcp_typesafe/releases/tag/v0.1.1
[0.1.0]: https://github.com/sf-stav/mcp_typesafe/releases/tag/v0.1.0
