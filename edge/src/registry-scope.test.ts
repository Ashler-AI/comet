import { expect, it } from "vitest";
import { authorizeRoute } from "./authority";
import type { Verified } from "./auth";

it("only opens typed registry transports for an epoch-bound Scaffold writer", () => {
  const host: Verified = {
    credential: "device", userId: "owner", email: "owner", orgId: "canary", projectScope: "canary",
    deploymentId: "canary", sessionId: "one", sandboxId: "box", lifecycleEpoch: 2,
    targetDeviceId: "comet-scaffold-box-e2", capabilities: ["session.read", "session.sync.write"],
  };
  for (const [path, method] of [["rows", "GET"], ["ws", "GET"], ["push", "POST"]]) {
    const request = new Request(`https://edge/registry/canary/${path}`, { method });
    expect(authorizeRoute(request, host)).toBe(true);
    for (const changed of [
      { capabilities: ["session.read"] }, { sandboxId: undefined }, { lifecycleEpoch: undefined },
      { targetDeviceId: "desktop" }, { lifecycleEpoch: 3 }, { sessionId: undefined },
    ]) expect(authorizeRoute(request, { ...host, ...changed })).toBe(false);
  }
  for (const [path, method] of [["stats", "GET"], ["reset", "POST"], ["push-target", "POST"], ["device-presence", "POST"], ["peer-target", "GET"], ["rows/extra", "GET"]]) {
    expect(authorizeRoute(new Request(`https://edge/registry/canary/${path}`, { method }), host)).toBe(false);
  }
  expect(authorizeRoute(new Request("https://edge/registry/canary/ws?device=desktop"), host)).toBe(false);
  expect(authorizeRoute(new Request("https://edge/registry/other/rows"), host)).toBe(false);
});
