#!/usr/bin/env node
// Crew zgx's staging Edge deploy guard (docs/crew/zgx-thin-port.md,
// "Staging Edge"): the deploy's token can replace any Worker in Crew's
// account, so before `wrangler deploy` this checks that
// wrangler.crew-zgx-staging.jsonc can only make or update crew-zgx's own
// Worker, with nothing shared with Crew's Edges. Any surprise fails it.
//
//   node scripts/crew-zgx-staging-guard.mjs [config]

import { readFileSync } from "node:fs";

const NAME = "crew-zgx-edge-staging";
const ENVIRONMENT = "crew-zgx-staging";
const BUCKET = "crew-zgx-staging-blobs";
const MIGRATION_PREFIX = "crew-zgx-staging-";
const BINDINGS = {
  AUTH_GRANTS: "AuthGrant",
  NATIVE_GRANT_EPOCHS: "AuthGrant",
  DEVICE_ROOMS: "DeviceRoom",
  PREVIEW_ROOMS: "PreviewRoom",
  REGISTRY_ROOMS: "RegistryRoom",
  CHAT_ROOMS: "ChatRoom",
};
const ALLOWED_KEYS = new Set([
  "$schema", "name", "main", "compatibility_date", "compatibility_flags", "workers_dev",
  "preview_urls", "routes", "triggers", "alias", "rules", "durable_objects", "migrations",
  "r2_buckets", "vars", "observability",
]);

/** JSONC as JSON: comments and trailing commas out, strings kept whole. */
export function parseJsonc(text) {
  let out = "";
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '"') {
      let j = i + 1;
      while (j < text.length && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
      out += text.slice(i, j + 1);
      i = j;
    } else if (c === "/" && text[i + 1] === "/") {
      while (i < text.length && text[i] !== "\n") i++;
      out += "\n";
    } else if (c === "/" && text[i + 1] === "*") {
      i = text.indexOf("*/", i + 2) + 1;
      if (i === 0) throw new Error("unterminated comment");
    } else {
      out += c;
    }
  }
  return JSON.parse(out.replace(/,(\s*[}\]])/g, "$1"));
}

/** What's wrong with `config` (none: it may be deployed). */
export function problems(config) {
  const wrong = [];
  const check = (ok, why) => {
    if (!ok) wrong.push(why);
  };
  for (const key of Object.keys(config)) check(ALLOWED_KEYS.has(key), `unexpected key "${key}"`);
  check(config.name === NAME, `name is ${JSON.stringify(config.name)}, not "${NAME}"`);
  check(!("account_id" in config), "names an account (the deploy's CLOUDFLARE_ACCOUNT_ID is Crew's)");
  check(config.workers_dev === true, "not on workers.dev");
  check(Array.isArray(config.routes) && config.routes.length === 0, "has routes");
  check(!("route" in config), "has a route");
  check(
    config.triggers && Array.isArray(config.triggers.crons) && config.triggers.crons.length === 0,
    "has crons",
  );
  const bindings = config.durable_objects?.bindings ?? [];
  check(bindings.length === Object.keys(BINDINGS).length, "Durable Object bindings changed");
  for (const binding of bindings) {
    check(BINDINGS[binding.name] === binding.class_name, `unexpected Durable Object ${binding.name}`);
    check(!("script_name" in binding), `${binding.name} names another Worker's Durable Objects`);
    check(!("environment" in binding), `${binding.name} names another environment`);
  }
  const migrations = config.migrations ?? [];
  check(migrations.length > 0, "no migrations");
  for (const migration of migrations) {
    check(
      typeof migration.tag === "string" && migration.tag.startsWith(MIGRATION_PREFIX),
      `migration ${JSON.stringify(migration.tag)} isn't crew-zgx staging's`,
    );
  }
  const buckets = config.r2_buckets ?? [];
  check(
    buckets.length === 1 && buckets[0].binding === "BLOBS" && buckets[0].bucket_name === BUCKET,
    `buckets aren't exactly BLOBS → ${BUCKET}`,
  );
  for (const bucket of buckets) check(!("jurisdiction" in bucket), "a bucket names a jurisdiction");
  check(config.vars?.AUTH_MODE === "scaffold", "AUTH_MODE isn't scaffold");
  check(config.vars?.ENVIRONMENT === ENVIRONMENT, `ENVIRONMENT isn't ${ENVIRONMENT}`);
  check(
    typeof config.vars?.SCAFFOLD_CONTROL_PLANE_URL === "string" &&
      config.vars.SCAFFOLD_CONTROL_PLANE_URL.startsWith("https://"),
    "no https Scaffold authority",
  );
  return wrong;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const path = process.argv[2] ?? new URL("../wrangler.crew-zgx-staging.jsonc", import.meta.url).pathname;
  const wrong = problems(parseJsonc(readFileSync(path, "utf8")));
  if (wrong.length) {
    console.error(`crew-zgx staging guard: refusing to deploy ${path}:`);
    for (const why of wrong) console.error(`  - ${why}`);
    process.exit(1);
  }
  console.log(`crew-zgx staging guard: ${path} deploys only ${NAME}`);
}
