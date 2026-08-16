# Design

## Source of truth
- Status: Active
- Last refreshed: 2026-08-14
- Primary product surfaces: Native Comet composer, transcript output, harness/model picker, composer run configuration.
- Evidence reviewed: `crates/ui/src/composer.rs`, `crates/ui/src/transcript.rs`, `crates/ui/src/markdown/render.rs`, `crates/ui/src/markdown/selection.rs`, `crates/ui/src/pickers.rs`, `crates/ui/src/popover.rs`, `crates/ui/src/theme.rs`, `crates/ui/src/settings/composer.rs`, `crates/proto/src/agent.rs`, `crates/harness/src/omp/mod.rs`, `assets/brand/`, `crates/ui/assets/icons/`, `docs/screenshot.png`.

## Brand
- Personality: Quiet, technical, precise, native.
- Trust signals: Concrete selected state, provider-qualified model identity, explicit loading/error/disabled states, no unsupported availability claims.
- Avoid: Dashboard chrome, decorative gradients, oversized controls, provider logos that imply endorsement, manual account pinning in the model picker, or obscured provider/model identity.

## Product goals
- Goals: Make large model catalogs navigable; preserve exact model identity; route accounts automatically; provide native prompt and transcript text selection across scrolling and rendered block boundaries.
- Non-goals: Provider account management in the model picker, pricing comparison, model recommendations, or availability guarantees.
- Success signals: A user can select prompt text with native multi-click/Shift behavior, extend transcript selection while scrolling or Shift-clicking across code blocks, find a model by name or selector, and run it without choosing an account.

## Personas and jobs
- Primary personas: Developers running local coding agents through Comet and OMP.
- User jobs: Select text for reuse, select the exact provider/model, switch between model families quickly, and reuse a small working set from a large catalog.
- Key contexts of use: Prompt editing, transcript review/copy, new-session setup, pending Scaffold first-send configuration, and existing local chat configuration.

## Information architecture
- Primary navigation: Existing composer action row opens one combined agent/model/traits picker.
- Core routes/screens: No new route. The model picker retains the agent rail, adds an OMP provider section, and keeps the model list plus traits inspector.
- Content hierarchy: Agent → provider → searchable model list → model traits. Favorites is a provider-level saved view; account routing is automatic and has no picker section.

## Design principles
- Preserve identity: Display labels are human-readable, but selection and persistence always use the full provider-qualified selector.
- Progressive narrowing: Agent selection constrains provider options; provider selection constrains search; search never changes the underlying catalog.
- Separate pinning from selection: The favorite control must not select or close the model picker; account pinning is not exposed.
- Tradeoffs: Provider filtering is shown only for OMP because other harness catalogs do not expose the same multi-provider namespace. Search remains available for every harness; automatic account routing removes per-run account control.

## Visual language
- Color: Existing `Theme` tokens and hairline helpers only; active favorites use the normal selected/control foreground rather than a new accent.
- Typography: Existing 12–13 px menu scale; descriptions remain the existing muted 11 px subline.
- Spacing/layout rhythm: Existing 4/8/12 px picker rhythm. Provider rows reuse agent-rail row dimensions.
- Shape/radius/elevation: Existing 8 px rows, 12 px popover surface, selected-card shadow, and recessed header/footer bands.
- Motion: Existing menu entrance and hover-color transitions; no new motion primitive.
- Imagery/iconography: Existing monochrome Solar-style assets. Stars use outline/filled states and inherit text color.

## Components
- Existing components to reuse: `popover_card_flush`, `menu_heading`, `menu_row_nav`, `search_input_frame`, `ComposerInput`, `ScrollHandle`, `ComposerDefaults`, markdown selection registry.
- New/changed components: Prompt multi-click selection, virtualized transcript range selection, selectable code lines, OMP provider rail rows, searchable model projection, independent favorite button, persisted favorite selector list.
- Variants and states: Selection: caret/word/all/drag/Shift-extend/scroll-extend. Providers: Favorites/OpenAI/Anthropic/OpenRouter. Favorite: pinned/unpinned. Model: selected/highlighted/filtered/empty/loading/error.
- Token/component ownership: Prompt input remains in `crates/ui/src/composer.rs`; transcript selection remains in `crates/ui/src/markdown/selection.rs` and `render.rs`; picker layout remains in `crates/ui/src/pickers.rs`; durable composer preferences remain in `crates/ui/src/settings/composer.rs`; shared tokens remain in `Theme`/`popover`.

## Accessibility
- Target standard: Preserve native GPUI keyboard, focus, and text-selection behavior; no pointer-only requirement for finding models.
- Keyboard/focus behavior: Prompt Shift+Delete/Backspace still deletes; double-click selects a word; triple-click selects the prompt. Transcript Shift-click extends from the current anchor, including across code blocks. Opening the model picker focuses search; Up/Down traverses model rows, Enter selects, Escape closes.
- Contrast/readability: Selection uses the existing accent wash beneath glyphs; selected, muted, border, and text tokens remain unchanged; active star is distinguishable by both fill and color.
- Screen-reader semantics: Use stable control IDs and visible text labels; do not encode provider/favorite state by color alone. Selection remains copyable as ordered plain text.
- Reduced motion and sensory considerations: Reuse the existing motion layer, which owns app-level reduced-motion behavior.

## Responsive behavior
- Supported breakpoints/devices: Native desktop window sizes supported by Comet.
- Layout adaptations: Fixed-height popover; model pane scrolls independently; long labels/selectors truncate without widening the composer.
- Touch/hover differences: Desktop pointer and keyboard are primary; selected/favorite state remains visible without hover.

## Interaction states
- Loading: Preserve model skeleton/loading copy while catalog RPC is pending.
- Empty: Distinguish “No models found” from “No favorites yet.”
- Error: Preserve inline catalog error with Retry.
- Success: Prompt and transcript selections remain visibly stable while extending; model selection closes the picker and updates the existing model chip; favoriting keeps the picker open; account routing stays automatic.
- Disabled: Existing-chat harness lock continues to dim foreign agent rows; provider browsing does not alter that rule. Modifier keys never disable deletion.
- Offline/slow network, if applicable: Cached model labels and favorites remain available in preferences; catalog rows still require the existing RPC result.

## Content voice
- Tone: Short, factual, provider-native.
- Terminology: “Agents,” “Providers,” “Models,” “Favorites,” “Search models…”. Use provider names OpenAI, Anthropic, and OpenRouter; omit account-routing copy from the picker.
- Microcopy rules: Never claim a catalog entry is runnable or authorized; automatic account routing has no selectable state; empty/error states explain the immediate condition only.

## Implementation constraints
- Framework/styling system: Rust + GPUI; existing `Theme`, `popover`, and composer input primitives.
- Design-token constraints: No new palette or spacing system.
- Performance constraints: Provider/search projections remain linear in catalog size and avoid network reloads; favorites persist as ordered selector strings; transcript selection stores only selected text spans plus the visible geometry registry.
- Compatibility constraints: Existing model selectors, reasoning ladders, model options, chat locks, and Scaffold first-send behavior remain unchanged; Comet-originated runs send no account pin.
- Test/screenshot expectations: Unit tests cover prompt multi-click ranges, virtualized/Shift transcript extension, automatic routing, provider grouping, search, favorite ordering/persistence, and complete OMP catalog conversion. Validate prompt, transcript, code-block, and live picker behavior in a separate isolated Comet process and viewport.

## Open questions
- [ ] Whether future catalog metadata should expose provider as a typed wire field rather than deriving it from the selector / UI platform / only material if new selector namespaces are added.
