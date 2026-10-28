# E2E Test Infra: moc-tui UI/UX Overhaul

## Test Philosophy
- Opaque-box, requirement-driven. Derived from `ORIGINAL_REQUEST.md`.
- No dependency on internal implementation structures.
- Methodology: Category-Partition + Boundary Value Analysis + Pairwise Combinatorial + Real-World Application Workload Testing.

## Feature Inventory & Test Coverage Matrix
| # | Feature | Source (Requirement) | Tier 1 | Tier 2 | Tier 3 | Tier 4 |
|---|---------|----------------------|:------:|:------:|:------:|:------:|
| 1 | Modal Background Opacity | ORIGINAL_REQUEST §R1.1 | 5 | 5 | ✓ | ✓ |
| 2 | Rounded Borders Unification | ORIGINAL_REQUEST §R1.2 | 5 | 5 | ✓ | ✓ |
| 3 | GrokNight Palette Hierarchy | ORIGINAL_REQUEST §R1.3 | 5 | 5 | ✓ | ✓ |
| 4 | Chinese Text Standardization | ORIGINAL_REQUEST §R2.1 | 5 | 5 | ✓ | ✓ |
| 5 | Subagent Roster Status Label | ORIGINAL_REQUEST §R2.2 | 5 | 5 | ✓ | ✓ |
| 6 | Subagent Empty Hint Cleanup | ORIGINAL_REQUEST §R2.3 | 5 | 5 | ✓ | ✓ |
| 7 | Dynamic Slash Completion Spacing | ORIGINAL_REQUEST §R2.4 | 5 | 5 | ✓ | ✓ |
| 8 | Help Modal Reorganization | ORIGINAL_REQUEST §R2.5 | 5 | 5 | ✓ | ✓ |
| 9 | Todo HUD Placement & Hover | ORIGINAL_REQUEST §R3.1 | 5 | 5 | ✓ | ✓ |
| 10 | Queue vs Completion Deconfliction | ORIGINAL_REQUEST §R3.2 | 5 | 5 | ✓ | ✓ |
| 11 | Subagent Sidebar Scrolling & Delimiters | ORIGINAL_REQUEST §R3.3 | 5 | 5 | ✓ | ✓ |
| 12 | BTW Dock Scroll & Footer Guard | ORIGINAL_REQUEST §R3.4 | 5 | 5 | ✓ | ✓ |
| 13 | Single-Line Up History & Visual Lines | ORIGINAL_REQUEST §R4.1 | 5 | 5 | ✓ | ✓ |
| 14 | Mouse Hit Targets (Tags & Modals) | ORIGINAL_REQUEST §R4.2 | 5 | 5 | ✓ | ✓ |
| 15 | Native macOS Clipboard Support | ORIGINAL_REQUEST §R4.3 | 5 | 5 | ✓ | ✓ |
| 16 | Backward Compatibility & Config | ORIGINAL_REQUEST §R4.4 | 5 | 5 | ✓ | ✓ |
| 17 | Markdown Links (OSC 8 & Fallback) | ORIGINAL_REQUEST §R5.1 | 5 | 5 | ✓ | ✓ |
| 18 | Table Cell Inline Formatting | ORIGINAL_REQUEST §R5.2 | 5 | 5 | ✓ | ✓ |
| 19 | Code Block Indentation Context | ORIGINAL_REQUEST §R5.3 | 5 | 5 | ✓ | ✓ |
| 20 | BTW Dock Thinking Trace Fidelity | ORIGINAL_REQUEST §R5.4 | 5 | 5 | ✓ | ✓ |

## Test Architecture
- **Test Runner**: Standard cargo test runner (`cargo test --package moc-tui --test e2e_ui_ux_overhaul` and in-crate test modules)
- **Invocation**: `cargo test --package moc-tui`
- **Pass/Fail Semantics**: Clean exit code 0, all tests pass, zero regressions across existing 293 unit/integration tests
- **Directory Layout**:
  - `crates/moc-tui/tests/e2e_ui_ux_overhaul.rs` (Opaque-box E2E test suite covering Tiers 1-4)
  - In-module unit/integration tests covering acceptance criteria

## Real-World Application Scenarios (Tier 4)
| # | Scenario | Features Exercised | Complexity |
|---|----------|--------------------|------------|
| 1 | Full Chat & Modal Interaction | F1, F2, F3, F8, F14 | High |
| 2 | Heavy Markdown Output (Tables, Code Blocks in Lists, Links) | F17, F18, F19 | High |
| 3 | Concurrent Queue, Slash Completion & Subagent Sidebar | F6, F7, F10, F11 | High |
| 4 | BTW Bypass Dock Thinking & Scrolling on Narrow Terminal | F1, F2, F12, F20 | Medium |
| 5 | Ergonomic Input Navigation, Tag Clicking & Clipboard Paste | F13, F14, F15, F16 | High |

## Coverage Thresholds
- Tier 1 (Feature Coverage): ≥5 per feature (20 × 5 = 100 cases)
- Tier 2 (Boundary & Corner Cases): ≥5 per feature (20 × 5 = 100 cases)
- Tier 3 (Cross-Feature Combinations): ≥20 interaction test cases
- Tier 4 (Real-World Application Scenarios): ≥5 end-to-end workload scenarios
- **Total Minimum Target: ~225 test cases**
