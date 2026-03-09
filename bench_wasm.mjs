import { initSync, WasmRouter } from './pkg/wheels_router_nano.js';
import * as fs from 'fs';

// read both the wasm binary and the data
const wasmBytes = fs.readFileSync('pkg/wheels_router_nano_bg.wasm');
const dataBytes = fs.readFileSync('data/marin.wheelsrouter');
const wasmData = new Uint8Array(dataBytes);

// initialize wasm module first with the binary
console.log('initializing WASM...');
const wasmStart = performance.now();
initSync(wasmBytes);
const wasmInitMs = performance.now() - wasmStart;
console.log(`wasm init time: ${wasmInitMs.toFixed(2)}ms`);

// then create router
console.log('loading router data...');
const start = performance.now();
const router = new WasmRouter(wasmData);
const loadMs = performance.now() - start;
console.log(`router load time: ${loadMs.toFixed(2)}ms`);

const stats = router.stats();
console.log(`stats: ${stats}`);

const request = JSON.stringify({
  origin: "37.971195,-122.522396",
  destination: "37.903697,-122.519149",
  depart_at: "2026-03-09T08:00:00Z",
  max_results: 5
});

// warmup query
router.plan(request);

// benchmark 100 queries
console.log('\nbenchmarking 100 queries...');
const queryStart = performance.now();
for (let i = 0; i < 100; i++) {
  router.plan(request);
}
const queryMs = performance.now() - queryStart;
const perQuery = queryMs / 100;

console.log(`100 queries: ${queryMs.toFixed(2)}ms total`);
console.log(`per query: ${perQuery.toFixed(3)}ms`);
console.log(`\nrouter data: ${(dataBytes.length / 1024).toFixed(1)}kb`);
console.log(`wasm binary: ${(wasmBytes.length / 1024).toFixed(1)}kb`);
console.log(`total download: ${((wasmBytes.length + dataBytes.length) / 1024 / 1024).toFixed(2)}mb`);


