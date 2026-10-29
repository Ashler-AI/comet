// The flag makes an extension load failure fatal before OMP can start a turn.
export default function crewGoogleRoute(pi: import("@earendil-works/pi-coding-agent").ExtensionAPI) {
  pi.registerFlag("crew-google-route", { type: "boolean" });
  pi.on("before_provider_request", (event, ctx) => {
    const expected = process.env.OMP_AUTH_GATEWAY_URL?.replace(/\/$/, "");
    const payload = event.payload as { model?: unknown } | undefined;
    if (!expected || ctx.model?.provider !== "google-antigravity" ||
        ctx.model.baseUrl?.replace(/\/$/, "") !== expected ||
        payload?.model !== process.env.OMP_GOOGLE_BOUND_MODEL) {
      // OMP logs hook exceptions and keeps going, so throwing is insufficient.
      process.stderr.write("Crew refused an inference request outside its managed Google route\n");
      process.exit(1);
    }
  });
}
