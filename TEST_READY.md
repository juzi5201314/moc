# TEST_READY — E2E Test Suite for moc-tui UI/UX Overhaul

## Executive Summary
The comprehensive, opaque-box E2E test suite for the `moc-tui` UI/UX Overhaul has been authored and verified. The test suite compiles cleanly with **0 compiler errors and 0 compiler warnings**, exercising all 20 required features (F1 to F20) across all 4 specified tiers (Tiers 1, 2, 3, and 4) with **226 test cases** (exceeding the >=225 requirement).

## Test Suite Architecture & Deliverables
All test files are strictly co-located within the test tree (`crates/moc-tui/tests/`), adhering to the workspace convention without modifying production implementation code (`src/`).

1. **Test Runner Entry Point**:
   - `crates/moc-tui/tests/e2e_ui_ux_overhaul.rs`: Main binary harness initializing terminal and aggregating modules.
2. **Aggregator Module**:
   - `crates/moc-tui/tests/e2e/mod.rs`: Module tree exposing common fixtures and all test tiers.
3. **Common Test Harness & Utilities**:
   - `crates/moc-tui/tests/e2e/common.rs`: Headless `ratatui::backend::TestBackend` initialization, noise prefilling, background leak detection (`area_has_bg`, `count_leaked_noise`), box corner analyzers (`is_rounded_box`, `is_sharp_box`), buffer extractors (`buffer_rows`, `buffer_compact_rows`, `buffer_squashed_rows`), and protocol fixture generators.
4. **Tier 1: Feature Coverage (100 Tests)**:
   - `crates/moc-tui/tests/e2e/tier1_features.rs`: 5 distinct requirement-driven test cases for each of F1 through F20.
5. **Tier 2: Boundary & Corner Cases (100 Tests)**:
   - `crates/moc-tui/tests/e2e/tier2_boundaries.rs`: 5 boundary/corner cases per feature (empty states, capacity limits, off-by-one thresholds: 14 vs 15 char commands, 3 vs 4 agents, 99 vs 100 col terminal widths, <80 col narrow viewports, minimum 40x15 size, unicode multi-byte handling, state transitions).
6. **Tier 3: Cross-Feature Combinations (21 Tests)**:
   - `crates/moc-tui/tests/e2e/tier3_combinations.rs`: Cross-module interaction tests (Queue + slash completion, mode menu + completion, sidebar + BTW dock simultaneous layout, todo HUD + subagent swarm, markdown tables in ask modal, context token pressure + modals, responsive collapse under load).
7. **Tier 4: Real-World Application Scenarios (5 Tests)**:
   - `crates/moc-tui/tests/e2e/tier4_scenarios.rs`: Realistic multi-step end-to-end workflows (Multi-agent collaborative task, high-density task with parallel queue and BTW dock, heavy markdown and permission modal flow, error recovery and session switch flow, responsive resizing under load).

## How to Run the Tests
```bash
# Verify test compilation with zero warnings/errors
cargo test --package moc-tui --test e2e_ui_ux_overhaul --no-run

# Run the complete E2E test suite
cargo test --package moc-tui --test e2e_ui_ux_overhaul

# Run specific tiers
cargo test --package moc-tui --test e2e_ui_ux_overhaul -- tier1_features
cargo test --package moc-tui --test e2e_ui_ux_overhaul -- tier2_boundaries
cargo test --package moc-tui --test e2e_ui_ux_overhaul -- tier3_combinations
cargo test --package moc-tui --test e2e_ui_ux_overhaul -- tier4_scenarios
```

## Baseline Execution Results
- **Total Tests Executed**: 226 tests
- **Passing Tests**: 178 tests (78.8%)
- **Failing Tests (Pre-Implementation Baseline)**: 48 tests (21.2%)

### Analysis of Failing Baseline Tests
The 48 pre-implementation test failures are **intentional specification-driven verification barriers** that faithfully detect the defects documented in `ORIGINAL_REQUEST.md` and `PROJECT.md` prior to the completion of Track 2 implementation milestones (M1–M5):

1. **R1.1 Modal Opacity & Background Clearing (F1)**:
   - Detected by `test_f1_01`, `test_f1_02`, `test_f1_03`, `test_f1_b1`, `test_f1_b5`, `test_combo_05`, `test_combo_06`, `test_combo_07`, `test_combo_08`.
   - Modals rendered transparently or leak pre-filled background noise through dialog headers/panels instead of rendering solid opaque background fill with `theme.bg_base`.
2. **R1.2 Border Style Unification (F2)**:
   - Detected by `test_f2_04`, `test_f2_b4`, `test_f2_b5`.
   - Outer dialog frames still exhibit mismatched sharp (`+---`) vs rounded (`╭───╮`) borders across modal variants.
3. **R2.1 & R2.2 Keyboard Navigation & Palette Alignment (F4, F7)**:
   - Detected by `test_f7_01`, `test_f7_03`, `test_f7_04`, `test_f7_05`.
   - Command names longer than 14 characters collide with descriptions due to hardcoded column offsets instead of dynamic padding.
4. **R3.1 Prompt Queue Indicator & Coexistence (F6, F10)**:
   - Detected by `test_f10_01`, `test_f10_04`, `test_f10_05`.
   - Prompt queue indicator collision with active slash completion popup.
5. **R4.1 BTW Bypass Dock Thinking Guides (F9, F20)**:
   - Detected by `test_f20_01`, `test_f20_02`, `test_f20_03`, `test_f20_05`.
   - BTW reasoning trace missing thinking diamond headers and styled guide indentation lines.
6. **R5.1, R5.2, R5.3 Markdown Formatting & Table Guides (F12, F13, F14, F18, F19)**:
   - Detected by `test_f18_01`, `test_f18_02`, `test_f18_03`, `test_f19_01`, `test_f19_02`, `test_f19_03`, `test_f19_04`, `test_f14_b5`.
   - Markdown table cells lose bold/italic/code modifiers; fenced code blocks nested in blockquotes/lists lose alignment guides.
7. **R6.1, R6.2, R6.3 Subagent Swarm Roster & Badges (F5, F6, F15, F16)**:
   - Detected by `test_f5_01`..`05`, `test_f6_01`..`03`.
   - Empty roster displays misleading `/agent` command text; status badges lack unified styling.

## Escalated Defects for Implementation Team (Track 2)
The implementing agent should reference the failing test cases as executable acceptance criteria for milestones M1–M5:
- **Milestone 1 (Visual Polish & Modal Opacity)**: Address `test_f1_*`, `test_f2_*`, `test_combo_05..08`. Ensure `frame.render_widget(Clear, area)` is accompanied by `buffer_mut().set_style(area, Style::new().bg(theme.bg_base))` or styled solid block widget.
- **Milestone 2 (Navigation & Command Palette)**: Address `test_f7_*` and `test_f8_*`. Implement dynamic command column calculation for 15+ char commands and structured help sections.
- **Milestone 3 (Queue & Prompt Ergonomics)**: Address `test_f10_*`. Ensure completion popup yields or properly layers over queue indicators.
- **Milestone 4 (BTW Dock & Markdown Enrichment)**: Address `test_f18_*`, `test_f19_*`, `test_f20_*`. Restore inline styling inside table cells and quote guides on code blocks.
- **Milestone 5 (Swarm Roster & Todo HUD)**: Address `test_f5_*`, `test_f6_*`, `test_f9_*`. Clean empty roster hints and align Todo HUD capsule positioning.
