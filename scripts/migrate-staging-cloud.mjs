#!/usr/bin/env node
// Staging only. Inventory never asks the provider to probe, resume, or reconcile.
import { createHash } from 'node:crypto';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { readFile, writeFile, rename } from 'node:fs/promises';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

export const CONTROL = 'https://scaffold-staging.internal.ashler.com';
export const SCOPE = 'ashler-staging';
export function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().filter(key => value[key] !== undefined).map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}
export const digest = value => createHash('sha256').update(canonical(value)).digest('hex');
const nonempty = value => typeof value === 'string' && value.trim().length > 0;

export function inventoryRecord(record) {
  if (!record || !nonempty(record.id)) throw new Error('invalid_session_record');
  const profile = record.cometRuntimeProfile;
  const result = { id: record.id, status: record.status ?? null, recordSha256: digest(record), blockers: [] };
  result.reportedAuthority = {
    runtimeProfile: record.runtimeProfile ?? null, kind: record.kind ?? null, profileVersion: profile?.version ?? null,
    runtimeProfilePresent: Object.hasOwn(record, 'runtimeProfile'),
    profileVersionPresent: profile != null && Object.hasOwn(profile, 'version'),
    runtimeProfileValueType: record.runtimeProfileValueType ?? null,
    profileVersionValueType: profile?.versionValueType ?? null,
    ownerSubject: record.ownerEmail ?? null, profileOwnerSubject: profile?.actor?.sub ?? null,
    projectId: profile?.projectId ?? null, deploymentId: profile?.deploymentId ?? null,
    logicalSessionId: profile?.sessionId ?? null, sandboxId: profile?.sandboxId ?? null,
    deviceId: profile?.targetDeviceId ?? null, lifecycleEpoch: record.lifecycleEpoch ?? null,
    profileLifecycleEpoch: profile?.lifecycleEpoch ?? null,
  };
  result.lifecycleEvidence = {
    providerAction: record.providerAction ?? null, terminatedAt: record.terminatedAt ?? null,
    terminationReason: record.terminationReason ?? null, baseTemplateSnapshotId: record.snapshotId ?? null,
  };
  const preserved = { ...record };
  for (const key of ['crewSyncGeneration', 'updatedAt', 'lastActivityAt']) delete preserved[key];
  result.preservationSha256 = digest(preserved);
  if (!profile && record.runtimeProfile !== 'comet_remote' && (record.kind === 'agent' || ['agent_opencode', 'remote_code'].includes(record.runtimeProfile))) {
    return { ...result, disposition: 'not-crew' };
  }
  result.sourceGeneration = record.crewSyncGeneration ?? 'session'; // Existing pre-cutover server contract.
  if (!['session', 'chat2'].includes(result.sourceGeneration)) result.blockers.push('unknown_source_generation');
  if (!profile || profile.version !== 'scaffold.comet-runtime.v1') result.blockers.push('missing_runtime_authority');
  if (!nonempty(record.ownerEmail) || profile?.actor?.sub !== record.ownerEmail) result.blockers.push('unknown_source_owner');
  if (profile?.projectId !== SCOPE || profile?.deploymentId !== SCOPE) result.blockers.push('non_staging_or_unknown_scope');
  if (!nonempty(profile?.sessionId) || profile?.sandboxId !== record.id) result.blockers.push('invalid_session_binding');
  if (!Number.isSafeInteger(record.lifecycleEpoch) || record.lifecycleEpoch < 1) result.blockers.push('unknown_lifecycle_epoch');
  if (!Number.isSafeInteger(profile?.lifecycleEpoch) || profile.lifecycleEpoch < 1) result.blockers.push('unknown_original_lifecycle_epoch');
  if (!nonempty(profile?.targetDeviceId)) result.blockers.push('unknown_original_device');
  else if (profile.targetDeviceId !== `comet-scaffold-${record.id}`
      && profile.targetDeviceId !== `comet-scaffold-${record.id}-e${profile.lifecycleEpoch}`) result.blockers.push('invalid_original_device_binding');
  if (Number.isSafeInteger(profile?.lifecycleEpoch) && Number.isSafeInteger(record.lifecycleEpoch)
      && profile.lifecycleEpoch > record.lifecycleEpoch) result.blockers.push('original_epoch_exceeds_coordinator');
  if (record.runtimeProfile !== 'comet_remote') result.blockers.push('unknown_runtime_profile');
  if (result.blockers.length) return { ...result, disposition: 'blocked' };
  result.coordinatorLifecycleEpoch = record.lifecycleEpoch;
  result.scope = {
    ownerSubject: record.ownerEmail, projectId: profile.projectId, deploymentId: profile.deploymentId,
    logicalSessionId: profile.sessionId, sandboxId: record.id,
    deviceId: profile.targetDeviceId, lifecycleEpoch: profile.lifecycleEpoch,
  };
  const terminalCandidate = ['stopped', 'failed'].includes(record.status) && record.providerAction === 'kill'
    && typeof record.terminatedAt === 'string' && Number.isFinite(Date.parse(record.terminatedAt))
    && ['timeout', 'provider_missing'].includes(record.terminationReason);
  if (result.sourceGeneration === 'session' && terminalCandidate) {
    result.sourceDisposition = 'terminal-original-epoch-candidate';
  } else if (result.sourceGeneration === 'session' && !['ready', 'agent_running'].includes(record.status)) {
    result.blockers.push(record.status === 'paused' ? 'offline_source_access_required'
      : ['stopped', 'failed'].includes(record.status) ? 'terminal_epoch_authority_incomplete' : 'source_lifecycle_not_quiescent');
    return { ...result, disposition: 'blocked' };
  }
  result.disposition = result.sourceGeneration === 'chat2' ? 'native' : 'requires-fenced-import';
  return result;
}

async function inventoryIds(api) {
  const ids = new Set();
  let offset = 0;
  while (true) {
    const page = await api(`/api/sessions?showStopped=true&limit=500&offset=${offset}`);
    if (!Array.isArray(page.sessions) || typeof page.pageInfo?.hasMore !== 'boolean') throw new Error('invalid_inventory_page');
    for (const record of page.sessions) {
      if (!nonempty(record.id) || ids.has(record.id)) throw new Error('duplicate_or_invalid_inventory_identity');
      ids.add(record.id);
    }
    if (!page.pageInfo.hasMore) break;
    const next = page.pageInfo.nextOffset;
    if (!Number.isSafeInteger(next) || next <= offset || next > 100_000) throw new Error('incomplete_inventory_pagination');
    offset = next;
  }
  return [...ids].sort();
}

export async function inventory(api) {
  const ids = await inventoryIds(api);
  const records = [];
  for (const id of ids) {
    const { inventoryRecord: record } = await api(`/api/sessions/${encodeURIComponent(id)}/crew-migration`);
    if (record?.id !== id) throw new Error('authority_record_identity_changed');
    const entry = inventoryRecord(record);
    if (api.ownerSubject && record.ownerEmail !== api.ownerSubject) {
      entry.blockers.push('owner_scope_mismatch');
      entry.disposition = 'blocked';
    }
    records.push(entry);
  }
  if (canonical(await inventoryIds(api)) !== canonical(ids)) throw new Error('inventory_changed_during_read');
  const logicalIds = new Map();
  for (const record of records) {
    if (!record.scope) continue;
    const key = canonical([record.scope.ownerSubject, record.scope.projectId, record.scope.deploymentId, record.scope.logicalSessionId]);
    const group = logicalIds.get(key) ?? [];
    group.push(record);
    logicalIds.set(key, group);
  }
  const duplicateLogicalGroups = [];
  for (const group of logicalIds.values()) {
    if (group.length < 2) continue;
    for (const record of group) {
      record.blockers.push('duplicate_logical_session_authority');
      record.disposition = 'blocked';
    }
    duplicateLogicalGroups.push({
      ownerSubject: group[0].scope.ownerSubject, projectId: group[0].scope.projectId,
      deploymentId: group[0].scope.deploymentId, logicalSessionId: group[0].scope.logicalSessionId,
      records: group.map(({ id, status, scope, coordinatorLifecycleEpoch, sourceGeneration }) => ({ id, status, scope, coordinatorLifecycleEpoch, sourceGeneration })),
    });
  }
  return {
    version: 1, controlOrigin: CONTROL, projectId: SCOPE, deploymentId: SCOPE, ownerSubject: api.ownerSubject,
    inventorySha256: digest(records), records, duplicateLogicalGroups,
    counts: Object.fromEntries(['not-crew', 'native', 'blocked', 'requires-fenced-import'].map(kind => [kind, records.filter(record => record.disposition === kind).length])),
    statusCounts: Object.fromEntries([...new Set(records.map(record => record.status ?? 'unknown'))].sort()
      .map(status => [status, records.filter(record => (record.status ?? 'unknown') === status).length])),
  };
}

export function validatePlan(plan) {
  if (plan?.version !== 1 || plan.controlOrigin !== CONTROL || plan.projectId !== SCOPE || plan.deploymentId !== SCOPE || !Array.isArray(plan.records)
      || plan.inventorySha256 !== digest(plan.records) || new Set(plan.records.map(record => record.id)).size !== plan.records.length) {
    throw new Error('invalid_staging_plan');
  }
  if (plan.records.some(record => record.disposition === 'blocked')) throw new Error('inventory_contains_unresolved_records');
  for (const record of plan.records) {
    if (!nonempty(record.id) || !/^[a-f0-9]{64}$/.test(record.recordSha256) || !/^[a-f0-9]{64}$/.test(record.preservationSha256)
        || !Array.isArray(record.blockers) || record.blockers.length) throw new Error('invalid_inventory_record');
    if (record.disposition === 'not-crew') {
      if (record.scope !== undefined || record.coordinatorLifecycleEpoch !== undefined || record.sourceGeneration !== undefined) throw new Error('invalid_inventory_disposition');
      continue;
    }
    const scope = record.scope;
    if (!scope || scope.projectId !== SCOPE || scope.deploymentId !== SCOPE || scope.sandboxId !== record.id
        || !nonempty(scope.ownerSubject) || !nonempty(scope.logicalSessionId) || !Number.isSafeInteger(scope.lifecycleEpoch) || scope.lifecycleEpoch < 1
        || (scope.deviceId !== `comet-scaffold-${record.id}` && scope.deviceId !== `comet-scaffold-${record.id}-e${scope.lifecycleEpoch}`)
        || !Number.isSafeInteger(record.coordinatorLifecycleEpoch)
        || record.coordinatorLifecycleEpoch < scope.lifecycleEpoch
        || !((record.disposition === 'native' && record.sourceGeneration === 'chat2')
          || (record.disposition === 'requires-fenced-import' && record.sourceGeneration === 'session'))) throw new Error('invalid_inventory_disposition');
  }
}

function compareRecovery(plan, current) {
  if (plan.records.length !== current.records.length) throw new Error('inventory_changed');
  for (let index = 0; index < plan.records.length; index++) {
    const before = plan.records[index], after = current.records[index];
    if (before.id !== after.id || before.preservationSha256 !== after.preservationSha256 || canonical(before.scope) !== canonical(after.scope)
        || before.coordinatorLifecycleEpoch !== after.coordinatorLifecycleEpoch
        || (before.sourceGeneration !== after.sourceGeneration && !(before.sourceGeneration === 'session' && after.sourceGeneration === 'chat2'))) {
      throw new Error('source_identity_or_preserved_state_changed');
    }
  }
}

// No mutation is retried automatically. A lost response is unknown, not failure.
export async function applyPlan(plan, { api, beforeMutation, recover = false, prepareOnly = false, destinationDeviceId, persist = async () => {} }) {
  if (typeof beforeMutation !== 'function') throw new Error('live_compatibility_verification_required');
  validatePlan(plan);
  if (!prepareOnly && plan.records.some(record => record.sourceDisposition === 'terminal-original-epoch-candidate') && !nonempty(destinationDeviceId)) throw new Error('terminal_destination_controller_required');
  const current = await inventory(api);
  validatePlan(current);
  if (plan.ownerSubject !== current.ownerSubject) throw new Error('plan_owner_changed');
  if (recover) compareRecovery(plan, current);
  else if (current.inventorySha256 !== plan.inventorySha256) throw new Error('inventory_changed_run_new_dry_run');
  const report = { version: 1, mode: recover ? 'recover' : 'apply', planSha256: plan.inventorySha256, prepared: false, complete: false, operations: [] };
  await persist(report);
  const targets = plan.records.filter(record => record.disposition === 'requires-fenced-import');
  const prepared = [];
  async function action(record, operation) {
    const entry = { id: record.id, operation, outcome: 'unknown' };
    report.operations.push(entry);
    await persist(report); // Intent survives a process crash before/after the request.
    let dispatched = false;
    try {
      await beforeMutation();
      dispatched = true;
      await api(`/api/sessions/${encodeURIComponent(record.id)}/crew-migration/${operation}`, {
        method: 'POST', body: {
          expected: { scope: record.scope, coordinatorLifecycleEpoch: record.coordinatorLifecycleEpoch, sourceGeneration: 'session' },
          ...(operation === 'commit' && destinationDeviceId ? { destinationDeviceId } : {}),
        },
      });
      entry.outcome = 'acknowledged';
      await persist(report);
    } catch (error) {
      if (!dispatched) entry.outcome = 'blocked';
      entry.error = /^[a-zA-Z0-9_-]{1,100}$/.test(error.message) ? error.message : 'request_outcome_unknown';
      await persist(report);
      return false;
    }
    return true;
  }
  for (const record of targets) {
    let operation = recover ? 'recover' : 'prepare';
    if (recover) {
      const status = await api(`/api/sessions/${encodeURIComponent(record.id)}/crew-migration`);
      if (canonical(status.scope) !== canonical(record.scope) || status.coordinatorLifecycleEpoch !== record.coordinatorLifecycleEpoch) throw new Error('migration_scope_changed');
      if (status.status === 'committed') {
        if (current.records.find(item => item.id === record.id)?.sourceGeneration !== 'chat2') throw new Error('committed_generation_mismatch');
        report.operations.push({ id: record.id, operation: 'status', outcome: 'committed' });
        await persist(report);
        continue;
      }
      if (!['unprepared', 'prepared', 'activating'].includes(status.status)) throw new Error('unknown_migration_claim_state');
      if (status.status === 'unprepared') {
        if (current.records.find(item => item.id === record.id)?.sourceGeneration !== 'session') throw new Error('migration_generation_unproven');
        // The coordinator serializes prepare and reuses an existing exact claim.
        // A delayed first request cannot race recovery into a second token.
        operation = 'prepare';
      }
    }
    if (!await action(record, operation)) return report;
    prepared.push(record);
  }
  report.prepared = true;
  if (prepareOnly) { await persist(report); return report; }
  for (const record of prepared) {
    if (!await action(record, 'commit')) return report;
    const { inventoryRecord: stored } = await api(`/api/sessions/${encodeURIComponent(record.id)}/crew-migration`);
    if (stored?.id !== record.id) throw new Error('authority_record_identity_changed');
    const after = inventoryRecord(stored);
    if (after.sourceGeneration !== 'chat2' || after.preservationSha256 !== record.preservationSha256 || canonical(after.scope) !== canonical(record.scope)
        || after.coordinatorLifecycleEpoch !== record.coordinatorLifecycleEpoch) {
      throw new Error('generation_commit_readback_mismatch');
    }
  }
  report.complete = true;
  await persist(report);
  return report;
}

export function authenticatedApi(token, fetchImpl = fetch, viaStagingBridge = false) {
  if (!nonempty(token) || /[\r\n]/.test(token)) throw new Error('SCAFFOLD_ACCESS_TOKEN_required');
  return async (path, { method = 'GET', body } = {}) => {
    if (!/^\/api\/sessions(?:[/?]|$)/.test(path) && path !== '/api/code-sandboxes/auth/session' && !(viaStagingBridge && path === '/health')) throw new Error('invalid_control_path');
    const phase = path === '/api/code-sandboxes/auth/session' ? 'owner_auth'
      : path === '/health' ? 'bridge_health' : path.startsWith('/api/sessions?') ? 'session_inventory'
        : path.includes('/crew-migration') ? 'migration' : 'session_record';
    const origin = viaStagingBridge ? 'http://127.0.0.1:18081' : CONTROL;
    const response = await fetchImpl(`${origin}${path}`, {
      method, redirect: 'error', signal: AbortSignal.timeout(method === 'GET' ? 120_000 : 900_000),
      headers: { authorization: `Bearer ${token}`, ...(body ? { 'content-type': 'application/json' } : {}) },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    if (!response.ok) throw new Error(`control_${phase}_http_${response.status}`);
    return response.json();
  };
}

export function tokenFromAuthFile(text) {
  const trimmed = text.trim();
  if (!trimmed.startsWith('{')) return trimmed;
  const session = JSON.parse(trimmed);
  if ((session.resource !== undefined && session.resource !== CONTROL)
      || (session.edgeOrigin !== undefined && session.edgeOrigin !== 'https://comet-staging.internal.ashler.com')) throw new Error('auth_file_not_staging');
  const token = session.accessToken ?? session.access_token;
  if (!nonempty(token)) throw new Error('auth_file_missing_access_token');
  return token;
}

function compatibilityVerifier(platformCheckout, receipt) {
  if (!nonempty(platformCheckout) || !nonempty(receipt)) throw new Error('platform_checkout_and_compatibility_receipt_required');
  const script = resolve(platformCheckout, 'internal/scaffold/scripts/deploy-staging-compatible-services.mjs');
  return async () => {
    try {
      // This read-only helper re-fetches actual K8s pods and Worker versions.
      // Neither the local receipt nor a caller boolean establishes a drain.
      await promisify(execFile)(process.execPath, [script, '--verify-receipt', resolve(receipt)], {
        cwd: resolve(platformCheckout), timeout: 300_000, maxBuffer: 1024 * 1024,
      });
    } catch { throw new Error('live_compatibility_verification_failed'); }
  };
}

async function main(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index++) {
    const flag = argv[index];
    if (flag === '--dry-run') { if (args.mode) throw new Error('choose_one_mode'); args.mode = 'dry-run'; }
    else if (flag === '--prepare-only') args.prepareOnly = true;
    else if (flag === '--via-staging-bridge') args.viaStagingBridge = true;
    else if (flag === '--apply' || flag === '--recover') {
      if (args.mode || !argv[index + 1] || argv[index + 1].startsWith('--')) throw new Error('plan_path_required');
      args.mode = flag.slice(2); args.plan = argv[++index];
    } else if (['--report', '--auth-file', '--platform-checkout', '--compatibility-receipt', '--destination-device-id'].includes(flag) && argv[index + 1] && !argv[index + 1].startsWith('--')) args[flag.slice(2)] = argv[++index];
    else throw new Error('usage_--dry-run_or_--apply_PLAN_or_--recover_PLAN_--report_NEW_FILE');
  }
  if (!args.report) throw new Error('report_path_required');
  if (args.prepareOnly && !['apply', 'recover'].includes(args.mode)) throw new Error('prepare_only_requires_apply_or_recover');
  const beforeMutation = args.mode && args.mode !== 'dry-run'
    ? compatibilityVerifier(args['platform-checkout'], args['compatibility-receipt']) : undefined;
  const token = args['auth-file'] ? tokenFromAuthFile(await readFile(args['auth-file'], 'utf8')) : process.env.SCAFFOLD_ACCESS_TOKEN;
  const request = authenticatedApi(token, fetch, args.viaStagingBridge === true);
  const identity = await authenticatedApi(token)('/api/code-sandboxes/auth/session');
  if (identity.ok !== true || identity.resource !== CONTROL || !nonempty(identity.actor?.sub)) throw new Error('verified_current_owner_required');
  if (args.viaStagingBridge) {
    const health = await request('/health');
    if (health.service !== 'scaffold-edge-bridge' || health.workerConfigured !== true) throw new Error('bridge_not_configured');
    const upstreamIdentity = await request('/api/code-sandboxes/auth/session');
    if (upstreamIdentity.ok !== true || upstreamIdentity.resource !== CONTROL || upstreamIdentity.actor?.sub !== identity.actor.sub) throw new Error('bridge_not_verified_staging');
  }
  const api = (path, options) => request(path.startsWith('/api/sessions?') ? `${path}&user=${encodeURIComponent(identity.actor.sub)}` : path, options);
  api.ownerSubject = identity.actor.sub;
  const reportPath = resolve(args.report);
  // Never overwrite a plan/evidence file from an earlier attempt.
  await writeFile(reportPath, '', { flag: 'wx', mode: 0o600 });
  const persist = async report => {
    const temporary = `${reportPath}.${process.pid}.tmp`;
    await writeFile(temporary, `${canonical(report)}\n`, { flag: 'wx', mode: 0o600 });
    await rename(temporary, reportPath);
  };
  if (!args.mode || args.mode === 'dry-run') {
    const plan = await inventory(api);
    await persist(plan);
    process.stdout.write(`${canonical({ mode: 'dry-run', counts: plan.counts, statusCounts: plan.statusCounts, inventorySha256: plan.inventorySha256 })}\n`);
    if (plan.counts.blocked) process.exitCode = 2;
  } else {
    const plan = JSON.parse(await readFile(args.plan, 'utf8'));
    const report = await applyPlan(plan, { api, beforeMutation, recover: args.mode === 'recover', prepareOnly: args.prepareOnly === true, destinationDeviceId: args['destination-device-id'], persist });
    process.stdout.write(`${canonical({ mode: args.mode, prepared: report.prepared, complete: report.complete, operations: report.operations.length })}\n`);
    if (!report.complete && !(args.prepareOnly && report.prepared)) process.exitCode = 2;
  }
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).catch(error => {
    process.stderr.write(`${/^[a-zA-Z0-9_-]{1,100}$/.test(error.message) ? error.message : 'migration_stopped_preserve_evidence_and_recover_explicitly'}\n`);
    process.exitCode = 1;
  });
}
