// Crew zgx's staging Edge deploy guard (scripts/crew-zgx-staging-guard.mjs):
// the checked-in config deploys only crew-zgx's own Worker, and every way of
// reaching Crew's shared Edges or upstream's account is refused.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { parseJsonc, problems } from "../../scripts/crew-zgx-staging-guard.mjs";

const config = () =>
  parseJsonc(readFileSync(new URL("../../wrangler.crew-zgx-staging.jsonc", import.meta.url), "utf8"));

describe("crew-zgx staging deploy guard", () => {
  it("passes the checked-in config", () => {
    expect(problems(config())).toEqual([]);
  });

  it.each([
    ["the shared staging Edge's name", (c: any) => { c.name = "ashler-comet-edge-staging"; }],
    ["an account of its own", (c: any) => { c.account_id = "upstream"; }],
    ["a route", (c: any) => { c.routes = [{ pattern: "comet-staging.internal.ashler.com/*" }]; }],
    ["another Worker's Durable Objects", (c: any) => { c.durable_objects.bindings[2].script_name = "ashler-comet-edge-staging"; }],
    ["a shared bucket", (c: any) => { c.r2_buckets[0].bucket_name = "comet-staging-blobs"; }],
    ["upstream's migrations", (c: any) => { c.migrations.push({ tag: "v7" }); }],
    ["an environment block", (c: any) => { c.env = {}; }],
    ["a service binding", (c: any) => { c.services = [{ binding: "VAULT", service: "zeron-vault" }]; }],
    ["a cron", (c: any) => { c.triggers.crons = ["0 * * * *"]; }],
    ["dev auth", (c: any) => { c.vars.AUTH_MODE = "dev"; }],
  ])("refuses %s", (_name, change) => {
    const c = config();
    change(c);
    expect(problems(c)).not.toEqual([]);
  });
});
