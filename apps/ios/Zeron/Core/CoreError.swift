import Foundation

extension CoreError: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case let .NotFound(message), let .InvalidArgument(message), let .HostUnavailable(message),
             let .Unsupported(message), let .HostError(message), let .Network(message),
             let .Auth(message), let .Storage(message), let .NotImplemented(message), let .Internal(message):
            return message
        case .Closed:
            return "Crew connection is closed."
        }
    }
}
