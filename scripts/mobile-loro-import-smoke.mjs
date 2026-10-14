// Usage: node scripts/mobile-loro-import-smoke.mjs PACKAGES_DIRECTORY
// Public deterministic wide-history fixture; exercises the shipped UniFFI ABI.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

assert.equal(process.argv.length, 3, "Provide the resolved Swift packages directory");
const root = fileURLToPath(new URL("..", import.meta.url));
const packages = resolve(process.argv[2]);
const env = { ...process.env, ASHLER_INCREMENTAL_TSC_CHECKS: "false" };
const run = (command, args) => execFileSync(command, args, { cwd: root, env, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] }).trim();
env.PATH = dirname(run("rustup", ["which", "cargo"])) + ":" + env.PATH;
const framework = join(packages, "artifacts/loro-swift/LoroFFI/loroFFI.xcframework");
const info = JSON.parse(run("plutil", ["-convert", "json", "-o", "-", join(framework, "Info.plist")]));
const device = info.AvailableLibraries.find(item => item.SupportedPlatform === "ios" && !item.SupportedPlatformVariant);
assert.ok(device?.HeadersPath, "Missing pinned UniFFI headers");
const work = mkdtempSync(join(tmpdir(), "crew-mobile-import-smoke-"));
const source = String.raw`
#include <assert.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <mach/mach.h>
#include <loroFFI/loroFFI.h>
static atomic_int done;
static atomic_ullong peak;
static void sample_memory(void) {
    task_vm_info_data_t info;
    mach_msg_type_number_t count = TASK_VM_INFO_COUNT;
    assert(task_info(mach_task_self(), TASK_VM_INFO, (task_info_t)&info, &count) == KERN_SUCCESS);
    unsigned long long value = info.phys_footprint, previous = atomic_load(&peak);
    while (previous < value && !atomic_compare_exchange_weak(&peak, &previous, value)) {}
    if (value > 768ULL * 1048576) {
        fprintf(stderr, "Native mobile import exceeds 768 MiB physical footprint\n");
        _Exit(86);
    }
}
static void *watch(void *unused) {
    (void)unused;
    while (!atomic_load(&done)) { sample_memory(); usleep(10000); }
    return NULL;
}
static void ok(RustCallStatus *status) { assert(status->code == 0); }
static RustBuffer lower(const unsigned char *bytes, int length) {
    RustCallStatus status = {0};
    RustBuffer result = ffi_loro_ffi_rustbuffer_from_bytes((ForeignBytes){.len=length,.data=bytes}, &status);
    ok(&status); return result;
}
static uint64_t clone_doc(uint64_t doc) {
    RustCallStatus status = {0};
    uint64_t clone = uniffi_loro_ffi_fn_clone_lorodoc(doc, &status);
    ok(&status); return clone;
}
int main(int argc, char **argv) {
    assert(argc >= 4);
    pthread_t watcher;
    assert(pthread_create(&watcher, NULL, watch, NULL) == 0);
    RustCallStatus status = {0};
    uint64_t doc = uniffi_loro_ffi_fn_constructor_lorodoc_new(&status);
    ok(&status);
    for (int i = 2; i < argc; i++) {
        FILE *file = fopen(argv[i], "rb");
        assert(file && fseek(file, 0, SEEK_END) == 0);
        long length = ftell(file);
        assert(length > 0 && length < 64 * 1048576 && fseek(file, 0, SEEK_SET) == 0);
        unsigned char *bytes = malloc((size_t)length + 4);
        assert(bytes);
        bytes[0] = (length >> 24) & 255; bytes[1] = (length >> 16) & 255;
        bytes[2] = (length >> 8) & 255; bytes[3] = length & 255;
        assert(fread(bytes + 4, 1, (size_t)length, file) == (size_t)length);
        fclose(file);
        RustBuffer data = lower(bytes, (int)length + 4);
        free(bytes);
        RustBuffer imported = uniffi_loro_ffi_fn_method_lorodoc_import_with(clone_doc(doc), data,
            lower((const unsigned char *)"smoke", 5), &status);
        ok(&status);
        ffi_loro_ffi_rustbuffer_free(imported, &status); ok(&status);
        sample_memory();
    }
    uint64_t materialized = uniffi_loro_ffi_fn_method_lorodoc_state_vv(clone_doc(doc), &status); ok(&status);
    uint64_t accepted = uniffi_loro_ffi_fn_method_lorodoc_oplog_vv(clone_doc(doc), &status); ok(&status);
    int8_t equal = uniffi_loro_ffi_fn_method_versionvector_eq(materialized, accepted, &status); ok(&status);
    assert(equal);
    RustBuffer snapshot = uniffi_loro_ffi_fn_method_lorodoc_export_snapshot(clone_doc(doc), &status); ok(&status);
    assert(snapshot.len >= 4);
    size_t length = ((size_t)snapshot.data[0] << 24) | ((size_t)snapshot.data[1] << 16)
        | ((size_t)snapshot.data[2] << 8) | snapshot.data[3];
    assert(length == snapshot.len - 4);
    FILE *output = fopen(argv[1], "wb");
    assert(output && fwrite(snapshot.data + 4, 1, length, output) == length);
    fclose(output);
    ffi_loro_ffi_rustbuffer_free(snapshot, &status); ok(&status);
    sample_memory();
    atomic_store(&done, 1); pthread_join(watcher, NULL);
    uniffi_loro_ffi_fn_free_lorodoc(doc, &status); ok(&status);
    printf("{\"peakPhysicalMiB\":%.2f}\n", atomic_load(&peak)/1048576.0);
}
`;
try {
  const library = join(work, "libloro_swift.a");
  run("python3", ["scripts/local-cargo.py", "build", "--locked", "--release", "-p", "comet-mobile-ffi",
    "--copy-binary", "release/libloro_swift.a", library]);
  run(process.execPath, [join(root, "edge/scripts/workspace-replay-memory-smoke.mjs"), "--prepare", work]);
  const manifest = JSON.parse(readFileSync(join(work, "manifest.json"), "utf8"));
  writeFileSync(join(work, "probe.c"), source);
  const executable = join(work, "probe");
  run("xcrun", ["clang", "-arch", "arm64", "-O2", "-I", join(framework, device.LibraryIdentifier, device.HeadersPath),
    join(work, "probe.c"), library, "-framework", "Security", "-framework", "CoreFoundation", "-liconv", "-o", executable]);
  const snapshot = join(work, "native-export.loro");
  const memory = JSON.parse(run(executable, [snapshot, ...manifest.files]));
  assert.ok(memory.peakPhysicalMiB < 256, "Wide-history mobile import exceeds the 256 MiB native budget");
  const { LoroDoc } = createRequire(new URL("../edge/package.json", import.meta.url))("loro-crdt");
  const reference = new LoroDoc(), actual = new LoroDoc(), cold = new LoroDoc();
  const vectors = [];
  const vv = value => { vectors.push(value); return value.toJSON(); };
  const frontiers = value => value.map(({ peer, counter }) => [String(peer), counter]).sort();
  try {
    for (const file of manifest.files) reference.import(readFileSync(file));
    actual.import(readFileSync(snapshot));
    cold.import(actual.export({ mode: "snapshot" }));
    for (const replica of [actual, cold]) {
      assert.deepEqual(replica.toJSON(), reference.toJSON(), "Mobile ABI changed visible state");
      assert.deepEqual(vv(replica.oplogVersion()), vv(reference.oplogVersion()), "Mobile ABI lost accepted history");
      assert.deepEqual(vv(replica.version()), vv(replica.oplogVersion()), "Mobile ABI left state behind its oplog");
      assert.deepEqual(vv(replica.shallowSinceVV()), vv(reference.shallowSinceVV()), "Mobile ABI changed the retained floor");
      assert.deepEqual(frontiers(replica.shallowSinceFrontiers()), frontiers(reference.shallowSinceFrontiers()));
      assert.equal(replica.opCount(), reference.opCount());
      assert.equal(replica.changeCount(), reference.changeCount());
    }
    console.log(JSON.stringify({ ...memory, platform: process.platform, architecture: process.arch,
      stateAndHistoryEquivalent: true, coldReopenEquivalent: true,
      operations: reference.opCount(), changes: reference.changeCount() }));
  } finally { for (const vector of vectors) vector.free(); reference.free(); actual.free(); cold.free(); }
} finally { rmSync(work, { recursive: true, force: true }); }
