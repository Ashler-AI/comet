import test from 'node:test';
import assert from 'node:assert/strict';
import { inventory, inventoryRecord, applyPlan, canonical, digest, validatePlan, authenticatedApi, tokenFromAuthFile, CONTROL } from './migrate-staging-cloud.mjs';

function session(id = 'sandbox-a') {
  return {
    id, status: 'ready', kind: 'remote_code', runtimeProfile: 'comet_remote', lifecycleEpoch: 7,
    ownerEmail: 'owner@example.test', agentModel: 'preserved-model', sourceRef: 'preserved-worktree',
    cometRuntimeProfile: { version: 'scaffold.comet-runtime.v1', projectId: 'ashler-staging', deploymentId: 'ashler-staging',
      sandboxId: id, sessionId: `logical-${id}`, targetDeviceId: `comet-scaffold-${id}-e7`, lifecycleEpoch: 7, actor: { sub: 'owner@example.test' } },
  };
}
function authority(records, { failPrepare = false, status = 'prepared' } = {}) {
  const calls = [];
  const api = async (path, options = {}) => {
    calls.push([path, options]);
    const url = new URL(path, CONTROL);
    if (url.pathname === '/api/sessions') {
      assert.equal(url.searchParams.get('showStopped'), 'true');
      assert.equal(url.searchParams.has('live'), false);
      assert.equal(url.searchParams.has('reconcile'), false);
      const offset = Number(url.searchParams.get('offset'));
      return { sessions: records.slice(offset, offset + 1), pageInfo: { hasMore: offset + 1 < records.length, nextOffset: offset + 1 } };
    }
    const parts = url.pathname.split('/');
    const record = records.find(item => item.id === decodeURIComponent(parts[3]));
    assert.ok(record);
    if (parts[4] === 'crew-migration') {
      if (!parts[5]) return { status, scope: inventoryRecord(record).scope, coordinatorLifecycleEpoch: record.lifecycleEpoch, inventoryRecord: structuredClone(record) };
      assert.deepEqual(options.body.expected.scope, inventoryRecord(record).scope);
      assert.equal(options.body.expected.coordinatorLifecycleEpoch, record.lifecycleEpoch);
      assert.equal(options.body.expected.sourceGeneration, 'session');
      if (parts[5] === 'prepare' && failPrepare) throw new Error('connection_lost');
      if (parts[5] === 'commit') record.crewSyncGeneration = 'chat2';
      return { status: parts[5] === 'commit' ? 'committed' : 'prepared' };
    }
    assert.fail('operator must not use the normalizing session detail route');
  };
  return { api, calls };
}

test('inventory includes stopped records, exact source authority, and deterministic content digests', async () => {
  const rows = [session('b'), { ...session('a'), status: 'stopped' }];
  const { api, calls } = authority(rows);
  const plan = await inventory(api);
  assert.deepEqual(plan.records.map(record => record.id), ['a', 'b']);
  assert.equal(plan.records[0].sourceGeneration, 'session');
  assert.equal(plan.records[0].scope.lifecycleEpoch, 7);
  assert.equal(plan.counts['requires-fenced-import'], 1);
  assert.equal(plan.counts.blocked, 1);
  assert.deepEqual(plan.records[0].blockers, ['terminal_epoch_authority_incomplete']);
  assert.throws(() => validatePlan(plan), /unresolved/);
  assert.equal(canonical(await inventory(api)), canonical(plan));
  assert.ok(calls.every(([, options]) => !options.method));
  assert.equal(inventoryRecord({ ...session(), ownerEmail: undefined }).disposition, 'blocked');
  assert.equal(inventoryRecord({ ...session(), crewSyncGeneration: 'unknown' }).disposition, 'blocked');
  assert.equal(inventoryRecord({ ...session(), lifecycleEpoch: undefined }).disposition, 'blocked');
  const blocked = await inventory(authority([{ ...session(), ownerEmail: undefined }]).api);
  assert.throws(() => validatePlan(blocked), /unresolved/);
  assert.throws(() => validatePlan({ ...plan, controlOrigin: 'https://production.example' }), /invalid_staging/);
});

test('all source imports prepare before any CAS, and IDs/epoch/model survive', async () => {
  const records = [session('a'), session('b')];
  const { api, calls } = authority(records);
  const plan = await inventory(api);
  const snapshots = [];
  let drainReads = 0;
  const result = await applyPlan(plan, { api, beforeMutation: async () => { drainReads++; }, persist: async value => snapshots.push(structuredClone(value)) });
  assert.equal(drainReads, 4);
  assert.equal(result.complete, true);
  assert.deepEqual(calls.filter(([, options]) => options.method).map(([path]) => path.split('/').at(-1)), ['prepare', 'prepare', 'commit', 'commit']);
  assert.ok(snapshots.some(value => value.operations.at(-1)?.outcome === 'unknown'));
  assert.ok(records.every(record => record.crewSyncGeneration === 'chat2' && record.lifecycleEpoch === 7 && record.agentModel === 'preserved-model'));
});

test('unknown mutation response never retries, commits, or silently unlocks', async () => {
  const { api, calls } = authority([session()], { failPrepare: true });
  const plan = await inventory(api);
  const result = await applyPlan(plan, { api, beforeMutation: async () => {} });
  assert.equal(result.complete, false);
  assert.equal(result.operations[0].outcome, 'unknown');
  assert.deepEqual(calls.filter(([, options]) => options.method).map(([path]) => path.split('/').at(-1)), ['prepare']);
});

test('changed inventory and unknown recovery claim are hard gates', async () => {
  const records = [session()];
  const { api, calls } = authority(records, { status: 'unknown' });
  const plan = await inventory(api);
  records[0].agentModel = 'changed';
  await assert.rejects(applyPlan(plan, { api, beforeMutation: async () => {} }), /inventory_changed/);
  records[0].agentModel = 'preserved-model';
  await assert.rejects(applyPlan(plan, { api, beforeMutation: async () => {}, recover: true }), /unknown_migration_claim_state/);
  assert.equal(calls.filter(([, options]) => options.method).length, 0);
});

test('pagination cannot omit duplicates, stalled pages, or an unknown logical owner', async () => {
  await assert.rejects(inventory(async () => ({ sessions: [session(), session()], pageInfo: { hasMore: false } })), /duplicate/);
  await assert.rejects(inventory(async () => ({ sessions: [], pageInfo: { hasMore: true, nextOffset: 0 } })), /incomplete/);
  assert.equal(digest({ b: 2, a: 1 }), digest({ a: 1, b: 2 }));
});

test('credentials are sent only to fixed staging with redirects disabled', async () => {
  const api = authenticatedApi('private-token', async (url, options) => {
    assert.equal(url, `${CONTROL}/api/sessions`);
    assert.equal(options.redirect, 'error');
    assert.equal(options.headers.authorization, 'Bearer private-token');
    return { ok: true, json: async () => ({ sessions: [] }) };
  });
  await api('/api/sessions');
  await assert.rejects(api('https://production.example/api/sessions'), /invalid_control_path/);
});

test('inventory rejects membership drift while authority details are read', async () => {
  let lists = 0;
  await assert.rejects(inventory(async path => {
    if (path.startsWith('/api/sessions?')) return { sessions: lists++ ? [] : [session()], pageInfo: { hasMore: false } };
    return { inventoryRecord: session() };
  }), /inventory_changed_during_read/);
});

test('generation-swapped but fenced activation recovers without a new preparation claim', async () => {
  const records = [session()];
  const { api, calls } = authority(records, { status: 'activating' });
  const plan = await inventory(api);
  records[0].crewSyncGeneration = 'chat2';
  const report = await applyPlan(plan, { api, beforeMutation: async () => {}, recover: true });
  assert.equal(report.complete, true);
  assert.deepEqual(calls.filter(([, options]) => options.method).map(([path]) => path.split('/').at(-1)), ['recover', 'commit']);
});

test('explicit recovery prepares untouched records through the same serialized claim API', async () => {
  const { api, calls } = authority([session()], { status: 'unprepared' });
  const plan = await inventory(api);
  assert.equal((await applyPlan(plan, { api, beforeMutation: async () => {}, recover: true })).complete, true);
  assert.deepEqual(calls.filter(([, options]) => options.method).map(([path]) => path.split('/').at(-1)), ['prepare', 'commit']);
});

test('missing or stale live deployment evidence cannot dispatch a migration', async () => {
  const { api, calls } = authority([session()]);
  const plan = await inventory(api);
  await assert.rejects(applyPlan(plan, { api }), /live_compatibility_verification_required/);
  const report = await applyPlan(plan, { api, beforeMutation: async () => { throw new Error('live_compatibility_verification_failed'); } });
  assert.equal(report.complete, false);
  assert.equal(report.operations[0].outcome, 'blocked');
  assert.equal(calls.filter(([, options]) => options.method).length, 0);
});

test('preparation-only stops before generation CAS or activation', async () => {
  const rows = [session('a'), session('b')];
  const { api, calls } = authority(rows);
  const report = await applyPlan(await inventory(api), { api, beforeMutation: async () => {}, prepareOnly: true });
  assert.equal(report.prepared, true);
  assert.equal(report.complete, false);
  assert.deepEqual(calls.filter(([, options]) => options.method).map(([path]) => path.split('/').at(-1)), ['prepare', 'prepare']);
  assert.ok(rows.every(row => row.crewSyncGeneration === undefined));
});

test('a self-rehashed plan cannot relabel legacy records as already native or out of scope', async () => {
  const plan = await inventory(authority([session()]).api);
  for (const disposition of ['native', 'not-crew']) {
    const forged = structuredClone(plan);
    forged.records[0].disposition = disposition;
    forged.inventorySha256 = digest(forged.records);
    assert.throws(() => validatePlan(forged), /invalid_inventory_disposition/);
  }
});

test('existing user auth files stay scoped to staging and never require token arguments', () => {
  assert.equal(tokenFromAuthFile(JSON.stringify({ accessToken: 'private-token', resource: CONTROL })), 'private-token');
  assert.equal(tokenFromAuthFile(JSON.stringify({ access_token: 'private-token' })), 'private-token');
  assert.equal(tokenFromAuthFile('private-token\n'), 'private-token');
  assert.throws(() => tokenFromAuthFile(JSON.stringify({ accessToken: 'private-token', resource: 'https://production.example' })), /auth_file_not_staging/);
});

test('explicit Crew runtime cannot disappear behind generic agent kind', () => {
  const record = session();
  record.kind = 'agent';
  delete record.cometRuntimeProfile;
  const result = inventoryRecord(record);
  assert.equal(result.disposition, 'blocked');
  assert.ok(result.blockers.includes('missing_runtime_authority'));
});

test('existing service bridge is fixed loopback and HTTP failures name the safe phase', async () => {
  const seen = [];
  const api = authenticatedApi('private-token', async (url, options) => {
    seen.push(url);
    assert.equal(options.redirect, 'error');
    return { ok: false, status: 401 };
  }, true);
  await assert.rejects(api('/api/sessions?showStopped=true'), /control_session_inventory_http_401/);
  await assert.rejects(api('/api/code-sandboxes/auth/session'), /control_owner_auth_http_401/);
  assert.ok(seen.every(url => url.startsWith('http://127.0.0.1:18081/')));
  await assert.rejects(api('https://production.example/api/sessions'), /invalid_control_path/);
});

test('duplicate logical histories retain every physical record and exact epoch as blockers', async () => {
  const current = session('current');
  const historical = session('historical');
  historical.status = 'stopped';
  historical.lifecycleEpoch = 2;
  historical.cometRuntimeProfile.sessionId = current.cometRuntimeProfile.sessionId;
  historical.cometRuntimeProfile.lifecycleEpoch = 2;
  historical.cometRuntimeProfile.targetDeviceId = 'comet-scaffold-historical-e2';
  const plan = await inventory(authority([current, historical]).api);
  assert.deepEqual(plan.records.map(row => row.id), ['current', 'historical']);
  assert.equal(plan.counts.blocked, 2);
  assert.deepEqual(plan.statusCounts, { ready: 1, stopped: 1 });
  assert.equal(plan.duplicateLogicalGroups.length, 1);
  assert.deepEqual(plan.duplicateLogicalGroups[0].records.map(row => [row.id, row.scope.lifecycleEpoch]), [['current', 7], ['historical', 2]]);
  assert.ok(plan.records.every(row => row.blockers.includes('duplicate_logical_session_authority')));
  assert.throws(() => validatePlan(plan), /unresolved/);
});

test('terminal metadata is only a candidate and commit requires an independently verified controller selector', async () => {
  const row = { ...session(), status: 'stopped', providerAction: 'kill', terminatedAt: '2026-10-01T00:00:00.000Z', terminationReason: 'timeout', snapshotId: 'base-template' };
  const { api, calls } = authority([row]);
  const plan = await inventory(api);
  assert.equal(plan.records[0].sourceDisposition, 'terminal-original-epoch-candidate');
  assert.equal(plan.records[0].disposition, 'requires-fenced-import');
  await assert.rejects(applyPlan(plan, { api, beforeMutation: async () => {} }), /terminal_destination_controller_required/);
  assert.equal((await applyPlan(plan, { api, beforeMutation: async () => {}, prepareOnly: true })).prepared, true);
  const report = await applyPlan(plan, { api, beforeMutation: async () => {}, recover: true, destinationDeviceId: 'owner-controller' });
  assert.equal(report.complete, true);
  const commit = calls.find(([path]) => path.endsWith('/crew-migration/commit'));
  assert.equal(commit[1].body.destinationDeviceId, 'owner-controller');
  assert.equal(commit[1].body.expected.scope.deviceId, 'comet-scaffold-sandbox-a-e7');
});

test('original writer scope survives coordinator revocation and missing or stale fences fail closed', async () => {
  const row = { ...session(), status: 'stopped', lifecycleEpoch: 2 };
  row.cometRuntimeProfile.lifecycleEpoch = 1;
  row.cometRuntimeProfile.targetDeviceId = 'comet-scaffold-sandbox-a';
  const original = structuredClone(row);
  const blocked = inventoryRecord(row);
  assert.equal(blocked.scope.lifecycleEpoch, 1);
  assert.equal(blocked.scope.deviceId, original.cometRuntimeProfile.targetDeviceId);
  assert.equal(blocked.coordinatorLifecycleEpoch, 2);
  assert.deepEqual(blocked.blockers, ['terminal_epoch_authority_incomplete']);
  assert.deepEqual(row, original);
  const candidate = { ...row, providerAction: 'kill', terminatedAt: '2026-10-01T00:00:00.000Z', terminationReason: 'timeout' };
  const { api, calls } = authority([candidate]);
  const plan = await inventory(api);
  const prepared = await applyPlan(plan, { api, beforeMutation: async () => {}, prepareOnly: true });
  assert.equal(prepared.prepared, true);
  const expected = calls.find(([path]) => path.endsWith('/crew-migration/prepare'))[1].body.expected;
  assert.equal(expected.scope.lifecycleEpoch, 1);
  assert.equal(expected.scope.deviceId, original.cometRuntimeProfile.targetDeviceId);
  assert.equal(expected.coordinatorLifecycleEpoch, 2);
  for (const fence of [undefined, 0]) {
    const invalid = structuredClone(plan);
    invalid.records[0].coordinatorLifecycleEpoch = fence;
    invalid.inventorySha256 = digest(invalid.records);
    assert.throws(() => validatePlan(invalid), /invalid_inventory_disposition/);
  }
  const stale = (path, options) => path.endsWith('/crew-migration')
    ? Promise.resolve({ status: 'prepared', scope: plan.records[0].scope, coordinatorLifecycleEpoch: 1, inventoryRecord: structuredClone(candidate) })
    : api(path, options);
  const mutations = calls.filter(([, options]) => options.method).length;
  await assert.rejects(applyPlan(plan, { api: stale, beforeMutation: async () => {}, recover: true, prepareOnly: true }), /migration_scope_changed/);
  assert.equal(calls.filter(([, options]) => options.method).length, mutations);
  assert.equal(inventoryRecord({ ...row, cometRuntimeProfile: { ...row.cometRuntimeProfile, lifecycleEpoch: undefined } }).disposition, 'blocked');
  assert.equal(inventoryRecord({ ...row, cometRuntimeProfile: { ...row.cometRuntimeProfile, targetDeviceId: undefined } }).disposition, 'blocked');
  for (const targetDeviceId of ['unrelated-device', 'comet-scaffold-another-sandbox', 'comet-scaffold-sandbox-a-e2']) {
    const invalid = inventoryRecord({ ...row, cometRuntimeProfile: { ...row.cometRuntimeProfile, targetDeviceId } });
    assert.ok(invalid.blockers.includes('invalid_original_device_binding'));
  }
  assert.equal(inventoryRecord({ ...row, lifecycleEpoch: 0 }).disposition, 'blocked');
  assert.equal(inventoryRecord({ ...row, lifecycleEpoch: 1, cometRuntimeProfile: { ...row.cometRuntimeProfile, lifecycleEpoch: 2 } }).disposition, 'blocked');
});

test('inventory uses only selected migration authority and preserves unrecognized profile diagnostics', async () => {
  const record = session();
  record.cometRuntimeProfile.version = 'retained-unrecognized-version';
  record.runtimeProfile = 'remote_code';
  const { api, calls } = authority([record]);
  const plan = await inventory(api);
  assert.equal(plan.records[0].reportedAuthority.profileVersion, 'retained-unrecognized-version');
  assert.equal(plan.records[0].reportedAuthority.runtimeProfile, 'remote_code');
  assert.equal(plan.records[0].reportedAuthority.profileVersionPresent, true);
  const missingVersion = structuredClone(record);
  delete missingVersion.cometRuntimeProfile.version;
  const nullVersion = structuredClone(record);
  nullVersion.cometRuntimeProfile.version = null;
  assert.equal(inventoryRecord(missingVersion).reportedAuthority.profileVersionPresent, false);
  assert.equal(inventoryRecord(nullVersion).reportedAuthority.profileVersionPresent, true);
  assert.equal(inventoryRecord(nullVersion).reportedAuthority.profileVersion, null);
  assert.ok(plan.records[0].blockers.includes('missing_runtime_authority'));
  assert.ok(calls.filter(([path]) => !path.startsWith('/api/sessions?')).every(([path]) => path.endsWith('/crew-migration')));
  for (const selected of [undefined, { ...record, id: 'another-record' }]) {
    await assert.rejects(inventory(async path => path.startsWith('/api/sessions?')
      ? { sessions: [record], pageInfo: { hasMore: false } } : { inventoryRecord: selected }), /authority_record_identity_changed/);
  }
});
