// Font-driven row heights for the mobile transcript virtualizer.
//
// LazyVStack is only cheap when it can establish off-screen geometry without
// constructing every SwiftUI text tree. Heights here use the same Geist fonts,
// line heights, paddings and block metrics as the row views. Mounted rows still
// render normally; the prepared minimum gives the lazy stack its scroll model,
// and a larger mounted measurement is cached as the authoritative correction.

import SwiftUI
import UIKit

final class TranscriptHeightCache {
    private struct Key: Hashable {
        var id: String
        var version: UInt64
        var widthPixels: Int
        var expanded: Bool
    }

    private struct WidthKey: Hashable {
        var fontName: String
        var pointSize: CGFloat
        var text: String
    }

    private var values: [Key: CGFloat] = [:]
    /// Canvas-style shaping cache: once a token has been measured in a font,
    /// every row can reuse its width and line wrapping becomes pure arithmetic.
    private var tokenWidths: [WidthKey: CGFloat] = [:]

    func clear() {
        values.removeAll(keepingCapacity: true)
    }

    func height(for row: TranscriptRow, width: CGFloat, expanded: Bool = false) -> CGFloat {
        let key = key(for: row, width: width, expanded: expanded)
        if let cached = values[key] { return cached }
        let measured = ceil(contentHeight(for: row.kind, width: max(1, width), expanded: expanded))
        values[key] = measured
        if values.count > 20_000 {
            // Width/version keys naturally repopulate; a wholesale trim is
            // cheaper than maintaining an LRU during a scroll gesture.
            values = [key: measured]
        }
        return measured
    }

    func storeMeasuredHeight(_ height: CGFloat, for row: TranscriptRow,
                             width: CGFloat, expanded: Bool) {
        guard height > 0 else { return }
        values[key(for: row, width: width, expanded: expanded)] = ceil(height)
    }

    private func key(for row: TranscriptRow, width: CGFloat, expanded: Bool) -> Key {
        let scale = UIScreen.main.scale
        return Key(
            id: row.id,
            version: row.version,
            widthPixels: Int((width * scale).rounded()),
            expanded: expanded
        )
    }

    private func contentHeight(for kind: RowKind, width: CGFloat, expanded: Bool) -> CGFloat {
        switch kind {
        case .user(let text):
            let bubbleWidth = max(1, min(width, TranscriptView.maxContentWidth * 0.8) - 32)
            return measure(text: text, font: Theme.sansUI(MD.textSize),
                           lineHeight: MD.lineHeight, width: bubbleWidth) + 20

        case .markdown(let source, _):
            return markdownSourceHeight(source, width: width)

        case .toolGroup(let tools, _):
            return 26 + (expanded ? 2 + CGFloat(tools.count) * 38 : 0)

        case .inputChip, .errorChip:
            return 34
        }
    }

    /// Fast source-only Markdown layout. This deliberately does not construct
    /// a swift-markdown Document: it recognizes height-affecting block syntax,
    /// measures prose with the resolved fonts, and uses analytic metrics for
    /// code/tables/rules. The full AST is deferred until the row mounts.
    private func markdownSourceHeight(_ source: String, width: CGFloat) -> CGFloat {
        let lines = source.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        var blockHeights: [CGFloat] = []
        var index = 0

        while index < lines.count {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.isEmpty {
                index += 1
                continue
            }

            if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
                let marker = String(trimmed.prefix(3))
                let language = String(trimmed.dropFirst(3)).trimmingCharacters(in: .whitespaces)
                var codeLines = 0
                index += 1
                while index < lines.count {
                    if lines[index].trimmingCharacters(in: .whitespaces).hasPrefix(marker) {
                        index += 1
                        break
                    }
                    codeLines += 1
                    index += 1
                }
                let header: CGFloat = language.isEmpty ? 0 : 24
                blockHeights.append(
                    header + CGFloat(max(1, codeLines)) * MD.codeLineHeight + 2 * MD.codePaddingY
                )
                continue
            }

            if let heading = headingSource(trimmed) {
                let metrics = MD.headingMetrics(heading.level)
                blockHeights.append(
                    measure(text: heading.text,
                            font: Theme.sansUI(metrics.size, weight: .semibold),
                            lineHeight: metrics.line,
                            width: width)
                )
                index += 1
                continue
            }

            if isRule(trimmed) {
                blockHeights.append(1)
                index += 1
                continue
            }

            if isTableStart(lines, at: index) {
                var rowCount = 1 // header
                index += 2       // header + delimiter
                while index < lines.count, lines[index].contains("|"),
                      !lines[index].trimmingCharacters(in: .whitespaces).isEmpty {
                    rowCount += 1
                    index += 1
                }
                blockHeights.append(
                    CGFloat(rowCount) * (MD.lineHeight + 24) + CGFloat(max(0, rowCount - 1))
                )
                continue
            }

            if isListLine(trimmed) {
                var height: CGFloat = 0
                var count = 0
                while index < lines.count {
                    let item = lines[index].trimmingCharacters(in: .whitespaces)
                    guard isListLine(item) else { break }
                    let text = stripListMarker(item)
                    height += max(
                        MD.lineHeight,
                        measure(text: text, font: Theme.sansUI(MD.textSize),
                                lineHeight: MD.lineHeight, width: max(1, width - 26))
                    )
                    count += 1
                    index += 1
                }
                blockHeights.append(height + CGFloat(max(0, count - 1)) * 4)
                continue
            }

            var paragraph: [String] = []
            var quote = false
            while index < lines.count {
                let candidate = lines[index].trimmingCharacters(in: .whitespaces)
                if candidate.isEmpty || candidate.hasPrefix("```") || candidate.hasPrefix("~~~")
                    || headingSource(candidate) != nil || isRule(candidate)
                    || isTableStart(lines, at: index) || isListLine(candidate) {
                    break
                }
                if candidate.hasPrefix(">") {
                    quote = true
                    paragraph.append(
                        String(candidate.dropFirst()).trimmingCharacters(in: .whitespaces)
                    )
                } else {
                    paragraph.append(candidate)
                }
                index += 1
            }
            if paragraph.isEmpty {
                // Unknown syntax must still make progress.
                paragraph.append(trimmed)
                index += 1
            }
            let inset: CGFloat = quote ? 22 : 0
            let prose = paragraph.joined(separator: " ")
            let measured = measure(
                text: prose,
                font: Theme.sansUI(MD.textSize),
                lineHeight: MD.lineHeight,
                width: max(1, width - inset)
            )
            blockHeights.append(measured + (quote ? 12 : 0))
        }

        return blockHeights.reduce(0, +)
            + CGFloat(max(0, blockHeights.count - 1)) * MD.blockGap
    }

    private func headingSource(_ line: String) -> (level: Int, text: String)? {
        let hashes = line.prefix { $0 == "#" }.count
        guard (1...6).contains(hashes) else { return nil }
        let remainder = String(line.dropFirst(hashes))
        guard remainder.first?.isWhitespace == true else { return nil }
        return (hashes, remainder.trimmingCharacters(in: .whitespaces))
    }

    private func isRule(_ line: String) -> Bool {
        let compact = line.filter { !$0.isWhitespace }
        guard compact.count >= 3, let first = compact.first,
              first == "-" || first == "*" || first == "_" else { return false }
        return compact.allSatisfy { $0 == first }
    }

    private func isTableStart(_ lines: [String], at index: Int) -> Bool {
        guard index + 1 < lines.count, lines[index].contains("|") else { return false }
        let delimiter = lines[index + 1]
            .filter { !$0.isWhitespace && $0 != "|" && $0 != ":" }
        return delimiter.count >= 3 && delimiter.allSatisfy { $0 == "-" }
    }

    private func isListLine(_ line: String) -> Bool {
        if line.hasPrefix("- ") || line.hasPrefix("* ") || line.hasPrefix("+ ") { return true }
        guard let dot = line.firstIndex(of: ".") else { return false }
        return !line[..<dot].isEmpty
            && line[..<dot].allSatisfy { $0.isNumber }
            && line[line.index(after: dot)...].first?.isWhitespace == true
    }

    private func stripListMarker(_ line: String) -> String {
        if line.hasPrefix("- ") || line.hasPrefix("* ") || line.hasPrefix("+ ") {
            return String(line.dropFirst(2))
        }
        guard let dot = line.firstIndex(of: ".") else { return line }
        return String(line[line.index(after: dot)...]).trimmingCharacters(in: .whitespaces)
    }

    /// Mugen-style measurement: shape each distinct token once, cache its
    /// width, then calculate wrapping with additions and comparisons. This
    /// avoids allocating attributed strings and running TextKit layout for
    /// every paragraph in a large session.
    private func measure(text: String, font: UIFont, lineHeight: CGFloat,
                         width: CGFloat) -> CGFloat {
        guard !text.isEmpty else { return 0 }
        let available = max(1, width)
        var lineCount = 1
        var lineWidth: CGFloat = 0
        var pendingWhitespace: CGFloat = 0

        func placeWord(_ word: String) {
            let wordWidth = measuredWidth(word, font: font)
            if lineWidth > 0, lineWidth + pendingWhitespace + wordWidth <= available + 0.5 {
                lineWidth += pendingWhitespace + wordWidth
                pendingWhitespace = 0
                return
            }
            if lineWidth > 0 {
                lineCount += 1
                lineWidth = 0
                pendingWhitespace = 0
            }
            if wordWidth <= available + 0.5 {
                lineWidth = wordWidth
                return
            }

            // UIKit falls back to grapheme wrapping for an unbroken token that
            // is wider than the line. This path is rare (long URLs/code), and
            // individual grapheme widths share the same cache.
            for character in word {
                let characterWidth = measuredWidth(String(character), font: font)
                if lineWidth > 0, lineWidth + characterWidth > available + 0.5 {
                    lineCount += 1
                    lineWidth = 0
                }
                lineWidth += characterWidth
            }
        }

        func placeWhitespace(_ whitespace: String) {
            guard lineWidth > 0 else { return }
            pendingWhitespace += measuredWidth(whitespace, font: font)
        }

        var token = ""
        var tokenIsWhitespace: Bool?

        func flushToken() {
            guard !token.isEmpty, let whitespace = tokenIsWhitespace else { return }
            if whitespace {
                placeWhitespace(token)
            } else {
                placeWord(token)
            }
            token.removeAll(keepingCapacity: true)
            tokenIsWhitespace = nil
        }

        for character in text {
            if character.isNewline {
                flushToken()
                lineCount += 1
                lineWidth = 0
                pendingWhitespace = 0
                continue
            }
            let whitespace = character.isWhitespace
            if let currentKind = tokenIsWhitespace, currentKind != whitespace {
                flushToken()
            }
            tokenIsWhitespace = whitespace
            token.append(character)
        }
        flushToken()

        return CGFloat(lineCount) * lineHeight
    }

    private func measuredWidth(_ text: String, font: UIFont) -> CGFloat {
        let key = WidthKey(fontName: font.fontName, pointSize: font.pointSize, text: text)
        if let cached = tokenWidths[key] { return cached }
        let width = (text as NSString).size(withAttributes: [.font: font]).width
        tokenWidths[key] = width
        if tokenWidths.count > 30_000 {
            // Token widths are cheap to repopulate and independent of row
            // identity. Bound retained source strings on unusually varied logs.
            tokenWidths = [key: width]
        }
        return width
    }
}
