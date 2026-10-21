import legacy from "../legacy/ae2/index.js";
import native from "./index";
import { activeNativeEnv, type StagingEnv } from "./staging-native";

// Original exports must instantiate the original deployed code with RAW bindings.
export { AuthGrant, DeviceRoom, SessionRoom, sessionRoomKey } from "../legacy/ae2/index.js";
export {
  NativeAuthGrant, NativeDeviceRoom, NativePreviewRoom, NativeRegistryRoom, NativeChatRoom
} from "./staging-native";

export default {
  async fetch(request: Request, raw: StagingEnv): Promise<Response> {
    const parts = new URL(request.url).pathname.split("/").filter(Boolean);
    if (parts[0] === "api" && parts[1] === "releases") {
      // Exact ae2 route ownership, not an error fallback. That handler authenticates
      // with raw legacy scope/grants, then only reads GCS with read_only OAuth scope.
      if (parts.length === 3 && /^[A-Za-z0-9._-]{1,200}$/.test(parts[2]) &&
          (request.method === "GET" || request.method === "HEAD")) {
        return legacy.fetch(request, raw);
      }
      return Response.json({ error: "not_found" }, { status: 404 });
    }
    // /releases is canary-only R2, not an alias for the established GCS feed.
    if (parts[0] === "releases") return Response.json({ error: "not_found" }, { status: 404 });
    return native.fetch(request, activeNativeEnv(raw));
  }
} satisfies ExportedHandler<StagingEnv>;
