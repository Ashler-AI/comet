import { SessionRoom, type LegacyEnv } from "../../legacy/ae2/index.js";

// Test-only scheduling seam; construction, storage schema, WASM loading and the
// alarm handler itself are the immutable deployed implementation, not a copy.
export class RetainedAlarmSession extends SessionRoom {
  constructor(private readonly fixtureState: DurableObjectState, env: LegacyEnv) {
    super(fixtureState, env);
  }

  async fetch(request: Request): Promise<Response> {
    if (new URL(request.url).pathname !== "/__arm") return super.fetch(request);
    this.fixtureState.storage.sql.exec("INSERT INTO meta (key,value) VALUES ('chatId','retained-alarm-fixture'),('backupDirty','1') ON CONFLICT(key) DO UPDATE SET value=excluded.value");
    await this.fixtureState.storage.setAlarm(Date.now() + 1_000);
    return Response.json({ id: this.fixtureState.id.toString() });
  }
}
