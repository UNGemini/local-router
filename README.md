<picture>
  <source media="(prefers-color-scheme: dark)" srcset="images/logomark-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="images/logomark-light.png">
  <img alt="Wheels Router Nano" src="images/logomark-light.png" width="400">
</picture>

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[Demo (Hong Kong)](https://router-nano.justusewheels.com)

Wheels Router Nano is:
- a multi-modal trip planner
- should runs on WASM
- uses least data as possible
- no bloat, but no code golf as well

Wheels Router Nano is not [Wheels Router](https://router.justusewheels.com/) Service. Wheels Router Nano does not account for the fares, and might have differances in it's results.

Maintained by Wheels Labs, a part of Wheels Softworks

### Contributing
- We welcome code contributions. however, ai slop pull request will be closed instantly.
- if you did use AI in the process, read it yourself before submitting the PR. remove anything you don't need. make sure every line is correct, necessary, and as clean as it can be. because if you aren't willing to put in that time, why should we?

---

### Setup

Prerequisites: Rust, Python 3, [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/)

```bash
pip install pycapnp pyosmium
```

Build the data file from a GTFS zip and OSM extract:

```bash
python -m pipeline.build --gtfs data/gtfs.zip --osm data/region.osm.pbf -o data/region.wheelsrouter
```

Build the WASM package:

```bash
wasm-pack build --target web --out-dir pkg --release -- --features wasm
```

### Usage

From Rust:

```rust
let bytes = std::fs::read("data/region.wheelsrouter").unwrap();
let router = wheels_router_nano::Router::load(&bytes).unwrap();

let result_json = router.plan_json(r#"{
    "origin": "37.793,-122.397",
    "destination": "37.788,-122.417",
    "depart_at": "2026-03-09T10:00:00Z",
    "max_results": 5
}"#).unwrap();
```

From WASM/JS:

```js
import init, { WasmRouter } from './pkg/wheels_router_nano.js';

await init();
const data = new Uint8Array(await (await fetch('data/region.wheelsrouter')).arrayBuffer());
const router = new WasmRouter(data);

const plans = router.plan({
    origin: '37.793,-122.397',
    destination: '37.788,-122.417',
    depart_at: '2026-03-09T10:00:00Z',
    max_results: 5,
});
```

**Request parameters:**

| Parameter | Required | Description |
|---|---|---|
| `origin` | yes | `"lat,lon"` |
| `destination` | yes | `"lat,lon"` |
| `depart_at` | no | ISO 8601 departure time (UTC) |
| `max_results` | no | max plans to return (default 15) |
| `max_transfers` | no | max transfers allowed (default 3) |
| `max_walk_distance` | no | max access/egress walk in meters (default 1600) |
| `walking_speed` | no | `"slow"`, `"normal"`, or `"fast"` |

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="images/wlabs.png">
  <source media="(prefers-color-scheme: light)" srcset="images/wlabs.png">
  <img alt="Wheels Labs" src="images/wlabs.png" width="70">
</picture>
