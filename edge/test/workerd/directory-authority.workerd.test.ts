import { env } from "cloudflare:test";
import { expect, it } from "vitest";
import { AUTH_USER_HEADER } from "../../src/env";

it("directory lookup never claims a chat and observes the actual chat2 owner", async () => {
  const room = env.CHAT_ROOMS.get(env.CHAT_ROOMS.idFromName(`chat2/${crypto.randomUUID()}`));
  const lookup = (user: string) => room.fetch(new Request("https://chat.internal/directory-authority", {
    headers: { [AUTH_USER_HEADER]: user },
  }));
  expect(await (await lookup("attacker")).json()).toEqual({ ownsSession: false });
  expect((await room.fetch(new Request("https://chat.internal/bind-owner", {
    method: "POST", headers: { [AUTH_USER_HEADER]: "owner" },
  }))).status).toBe(200);
  expect(await (await lookup("owner")).json()).toEqual({ ownsSession: true });
  expect(await (await lookup("attacker")).json()).toEqual({ ownsSession: false });
});
