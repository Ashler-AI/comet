// Transcript — virtualized part rows with source-only height preparation and
// mounted-only Markdown parsing.
//
// Desktop parity (transcript.rs): GAP_TURN 14 / GAP_BLOCK 8 / MD_BLOCK_GAP 12,
// content column max 736, re-engage band 70, jump-button threshold 320,
// bottom pad 24. Rows are identified by stable ids and versioned by content
// fingerprints, so a streamed token re-renders exactly one row. SwiftUI's lazy
// stack + scroll APIs stand in for gpui's list(): the pin breaks only on
// user scroll-up and re-engages when approaching the bottom.

import SwiftUI

struct TranscriptView: View {
    let store: SessionStore
    let chatId: String

    static let gapTurn: CGFloat = 14
    static let gapBlock: CGFloat = 8
    static let maxContentWidth: CGFloat = 736
    static let stickThreshold: CGFloat = 70
    static let jumpThreshold: CGFloat = 320

    @State private var builder = TranscriptBuilderCache()
    @State private var veils = VeilStore()
    @State private var folds: [String: Bool] = [:]
    @State private var pinned = true
    @State private var distanceFromBottom: CGFloat = 0
    @State private var userScrolling = false
    @State private var scrollPosition = ScrollPosition(edge: .bottom)
    @State private var viewportWidth = UIScreen.main.bounds.width
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let rows = builder.rows(
            chatId: chatId,
            revision: store.transcriptRevision,
            entries: store.entries,
            pendingSends: store.pendingSends
        )
        let contentWidth = max(1, min(viewportWidth, Self.maxContentWidth) - 32)
        let preparedHeights = builder.prepareHeights(
            revision: store.transcriptRevision,
            rows: rows,
            width: contentWidth,
            folds: folds
        )
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(Array(rows.enumerated()), id: \.element.id) { ix, row in
                    let gap = rowGap(row, isFirst: ix == 0)
                    let expanded = builder.isExpanded(row, folds: folds)
                    rowView(row, isFirst: ix == 0)
                        .frame(
                            minHeight: gap + preparedHeights[ix],
                            alignment: .top
                        )
                        .onGeometryChange(for: CGFloat.self) { geo in
                            geo.size.height
                        } action: { _, actualHeight in
                            builder.recordMeasuredHeight(
                                max(0, actualHeight - gap),
                                for: row,
                                width: contentWidth,
                                expanded: expanded
                            )
                        }
                        .id(row.id)
                }
                Color.clear.frame(height: 44)  // bottom pad clears the fade + floating status strip
            }
            .frame(maxWidth: Self.maxContentWidth)
            .frame(maxWidth: .infinity)
        }
        .scrollPosition($scrollPosition)
        .defaultScrollAnchor(.bottom)
        .background(Theme.bg)
        .task {
            // Preloaded transcripts (disk hydration, demo) exist at first
            // layout, and lazy row materialization drifts the default bottom
            // anchor — snap once the first pass settles.
            try? await Task.sleep(nanoseconds: 80_000_000)
            scrollPosition.scrollTo(edge: .bottom)
        }
        .onGeometryChange(for: CGFloat.self) { geo in
            geo.size.width
        } action: { _, newWidth in
            if newWidth > 0 { viewportWidth = newWidth }
        }
        .onScrollPhaseChange { _, newPhase in
            // Desktop rule: the pin breaks only on USER input (wheel-up/drag),
            // never on streaming growth. Phases track the gesture.
            userScrolling = newPhase == .interacting || newPhase == .decelerating
        }
        .onScrollGeometryChange(for: CGFloat.self) { geo in
            max(0, geo.contentSize.height + geo.contentInsets.bottom - geo.containerSize.height - geo.contentOffset.y)
        } action: { old, new in
            distanceFromBottom = new
            if userScrolling, new > old + 1, new > 2 {
                pinned = false
            } else if !pinned, new <= Self.stickThreshold, new < old {
                // Re-stick only when moving TOWARD the bottom inside the 70pt
                // band, else the pin would be unbreakable.
                pinned = true
            }
        }
        .onChange(of: contentSignature(rows)) {
            guard pinned else { return }
            if reduceMotion {
                scrollPosition.scrollTo(edge: .bottom)
            } else {
                withAnimation(.spring(duration: 0.3)) {
                    scrollPosition.scrollTo(edge: .bottom)
                }
            }
        }
        .overlay(alignment: .top) {
            // Soft fade under the nav bar — content dissolves instead of
            // hard-clipping against the header.
            LinearGradient(
                stops: [
                    .init(color: Theme.bg, location: 0),
                    .init(color: Theme.bg.opacity(0.85), location: 0.45),
                    .init(color: Theme.bg.opacity(0), location: 1),
                ],
                startPoint: .top, endPoint: .bottom
            )
            .frame(height: 130)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
        }
        .overlay(alignment: .bottom) {
            // Short ramp that reaches FULL bg at the bottom edge — content
            // dissolves completely beneath the floating status strip, but the
            // fade starts low enough that message bottoms stay legible.
            LinearGradient(
                stops: [
                    .init(color: Theme.bg.opacity(0), location: 0),
                    .init(color: Theme.bg.opacity(0.55), location: 0.45),
                    .init(color: Theme.bg, location: 0.9),
                    .init(color: Theme.bg, location: 1),
                ],
                startPoint: .top, endPoint: .bottom
            )
            .frame(height: 44)
            .allowsHitTesting(false)
        }
        .overlay(alignment: .bottomTrailing) {
            // Jump-to-bottom floats ABOVE the fades.
            if distanceFromBottom > Self.jumpThreshold {
                Button {
                    pinned = true
                    withAnimation(.spring(duration: 0.35)) {
                        scrollPosition.scrollTo(edge: .bottom)
                    }
                } label: {
                    Image(systemName: "arrow.down")
                        .font(.system(size: 14, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .frame(width: 36, height: 36)
                }
                .glassEffect(.regular.interactive(), in: Circle())
                .padding(.trailing, 16)
                .padding(.bottom, 12)
                .transition(.opacity.combined(with: .move(edge: .bottom)))
            }
        }
        .motionAnimation(Motion.fadeQuick, value: distanceFromBottom > Self.jumpThreshold)
    }

    // Streamed growth signature: last row id + version + count. Any append or
    // reflow of the tail bumps it; scroll-back through history doesn't.
    private func contentSignature(_ rows: [TranscriptRow]) -> String {
        guard let last = rows.last else { return "" }
        return "\(rows.count)|\(last.id)|\(last.version)"
    }

    // MARK: Row rendering

    @ViewBuilder
    private func rowView(_ row: TranscriptRow, isFirst: Bool) -> some View {
        Group {
            switch row.kind {
            case .user(let text):
                UserBubble(text: text, pending: row.timestamp == nil)

            case .markdown(let source, let streaming):
                LazyMarkdownRowView(
                    row: row,
                    source: source,
                    streaming: streaming,
                    veils: veils,
                    cache: builder.markdownCache
                )

            case .toolGroup(let tools, let autoOpen):
                ToolGroupView(tools: tools,
                              open: folds[row.id] ?? autoOpen,
                              userToggled: folds[row.id] != nil) {
                    withAnimation(reduceMotion ? nil : Motion.resize) {
                        folds[row.id] = !(folds[row.id] ?? autoOpen)
                    }
                }

            case .inputChip(let header, let resolved):
                InputChipView(header: header, resolved: resolved)

            case .errorChip(let message):
                ErrorChipView(message: message)
            }
        }
        .padding(.top, rowGap(row, isFirst: isFirst))
        .padding(.horizontal, 16)
    }

    private func rowGap(_ row: TranscriptRow, isFirst: Bool) -> CGFloat {
        isFirst
            ? Self.gapTurn + 10
            : row.turnStart ? Self.gapTurn
            : Self.gapBlock
    }

}

/// Prepared transcript state retained across body evaluations: cheap part rows,
/// arithmetic heights, and parsed trees only for rows that have mounted.
final class TranscriptBuilderCache {
    private let heights = TranscriptHeightCache()
    let markdownCache = MarkdownRenderCache()
    private var lastChatId: String?
    private var lastRevision: UInt64?
    private var lastRows: [TranscriptRow] = []
    private var preparedRevision: UInt64?
    private var preparedWidthPixels: Int?
    private var preparedFoldFingerprint: Int?
    private var preparedHeights: [CGFloat] = []

    func rows(chatId: String,
              revision: UInt64,
              entries: [MessageEntry],
              pendingSends: [(messageId: String, text: String, at: Int64)]) -> [TranscriptRow] {
        if lastChatId == chatId, lastRevision == revision { return lastRows }
        if lastChatId != chatId {
            lastRows.removeAll(keepingCapacity: true)
            preparedHeights.removeAll(keepingCapacity: true)
            preparedRevision = nil
            heights.clear()
            markdownCache.clear()
        }
        let rows = TranscriptRowBuilder.rows(
            entries: entries,
            pendingSends: pendingSends
        )
        lastChatId = chatId
        lastRevision = revision
        lastRows = rows
        return rows
    }

    func prepareHeights(revision: UInt64, rows: [TranscriptRow], width: CGFloat,
                        folds: [String: Bool]) -> [CGFloat] {
        let widthPixels = Int((width * UIScreen.main.scale).rounded())
        var foldHasher = Hasher()
        for (key, value) in folds.sorted(by: { $0.key < $1.key }) {
            key.hash(into: &foldHasher)
            value.hash(into: &foldHasher)
        }
        let foldFingerprint = foldHasher.finalize()
        if preparedRevision == revision,
           preparedWidthPixels == widthPixels,
           preparedFoldFingerprint == foldFingerprint,
           preparedHeights.count == rows.count {
            return preparedHeights
        }
        let result = rows.map { row in
            let expanded = isExpanded(row, folds: folds)
            return heights.height(for: row, width: width, expanded: expanded)
        }
        preparedRevision = revision
        preparedWidthPixels = widthPixels
        preparedFoldFingerprint = foldFingerprint
        preparedHeights = result
        return result
    }

    func isExpanded(_ row: TranscriptRow, folds: [String: Bool]) -> Bool {
        guard case .toolGroup(_, let autoOpen) = row.kind else { return false }
        return folds[row.id] ?? autoOpen
    }

    func recordMeasuredHeight(_ height: CGFloat, for row: TranscriptRow,
                              width: CGFloat, expanded: Bool) {
        heights.storeMeasuredHeight(height, for: row, width: width, expanded: expanded)
        if let index = lastRows.firstIndex(where: { $0.id == row.id }),
           index < preparedHeights.count,
           height > preparedHeights[index] {
            preparedHeights[index] = ceil(height)
        }
    }
}

/// Veil registry — one RowVeil per live row, dropped on the live→complete flip.
@Observable
final class VeilStore {
    @ObservationIgnored private var veils: [String: RowVeil] = [:]

    func veil(for rowId: String, seeded: Bool) -> RowVeil {
        if let existing = veils[rowId] { return existing }
        let veil = RowVeil()
        veils[rowId] = veil
        return veil
    }

    func drop(_ rowId: String) {
        veils.removeValue(forKey: rowId)
    }
}

// MARK: - User bubble (transcript.rs:1671)

struct UserBubble: View {
    let text: String
    var pending = false

    var body: some View {
        HStack {
            Spacer(minLength: 0)
            Text(text)
                .font(Theme.sans(MD.textSize))
                .lineSpacing(MD.lineHeight - MD.textSize - 4)
                .foregroundStyle(Theme.text)
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
                .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: Theme.bubbleRadius))
                .frame(maxWidth: TranscriptView.maxContentWidth * 0.8, alignment: .trailing)
                .opacity(pending ? 0.65 : 1)
                .contextMenu {
                    Button {
                        UIPasteboard.general.string = text
                    } label: {
                        Label("Copy", systemImage: "doc.on.doc")
                    }
                }
        }
        .frame(maxWidth: .infinity, alignment: .trailing)
    }
}

// MARK: - Lazy Markdown parsing

/// Parsed trees for rows that have actually mounted. Stable row versions make
/// scroll-back a cache hit without parsing the off-screen transcript on open.
final class MarkdownRenderCache {
    private struct Entry {
        var version: UInt64
        var parser: IncrementalMarkdownParser
    }

    private var entries: [String: Entry] = [:]
    private var insertionOrder: [String] = []

    func clear() {
        entries.removeAll(keepingCapacity: true)
        insertionOrder.removeAll(keepingCapacity: true)
    }

    func blocks(for rowId: String, version: UInt64) -> [TopBlock]? {
        guard let entry = entries[rowId], entry.version == version else { return nil }
        return entry.parser.blocks
    }

    /// Return a value snapshot so parsing can mutate it off the main thread
    /// without racing cache reads. A streaming row therefore resumes from its
    /// previous stable block prefix instead of parsing the whole message.
    func parser(for rowId: String) -> IncrementalMarkdownParser {
        entries[rowId]?.parser ?? IncrementalMarkdownParser()
    }

    func store(_ parser: IncrementalMarkdownParser, for rowId: String, version: UInt64) {
        if entries[rowId] == nil {
            insertionOrder.append(rowId)
        }
        entries[rowId] = Entry(version: version, parser: parser)
        if insertionOrder.count > 2_048 {
            let evicted = insertionOrder.prefix(512)
            for key in evicted {
                entries.removeValue(forKey: key)
            }
            insertionOrder.removeFirst(min(512, insertionOrder.count))
        }
    }
}

/// The virtual row exists from raw source + arithmetic height immediately.
/// Only a mounted row pays for swift-markdown, and parsing runs after the
/// navigation frame rather than blocking the session press. Streaming rows
/// retain their incremental parser: plain tokens append directly, while
/// structural Markdown reparses only the unstable tail.
struct LazyMarkdownRowView: View {
    let row: TranscriptRow
    let source: String
    let streaming: Bool
    let veils: VeilStore
    let cache: MarkdownRenderCache

    @State private var blocks: [TopBlock] = []

    var body: some View {
        Group {
            if blocks.isEmpty {
                Color.clear
            } else {
                VStack(alignment: .leading, spacing: MD.blockGap) {
                    ForEach(Array(blocks.enumerated()), id: \.offset) { ix, top in
                        if streaming, ix == blocks.count - 1 {
                            MarkdownRowView(
                                row: row,
                                block: top.block,
                                streaming: true,
                                veils: veils
                            )
                        } else {
                            MarkdownBlockView(
                                block: top.block,
                                cacheKey: "\(row.id).\(ix)"
                            )
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .task(id: row.version) {
            if let cached = cache.blocks(for: row.id, version: row.version) {
                blocks = cached
                return
            }
            let text = source
            let version = row.version
            let parserSnapshot = cache.parser(for: row.id)
            let updatedParser = await Task.detached(priority: .userInitiated) {
                var updated = parserSnapshot
                updated.setText(text)
                return updated
            }.value
            guard !Task.isCancelled, version == row.version else { return }
            cache.store(updatedParser, for: row.id, version: version)
            blocks = updatedParser.blocks
        }
    }
}

// MARK: - Markdown row with veil

struct MarkdownRowView: View {
    let row: TranscriptRow
    let block: MDBlock
    let streaming: Bool
    let veils: VeilStore

    var body: some View {
        if streaming, isVeilable {
            TimelineView(.animation) { _ in
                veiledText
            }
            .onDisappear { veils.drop(row.id) }
        } else {
            MarkdownBlockView(block: block, cacheKey: row.id)
        }
    }

    private var isVeilable: Bool {
        switch block {
        case .paragraph, .heading: return true
        default: return false
        }
    }

    @ViewBuilder
    private var veiledText: some View {
        let veil = veils.veil(for: row.id, seeded: false)
        switch block {
        case .paragraph(let runs):
            let _ = veil.noteLength(runs.map(\.text.count).reduce(0, +))
            runs.styledVeiled(veil: veil)
                .textRenderer(InlineCodeRenderer())
                .lineSpacing(MD.lineHeight - MD.textSize - 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .heading(let level, let runs):
            let m = MD.headingMetrics(level)
            let _ = veil.noteLength(runs.map(\.text.count).reduce(0, +))
            runs.styledVeiled(size: m.size, weight: .semibold, veil: veil)
                .textRenderer(InlineCodeRenderer())
                .lineSpacing(m.line - m.size - 4)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        default:
            MarkdownBlockView(block: block, cacheKey: row.id)
        }
    }
}

// MARK: - Tool group (transcript.rs render_tool_group)

struct ToolGroupView: View {
    let tools: [ToolItem]
    let open: Bool
    let userToggled: Bool
    let toggle: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Header stays quiet even on failure — chips carry the red.
            Button(action: toggle) {
                HStack(spacing: 8) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(Theme.textMuted)
                        .rotationEffect(.degrees(open ? 90 : 0))
                        .frame(width: 18, height: 18)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 5))
                    Text(toolGroupSummary(tools))
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                    Spacer(minLength: 0)
                }
                .frame(height: 26)
                .contentShape(Rectangle())
            }
            .buttonStyle(PressWashButtonStyle(cornerRadius: 6))

            if open {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(tools.enumerated()), id: \.offset) { _, tool in
                        ToolChipRow(tool: tool)
                    }
                }
                .padding(.top, 2)
            }
        }
    }
}

/// 38pt row containing a 30pt card (transcript.rs tool_chip).
struct ToolChipRow: View {
    let tool: ToolItem

    var body: some View {
        HStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: tool.call.chipSymbol)
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.textMuted)
                    .frame(width: 18, height: 18)
                    .background(whiteAlpha(0.08), in: RoundedRectangle(cornerRadius: 5))
                Text(tool.call.chipLabel)
                    .font(Theme.sans(12, weight: .medium))
                    .foregroundStyle(tool.isError ? Theme.danger : Theme.textMuted)
                Text(tool.call.chipDetail)
                    .font(Theme.sans(12))
                    .foregroundStyle(tool.isError ? Theme.danger : Theme.text.opacity(0.85))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: 30)
            .background(whiteAlpha(0.03), in: RoundedRectangle(cornerRadius: 9))
            .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
            .padding(.leading, 12)
        }
        .frame(height: 38)
    }
}

// MARK: - Chips (transcript.rs ErrorChip / InputChip)

struct ErrorChipView: View {
    let message: String

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .font(.system(size: 10))
                .foregroundStyle(Theme.dangerSoft.opacity(0.8))
                .frame(width: 20, height: 20)
                .background(Theme.danger.opacity(0.12), in: RoundedRectangle(cornerRadius: 6))
            Text("Error")
                .font(Theme.sans(12, weight: .medium))
                .foregroundStyle(Theme.text)
            Text(message)
                .font(Theme.sans(12))
                .foregroundStyle(Theme.text.opacity(0.8))
                .lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .frame(height: 34)
        .background(Theme.danger.opacity(0.05), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Theme.danger.opacity(0.16), lineWidth: 1))
    }
}

struct InputChipView: View {
    let header: String
    let resolved: Bool

    var body: some View {
        // Neutral throughout — resolution never recolors.
        HStack(spacing: 8) {
            Image(systemName: "bubble.left.and.text.bubble.right")
                .font(.system(size: 10))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 20, height: 20)
                .background(whiteAlpha(0.09), in: RoundedRectangle(cornerRadius: 6))
            Text("Question")
                .font(Theme.sans(12, weight: .medium))
                .foregroundStyle(Theme.text)
            Text(resolved ? header : "Awaiting your answer…")
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textMuted)
                .lineLimit(1)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .frame(height: 34)
        .background(whiteAlpha(0.045), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(whiteAlpha(0.08), lineWidth: 1))
    }
}
