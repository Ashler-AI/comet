// Forward deployment ONLY: retain the complete native-v2 migration/binding set.
// Direct Cloudflare version rollback to v2 is invalid after adding DO classes.
export { default, AuthGrant, DeviceRoom, SessionRoom, sessionRoomKey } from "../legacy/ae2/index.js";
export {
  NativeAuthGrant, NativeDeviceRoom, NativePreviewRoom, NativeRegistryRoom, NativeChatRoom
} from "./staging-native";
