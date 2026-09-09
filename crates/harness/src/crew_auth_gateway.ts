// @crew-managed auth-gateway v1
import { lstatSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerGateway } from "../comet-runtime/agent-auth-gateway.ts";

export default async function crewAuthGateway(pi: ExtensionAPI) {
  if (!process.env.COMET_SESSION_ID || !process.env.COMET_INFERENCE_TOKEN) return;

  // User extensions retain precedence, including overrides added after installation.
  try {
    lstatSync(new URL("./omp-auth-gateway.ts", import.meta.url));
    return;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }

  await registerGateway(pi, "COMET_INFERENCE_TOKEN");
}
