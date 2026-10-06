import { initSync } from "../node_modules/loro-crdt/web/index.js";
import module from "../node_modules/loro-crdt/web/loro_wasm_bg.wasm";

initSync({ module });

export * from "../node_modules/loro-crdt/web/index.js";
