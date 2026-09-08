import CoreTransferable
import ImageIO
import Observation
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers
import UIKit

// Immutable image bytes and a fully rendered, never-mutated UIKit thumbnail
// cross from background decoding to the main-actor draft.
struct MobileImageAttachment: Identifiable, @unchecked Sendable {
    let id: UUID
    let filename: String
    let bytes: Data
    let preview: UIImage

    // Keep mobile memory bounded and every image below the desktop edge mirror's
    // 32 MiB limit. The desktop's in-memory preview threshold is 24 MiB.
    static let maximumCount = 10
    static let maximumImageBytes = 24 * 1024 * 1024
    static let maximumTotalBytes = 32 * 1024 * 1024

    static func decode(_ data: Data, filename: String) throws -> Self {
        guard data.count <= maximumImageBytes else {
            throw MobileSessionError.unavailable("Choose images smaller than 24 MB each.")
        }
        guard let source = CGImageSourceCreateWithData(data as CFData, nil),
              let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceCreateThumbnailWithTransform: true,
                kCGImageSourceThumbnailMaxPixelSize: 4096,
                kCGImageSourceShouldCacheImmediately: true,
              ] as CFDictionary) else {
            throw MobileSessionError.unavailable("Crew couldn’t open \(filename) as an image.")
        }
        let decoded = UIImage(cgImage: image)
        let alpha = image.alphaInfo
        let transparent = alpha == .first || alpha == .last || alpha == .premultipliedFirst || alpha == .premultipliedLast
        // Normalize HEIC and other picker formats to image types the host reads.
        guard let bytes = transparent ? decoded.pngData() : decoded.jpegData(compressionQuality: 0.9),
              bytes.count <= maximumImageBytes else {
            throw MobileSessionError.unavailable("This image is too large after decoding. Choose a smaller image.")
        }
        let base = (filename as NSString).deletingPathExtension
        let name = String((base.isEmpty ? "image" : base).prefix(100))
        let size = decoded.size
        let scale = min(1, 240 / max(size.width, size.height))
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let preview = UIGraphicsImageRenderer(size: CGSize(width: size.width * scale, height: size.height * scale), format: format).image { _ in
            decoded.draw(in: CGRect(x: 0, y: 0, width: size.width * scale, height: size.height * scale))
        }
        return Self(id: UUID(), filename: name + (transparent ? ".png" : ".jpg"), bytes: bytes, preview: preview)
    }

    static func prompt(_ text: String, paths: [String]) -> String {
        guard !paths.isEmpty else { return text }
        let body = text.isEmpty ? "See the attached image(s)." : text
        return body + "\n\nAttached images (local files — open them to view):" + paths.map { "\n- \($0)" }.joined()
    }
}

@MainActor
@Observable
final class MobileImageDraft {
    var images: [MobileImageAttachment] = []
    var loading = false
    var error: String?
    @ObservationIgnored private var importTask: Task<Void, Never>?

    func cancelImport() {
        importTask?.cancel()
        importTask = nil
        loading = false
    }

    func removeSubmitted(_ submitted: [MobileImageAttachment]) {
        let ids = Set(submitted.map(\.id))
        images.removeAll { ids.contains($0.id) }
    }

    private func append(_ image: MobileImageAttachment) throws {
        guard images.count < MobileImageAttachment.maximumCount else {
            throw MobileSessionError.unavailable("Attach up to 10 images per message.")
        }
        guard images.reduce(image.bytes.count, { $0 + $1.bytes.count }) <= MobileImageAttachment.maximumTotalBytes else {
            throw MobileSessionError.unavailable("Keep attached images under 32 MB per message.")
        }
        images.append(image)
    }

    func importPhotos(_ items: [PhotosPickerItem]) {
        guard !items.isEmpty, !loading else { return }
        loading = true
        error = nil
        importTask = Task { @MainActor in
            defer { if !Task.isCancelled { loading = false; importTask = nil } }
            for item in items {
                do {
                    guard let picked = try await item.loadTransferable(type: PickedMobileImage.self) else {
                        throw MobileSessionError.unavailable("Crew couldn’t load this photo. Download it from iCloud and try again.")
                    }
                    let image = picked.image
                    try Task.checkCancellation()
                    try append(image)
                } catch {
                    guard !Task.isCancelled else { return }
                    self.error = error.localizedDescription
                }
            }
        }
    }

    func importFiles(_ result: Result<[URL], Error>) {
        guard !loading else { return }
        let urls: [URL]
        do { urls = try result.get() } catch {
            if (error as NSError).code != NSUserCancelledError { self.error = error.localizedDescription }
            return
        }
        guard !urls.isEmpty else { return }
        loading = true
        error = nil
        importTask = Task { @MainActor in
            defer { if !Task.isCancelled { loading = false; importTask = nil } }
            for url in urls.prefix(MobileImageAttachment.maximumCount) {
                do {
                    let image = try await Task.detached(priority: .userInitiated) {
                        let scoped = url.startAccessingSecurityScopedResource()
                        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                        return try MobileImageAttachment.readFile(url)
                    }.value
                    try Task.checkCancellation()
                    try append(image)
                } catch {
                    guard !Task.isCancelled else { return }
                    self.error = error.localizedDescription
                }
            }
            if urls.count > MobileImageAttachment.maximumCount {
                error = "Attach up to 10 images per message. Extra files were not added."
            }
        }
    }
}

private struct PickedMobileImage: Transferable {
    let image: MobileImageAttachment

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(importedContentType: .image) { received in
            Self(image: try MobileImageAttachment.readFile(received.file))
        }
    }
}

extension MobileImageAttachment {
    static func readFile(_ url: URL) throws -> Self {
        let file = try FileHandle(forReadingFrom: url)
        defer { try? file.close() }
        let bytes = try file.read(upToCount: maximumImageBytes + 1) ?? Data()
        return try decode(bytes, filename: url.lastPathComponent)
    }
}

struct MobileImagePicker: View {
    @Bindable var draft: MobileImageDraft
    var disabled = false
    @State private var showPhotos = false
    @State private var showFiles = false
    @State private var selection: [PhotosPickerItem] = []

    var body: some View {
        Menu {
            Button("Photo Library", systemImage: "photo.on.rectangle") { showPhotos = true }
            Button("Choose Image Files", systemImage: "folder") { showFiles = true }
        } label: {
            Image(systemName: "paperclip")
                .font(.system(size: 16, weight: .medium))
                .foregroundStyle(Theme.text)
                .frame(width: 36, height: 36)
                .background(whiteAlpha(0.10), in: Circle())
        }
        .accessibilityLabel("Attach images")
        .disabled(disabled || draft.loading || draft.images.count >= MobileImageAttachment.maximumCount)
        .photosPicker(isPresented: $showPhotos, selection: $selection,
                      maxSelectionCount: max(1, MobileImageAttachment.maximumCount - draft.images.count),
                      matching: .images)
        .onChange(of: selection) { _, items in
            guard !items.isEmpty else { return }
            draft.importPhotos(items)
            selection = []
        }
        .fileImporter(isPresented: $showFiles, allowedContentTypes: [.image], allowsMultipleSelection: true) {
            draft.importFiles($0)
        }
    }
}

struct MobileImageDraftView: View {
    @Bindable var draft: MobileImageDraft
    var busy = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !draft.images.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 10) {
                        ForEach(draft.images) { image in
                            ZStack(alignment: .topTrailing) {
                                Image(uiImage: image.preview)
                                    .resizable().scaledToFill()
                                    .frame(width: 72, height: 72).clipped()
                                    .clipShape(RoundedRectangle(cornerRadius: 10))
                                    .accessibilityLabel(image.filename)
                                Button {
                                    draft.images.removeAll { $0.id == image.id }
                                } label: {
                                    Image(systemName: "xmark.circle.fill")
                                        .symbolRenderingMode(.palette)
                                        .foregroundStyle(.white, .black.opacity(0.75))
                                        .font(.system(size: 20))
                                        .padding(4)
                                }
                                .accessibilityLabel("Remove \(image.filename)")
                                .disabled(busy)
                            }
                        }
                    }
                }
            }
            if draft.loading {
                HStack {
                    ProgressView().controlSize(.small)
                    Text("Loading images…")
                    Button("Cancel") { draft.cancelImport() }
                }
            } else if busy, !draft.images.isEmpty {
                HStack {
                    ProgressView().controlSize(.small)
                    Text("Uploading images and sending…")
                }
            }
            if let error = draft.error {
                Text(error).foregroundStyle(Theme.warning)
            }
        }
        .font(Theme.sans(12))
        .foregroundStyle(Theme.textMuted)
        .padding(.horizontal, 20)
        .padding(.vertical, draft.images.isEmpty && !draft.loading && draft.error == nil ? 0 : 8)
    }
}
