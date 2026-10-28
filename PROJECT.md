# Project: moc-tui Comprehensive UI/UX Overhaul

## Architecture
`moc-tui` is a terminal user interface built on `ratatui` (0.30.2) and `crossterm` (0.29).
The UI consists of:
- **Frame Layout**: Header area, gap, scrollback view (chat history, tool calls, thinking blocks, markdown), gap, prompt input, status bar.
- **Overlays & Modals**: Modal dialogs (`Ask`, `Context`, `Model`, `Permission`, `Sessions`, `Plan`, `Subagents`, `Help`, `Rewind`), Slash Completion menu, Mode menu, Settings popups, Queue popup, Todo HUD capsule/card, BTW bypass dock.
- **Event Handling**: Keyboard and mouse event dispatching, mouse hit targets routing.
- **Markdown Pipeline**: Event-based Markdown renderer converting CommonMark tokens to styled Ratatui Lines and Spans.

## Feature Inventory
| # | Feature | Description | Milestone | Source |
|---|---------|-------------|-----------|--------|
| F1 | Modal Background Opacity | Clear calls on all modals, popups, and viewers paired with explicit solid bg (`bg_base` / `bg_panel`) | M1 | R1.1 |
| F2 | Rounded Borders Unification | Unify all dialogs, bypass docks, task cards, queue popups, and rewind panels to `ROUNDED` border set | M1 | R1.2 |
| F3 | GrokNight Gray & Thinking Theme | Refine `accent_thinking` to distinct slate tone and redistribute grayscale steps monotonically | M1 | R1.3 |
| F4 | Chinese Text Standardization | Eliminate English remnants in shortcut descriptions ("cancel") and toasts ("Stopping...", "clear input", "Stopped") | M2 | R2.1 |
| F5 | Subagent Roster Status Label | Format status through `agent_status_label` instead of raw integer cast (`agent.status as u8`) | M2 | R2.2 |
| F6 | Subagent Empty Hint Cleanup | Remove misleading nonexistent `/agent <task>` hint from empty subagent roster | M2 | R2.3 |
| F7 | Dynamic Slash Completion Spacing | Format slash commands with dynamic padding and tab stops so >14 char commands never collide with descriptions | M2 | R2.4 |
| F8 | Help Modal Reorganization | Reorganize Ctrl+H Help modal into 3 distinct structured sections: Session Commands, Editing Shortcuts, Bypass Interaction | M2 | R2.5 |
| F9 | Todo HUD Placement & Hover | Constrain/relocate Todo HUD capsule so it never obscures scrollback text; replace instant hover expansion with click/dwell | M3 | R3.1 |
| F10 | Queue vs Completion Deconfliction | Resolve coordinate collision between prompt Queue popup and Slash Completion menu via graceful yielding / stacking | M3 | R3.2 |
| F11 | Subagent Sidebar Scrolling & Delimiters | Add vertical scrolling, scroll indicators, and distinct visual delimiters to sidebar when >3 agents exist | M3 | R3.3 |
| F12 | BTW Dock Scroll & Footer Responsiveness | Add scroll progress indicator to BTW dock and guard footer shortcuts against overflow on narrow viewports | M3 | R3.4 |
| F13 | Single-Line Up History & Visual Lines | Single-line prompt ↑ recalls history on first press; implement visual-line movement for soft-wrapped text | M4 | R4.1 |
| F14 | Mouse Hit Targets for Tags & Modals | Add mouse hit targets for prompt bottom tags (model, permission) and modal items (Permission, Model, Sessions) | M4 | R4.2 |
| F15 | Native macOS Clipboard | Implement native `pbcopy`/`pbpaste` support alongside WSL and Linux Wayland/X11 | M4 | R4.3 |
| F16 | Backward Compatibility & Config | Preserve established habits (`Home`/`Ctrl+A`) and ensure non-breaking behavior | M4 | R4.4 |
| F17 | Markdown Link URLs & Fallback References | Preserve Markdown link URLs with OSC 8 hyperlinks and fallback numbered URL reference list | M5 | R5.1 |
| F18 | Table Cell Inline Formatting | Retain inline formatting (bold, italic, code) in table cells using styled run wrapping | M5 | R5.2 |
| F19 | Code Block Indentation Context | Maintain quote (`│ `) and list indentation context for fenced code blocks | M5 | R5.3 |
| F20 | BTW Dock Thinking Trace Fidelity | Upgrade BTW bypass dock thinking traces to match main viewport (`◆ 思考` header and `▏ ` border in `accent_thinking`) | M5 | R5.4 |
| F21 | E2E Testing Suite & Automated Verification | Full test suite covering Tiers 1-4 tests, automated checks, and regression tests | M6 | Acceptance Criteria |

## Milestones
| # | Name | Scope | Dependencies | Status |
|---|------|-------|-------------|--------|
| M1 | Visual Aesthetics & Theming | F1, F2, F3 | none | IN_PROGRESS (worker: 09bc40c3) |
| M2 | Design & Language Consistency | F4, F5, F6, F7, F8 | none | PLANNED |
| M3 | Spatial Layout & Viewport | F9, F10, F11, F12 | M1 | PLANNED |
| M4 | HCI & Ergonomics | F13, F14, F15, F16 | M1, M2 | PLANNED |
| M5 | Content Typography & Rich-Text | F17, F18, F19, F20 | M1 | PLANNED |
| M6 | Integration, E2E & Hardening | F21 (Full Test Suite, 100% Pass, Tier 5 Adversarial Hardening) | M1, M2, M3, M4, M5 | PLANNED |

## Interface Contracts & Data Structures
### Theme & Borders
- `Theme.accent_thinking`: `Color::Rgb(0x73, 0x7d, 0x94)`
- `Theme.gray_dim`: `(0x54, 0x54, 0x5a)`, `Theme.gray`: `(0x78, 0x78, 0x82)`, `Theme.gray_bright`: `(0x9e, 0x9e, 0xaa)`
- Standard border set: `ratatui::symbols::border::ROUNDED`

### Mouse Hit Targets in `App`
- `model_tag_area: Rect`: prompt bottom border model picker hit area
- `permission_tag_area: Rect`: prompt bottom border permission mode hit area
- `modal_item_hits: Vec<(usize, Rect)>`: index and bounding box of items in `Modal::Permission`, `Modal::Model`, and `Modal::Sessions`
- `subagent_scroll: usize`: vertical scroll offset for subagent sidebar

### Markdown Rendering
- `wrap_cell_runs(runs: &[Run], width: usize) -> Vec<Vec<Span<'static>>>` for table cells
- `flush_code`: prepends `quote = "│ ".repeat(self.quote_depth)` and list indentation spans
- `links: Vec<(String, String)>`: captures `(label, dest_url)` for OSC 8 and fallback references block

## Code Layout & Write Boundaries
- **Milestone 1**: `crates/moc-tui/src/theme.rs`, `crates/moc-tui/src/queue.rs`, `crates/moc-tui/src/render/todo_hud.rs`, `crates/moc-tui/src/render/rewind.rs`, `crates/moc-tui/src/render/context_content.rs`, `crates/moc-tui/src/settings/render.rs`
- **Milestone 2**: `crates/moc-tui/src/config.rs`, `crates/moc-tui/src/widgets/completion.rs`, `crates/moc-tui/src/render/modal.rs`
- **Milestone 3**: `crates/moc-tui/src/render/subagent.rs`, `crates/moc-tui/src/queue.rs`, `crates/moc-tui/src/render/todo_hud.rs`, `crates/moc-tui/src/render/btw.rs`
- **Milestone 4**: `crates/moc-tui/src/widgets/prompt.rs`, `crates/moc-tui/src/clipboard.rs`, `crates/moc-tui/src/app.rs`, `crates/moc-tui/src/event.rs`, `crates/moc-tui/src/render/mod.rs`
- **Milestone 5**: `crates/moc-tui/src/render/markdown.rs`, `crates/moc-tui/src/render/wrap.rs`, `crates/moc-tui/src/render/btw.rs`, `crates/moc-tui/src/btw_tests.rs`
- **E2E Testing Track**: `tests/` or dedicated test files under `crates/moc-tui/tests/` (e2e integration tests)
