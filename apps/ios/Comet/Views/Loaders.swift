// Loaders + status indicators — ports of crates/ui/src/loaders.rs.
//
// gradient-spin-pulse: a 3×3 cell grid with per-row "sunrise" tints; each cell
// pulses once per 750ms with phase = distance from bottom-center, so the wave
// travels upward. Session rows use the desktop's thin 9pt arc spinner.

import SwiftUI

enum GradientSpin {
    // GSPIN_ROW_TINTS: row0 cool blue, row1 amber, row2 pink.
    static let rowTints: [Color] = [
        Color(red: 0xB6 / 255, green: 0xD3 / 255, blue: 0xEF / 255),
        Color(red: 0xED / 255, green: 0xB1 / 255, blue: 0x85 / 255),
        Color(red: 0xF8 / 255, green: 0x88 / 255, blue: 0xA0 / 255),
    ]
    static let dim = 0.1

    /// Opacity keyframe (motion.rs gspin_opacity): full at 0, ease down to dim
    /// by 45%, hold to 92%, rise to full by 100%.
    static func opacity(phase: Double) -> Double {
        let p = phase.truncatingRemainder(dividingBy: 1)
        if p < 0.45 {
            let t = p / 0.45
            return 1 - (1 - dim) * (t * t * (3 - 2 * t))
        }
        if p < 0.92 { return dim }
        let t = (p - 0.92) / 0.08
        return dim + (1 - dim) * t
    }
}

/// 3×3 working indicator for the status strip (cell 2.5, arrow-up wave).
struct WorkingSpinner: View {
    var cellSize: CGFloat = 2.5
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let t = timeline.date.timeIntervalSinceReferenceDate / Motion.gradientSpinPeriod
            grid(time: t)
        }
    }

    private func grid(time: Double) -> some View {
        VStack(spacing: cellSize * 0.8) {
            ForEach(0..<3, id: \.self) { row in
                HStack(spacing: cellSize * 0.8) {
                    ForEach(0..<3, id: \.self) { col in
                        let dx = Double(col - 1)
                        let dy = Double(2 - row)  // distance from bottom-center
                        let dist = (dx * dx + dy * dy).squareRoot() / 2.5
                        Rectangle()
                            .fill(GradientSpin.rowTints[row])
                            .frame(width: cellSize, height: cellSize)
                            .opacity(GradientSpin.opacity(phase: time - dist))
                    }
                }
            }
        }
    }
}

/// Thin session-row activity ring — a SwiftUI port of loaders.rs `arc_spinner`.
/// Reduced motion keeps the partial ring static.
struct ArcSpinner: View {
    var color: Color = Theme.textMuted.opacity(0.8)
    var diameter: CGFloat = 9
    var lineWidth: CGFloat = 1.5
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let phase = reduceMotion
                ? 0
                : (timeline.date.timeIntervalSinceReferenceDate / Motion.arcSpinPeriod)
                    .truncatingRemainder(dividingBy: 1)
            Circle()
                .trim(from: 0.08, to: 0.74)
                .stroke(color, style: StrokeStyle(lineWidth: lineWidth, lineCap: .round))
                .rotationEffect(.degrees(phase * 360))
        }
        .frame(width: diameter, height: diameter)
    }
}

/// comet-pulse loading row: 5 cells, cosine wave, stagger 0.15/2.4
/// (loaders.rs:91).
struct CometPulse: View {
    var cellSize: CGFloat = 6
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        TimelineView(.animation(paused: reduceMotion)) { timeline in
            let t = timeline.date.timeIntervalSinceReferenceDate
            HStack(spacing: cellSize / 2) {
                ForEach(0..<5, id: \.self) { ix in
                    let phase = (t / Motion.cometPulsePeriod - Double(ix) * (0.15 / 2.4))
                        .truncatingRemainder(dividingBy: 1)
                    let wave = (1 - cos(phase * 2 * .pi)) / 2
                    RoundedRectangle(cornerRadius: cellSize * 0.25)
                        .fill(Theme.text)
                        .frame(width: cellSize, height: cellSize)
                        .opacity(0.08 + 0.92 * wave)
                        .scaleEffect(0.9 + 0.1 * wave)
                }
            }
        }
    }
}

// MARK: - Status dot

extension ChatIndicator {
    /// shell/spaces.rs status_dot_color.
    var dotColor: Color {
        switch self {
        case .working: return Theme.statusWorking.opacity(0.85)     // pink-400
        case .awaitingInput: return Theme.accent.opacity(0.9)       // indigo
        case .errored: return Theme.danger
        case .completed: return Theme.statusCompleted.opacity(0.9)  // emerald-400
        case .idle: return whiteAlpha(0.14)
        }
    }
}


/// Harness identity matches the desktop picker: OMP and Prime Agent use the
/// Crew mark; third-party harnesses retain their own brand marks.
struct HarnessBadge: View {
    let harness: String
    var size: CGFloat = 14
    var dimmed = false
    /// Color for marks that carry no brand color of their own (codex, cursor).
    /// Claude keeps its orange regardless.
    var neutral: Color = Theme.text

    var body: some View {
        Group {
            if harness == "omp" || harness == "prime-agent" {
                CrewMark(color: neutral)
            } else {
                BrandMarkShape(mark: BrandMark.forHarness(harness))
                    .fill(BrandMark.brandTint(for: harness) ?? neutral)
            }
        }
        .opacity(dimmed ? 0.6 : 0.9)
        .frame(width: size, height: size)
    }
}
