// visualize/app.js  — RAPTOR algorithm visualizer
import init, { WasmRouter } from "../pkg/wheels_router_nano.js";

// ── constants ────────────────────────────────────────────────────────────────

const CITIES = {
    hk: { label: "Hong Kong", file: "../data/hk.wheelsrouter.gz", center: [22.32, 114.17], zoom: 12 },
    sf: { label: "San Francisco", file: "../data/sf.wheelsrouter.gz", center: [37.77, -122.42], zoom: 12 },
};

// round 0 = walk-reach / origin; rounds 1–4 = RAPTOR k-rounds
const ROUND_COLORS = [
    "#00e5ff",  // 0 — origin / walk  (cyan)
    "#00ff88",  // 1 — first transit leg (green)
    "#f59e0b",  // 2 — second leg (amber)
    "#f43f5e",  // 3 — third leg (rose)
    "#a855f7",  // 4 — fourth leg (violet)
];

const ROUND_LABELS = [
    "Origin / Walk reach",
    "Round 1 — 1st transit leg",
    "Round 2 — 2nd transit leg",
    "Round 3 — 3rd transit leg",
    "Round 4 — 4th transit leg",
];

// Planner-matching colors (from index.html)
const WALK_COLOR     = "#ff9500";
const WALK_DASH      = "6 8";
const TRANSIT_WEIGHT = 4;
const STOP_DOT_FILL  = "#ffffff";

// NOT_SET sentinel (matches Rust u32::MAX)
const NOT_SET = 0xffffffff;

// How many stops remain visible behind the wavefront before fading to invisible.
// This gives the "spreading wave" look without opacity accumulation from overlaps.
const TRAIL_LENGTH = 80;

// pause between RAPTOR rounds (ms) — set to 0 to disable
const ROUND_PAUSE = 0;

// ── state ────────────────────────────────────────────────────────────────────

let router = null;
let currentCity = "hk";
let switchingCity = false;

let clickMode     = "origin";   // "origin" | "destination"
let originLatLon  = null;
let destLatLon    = null;
let originMarker  = null;
let destMarker    = null;

let vizData       = null;   // raw result from router.plan_viz()
let allStops      = [];     // flat array sorted by arrival_secs: [{...stop, roundIdx}]
let stopByIdx     = new Map(); // stop_idx → allStops entry (best arrival)
let routeTable    = null;   // vizData.routes: { route_idx → { color, stops: [[lat,lon]…] } }

// Canvas renderer (shared for all stop markers – avoids per-marker DOM nodes)
let canvasRenderer = null;

// Pre-created Leaflet layers (all added to map immediately, opacity controlled)
let stopCircles   = [];     // parallel to allStops; L.circleMarker with canvas renderer
let pathLayers    = [];     // polylines drawn when destination is reached
let destStopEntry = null;   // entry in allStops closest to destLatLon

// Animation state
let revealedCount = 0;
let isPlaying     = false;
let animHandle    = null;   // requestAnimationFrame handle
let lastFrameTime = 0;
let activeRound   = -1;
let destReached   = false;

// ms per stop reveal (based on speed selector)
const SPEED_MS = { slow: 25, normal: 8, fast: 2, instant: 0 };

// ── map init ─────────────────────────────────────────────────────────────────

const map = L.map("map", { zoomControl: false, attributionControl: false })
    .setView([22.32, 114.17], 12);

// Custom pane for destination-path layers — animated via CSS transition
map.createPane("pathPane");
map.getPane("pathPane").style.zIndex = 450;  // above overlayPane (400), below popupPane

L.control.zoom({ position: "topright" }).addTo(map);
L.control.attribution({ position: "bottomright", prefix: false })
    .addAttribution('&copy; <a href="https://www.openstreetmap.org/copyright">OSM</a> &bull; <a href="https://carto.com">CARTO</a>')
    .addTo(map);

L.tileLayer(
    "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png",
    { maxZoom: 19, subdomains: "abcd" }
).addTo(map);

// ── DOM refs ─────────────────────────────────────────────────────────────────

const $ = id => document.getElementById(id);

const loadingOverlay = $("loadingOverlay");
const loadingText    = $("loadingText");
const statusEl       = $("status");
const originInput    = $("originInput");
const destInput      = $("destInput");
const clickHint      = $("clickHint");
const dateInput      = $("dateInput");
const timeInput      = $("timeInput");
const walkspeedSel   = $("walkspeedSel");
const runBtn         = $("runBtn");
const pbSlider       = $("pbSlider");
const pbPlayBtn      = $("pbPlayBtn");
const pbPrevBtn      = $("pbPrevBtn");
const pbNextBtn      = $("pbNextBtn");
const pbRoundLabel   = $("pbRoundLabel");
const speedSel       = $("speedSel");
const algoBox        = $("algoBox");
const algoRound      = $("algoRound");
const algoDesc       = $("algoDesc");
const roundLegend    = $("roundLegend");
const statReached    = $("statReached");
const statRounds     = $("statRounds");
const statStops      = $("statStops");
const statMs         = $("statMs");

// ── helpers ───────────────────────────────────────────────────────────────────

async function fetchAndDecompress(url) {
    const resp = await fetch(url);
    if (!resp.ok) throw new Error(`fetch failed: ${resp.status}`);
    const ds = new DecompressionStream("gzip");
    const reader = resp.body.pipeThrough(ds).getReader();
    const chunks = [];
    let total = 0;
    for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value); total += value.byteLength;
    }
    const out = new Uint8Array(total);
    let off = 0;
    for (const c of chunks) { out.set(c, off); off += c.byteLength; }
    return out;
}

function fmtSecs(s) {
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
    return `${String(h).padStart(2,"0")}:${String(m).padStart(2,"0")}`;
}

function setStatus(msg, cls = "") {
    statusEl.textContent = msg;
    statusEl.className = "header-status" + (cls ? " " + cls : "");
}

function makeOriginIcon() {
    return L.divIcon({ className: "", html: `<div class="pin-origin"></div>`, iconSize: [16,16], iconAnchor: [8,8] });
}
function makeDestIcon() {
    return L.divIcon({ className: "", html: `<div class="pin-dest"></div>`, iconSize: [16,16], iconAnchor: [8,8] });
}

// ── click mode ────────────────────────────────────────────────────────────────

function setClickMode(mode) {
    clickMode = mode;
    clickHint.textContent = mode === "origin" ? "click map to set origin" : "click map to set destination";
    clickHint.dataset.mode = mode;
    $("originField").classList.toggle("active-field", mode === "origin");
    $("destField").classList.toggle("active-field", mode === "destination");
}

$("originField").addEventListener("click", () => setClickMode("origin"));
$("destField").addEventListener("click", () => setClickMode("destination"));

// ── map click ─────────────────────────────────────────────────────────────────

map.on("click", e => {
    if (!router || switchingCity) return;
    const { lat, lng } = e.latlng;
    if (clickMode === "origin") {
        originLatLon = [lat, lng];
        originInput.value = `${lat.toFixed(5)}, ${lng.toFixed(5)}`;
        if (originMarker) map.removeLayer(originMarker);
        originMarker = L.marker([lat, lng], { icon: makeOriginIcon() }).addTo(map);
        setClickMode("destination");
    } else {
        destLatLon = [lat, lng];
        destInput.value = `${lat.toFixed(5)}, ${lng.toFixed(5)}`;
        if (destMarker) map.removeLayer(destMarker);
        destMarker = L.marker([lat, lng], { icon: makeDestIcon() }).addTo(map);
        setClickMode("origin");
    }
    runBtn.disabled = !(originLatLon && destLatLon);
});

// ── city loading ──────────────────────────────────────────────────────────────

async function loadCity(key) {
    const city = CITIES[key];
    loadingOverlay.classList.remove("hidden");
    loadingText.textContent = `loading ${city.label}...`;
    setStatus("loading...", "loading");
    runBtn.disabled = true;
    clearViz();
    try {
        const buf = await fetchAndDecompress(city.file);
        loadingText.textContent = "building routing graph...";
        if (router && typeof router.free === "function") router.free();
        router = new WasmRouter(buf);
        const stats = router.stats();
        statStops.textContent = stats.stops.toLocaleString();
        setStatus("", "");
        loadingOverlay.classList.add("hidden");
    } catch (e) {
        loadingText.textContent = `error: ${e.message}`;
        setStatus("error", "error");
        console.error(e);
    }
}

$("cityHK").addEventListener("click", () => switchCity("hk"));
$("citySF").addEventListener("click", () => switchCity("sf"));

async function switchCity(key) {
    if (key === currentCity || switchingCity) return;
    switchingCity = true;
    $("cityHK").classList.toggle("active", key === "hk");
    $("citySF").classList.toggle("active", key === "sf");
    $("cityHK").disabled = true; $("citySF").disabled = true;
    clearViz();
    if (originMarker) { map.removeLayer(originMarker); originMarker = null; }
    if (destMarker)   { map.removeLayer(destMarker);   destMarker   = null; }
    originLatLon = null; destLatLon = null;
    originInput.value = ""; destInput.value = "";
    map.flyTo(CITIES[key].center, CITIES[key].zoom, { duration: 1.1 });
    currentCity = key;
    await loadCity(key);
    $("cityHK").disabled = false; $("citySF").disabled = false;
    switchingCity = false;
}

// ── run visualization ──────────────────────────────────────────────────────────

runBtn.addEventListener("click", runViz);

async function runViz() {
    if (!router || !originLatLon || !destLatLon) return;
    stopPlayback();
    clearViz();
    runBtn.disabled = true;
    setStatus("computing...", "running");

    const departAt = `${dateInput.value}T${timeInput.value}:00Z`;
    const t0 = performance.now();
    let result;
    try {
        result = router.plan_viz({
            origin:            `${originLatLon[0]},${originLatLon[1]}`,
            destination:       `${destLatLon[0]},${destLatLon[1]}`,
            depart_at:         departAt,
            walking_speed:     walkspeedSel.value,
            max_walk_distance: 1200,
        });
    } catch (e) {
        setStatus("error", "error");
        runBtn.disabled = false;
        console.error(e);
        return;
    }
    const elapsed = (performance.now() - t0).toFixed(0);

    vizData    = result;
    routeTable = result.routes; // { "route_idx": { color, stops: [[lat,lon],…] } }

    // flatten all rounds sorted by arrival_secs
    allStops = [];
    vizData.rounds.forEach((stops, roundIdx) => {
        stops.forEach(s => allStops.push({ ...s, roundIdx }));
    });
    allStops.sort((a, b) => a.arrival_secs - b.arrival_secs);

    // build stop lookup
    stopByIdx.clear();
    allStops.forEach(s => {
        if (!stopByIdx.has(s.stop_idx) || stopByIdx.get(s.stop_idx).arrival_secs > s.arrival_secs)
            stopByIdx.set(s.stop_idx, s);
    });

    // find dest stop
    destStopEntry = findDestStop();

    const totalReached = allStops.length;
    const numRounds    = vizData.rounds.filter(r => r.length > 0).length;
    statReached.textContent = totalReached.toLocaleString();
    statRounds.textContent  = numRounds;
    statMs.textContent      = elapsed + "ms";
    setStatus(`${totalReached.toLocaleString()} stops reached`, "");

    // pre-create canvas renderer + all stop circles (invisible, added to map once)
    buildCanvasLayers();
    buildLegend();

    pbSlider.max     = allStops.length - 1;
    pbSlider.value   = 0;
    pbSlider.disabled  = false;
    pbPlayBtn.disabled = false;
    pbPrevBtn.disabled = false;
    pbNextBtn.disabled = false;
    pbRoundLabel.textContent = `0 / ${allStops.length}`;

    runBtn.disabled = false;

    if (speedSel.value !== "instant") {
        startPlayback();
    } else {
        revealUpTo(allStops.length - 1);
    }
}

// ── destination detection ─────────────────────────────────────────────────────

function findDestStop() {
    if (!destLatLon || allStops.length === 0) return null;
    const [dlat, dlon] = destLatLon;
    let best = null, bestDist = Infinity;
    for (const s of allStops) {
        const d = Math.hypot(s.lat - dlat, s.lon - dlon);
        if (d < bestDist) { bestDist = d; best = s; }
    }
    // ~400m threshold
    return bestDist < 0.004 ? best : null;
}

// ── canvas layer building ─────────────────────────────────────────────────────
// All circles are added to the map immediately (canvas renderer draws them in
// one pass). We set fillOpacity=0 to hide them, then reveal by updating opacity.
// This avoids any DOM mutation during animation — only canvas redraws.

function buildCanvasLayers() {
    canvasRenderer = L.canvas({ padding: 0.2 });

    stopCircles = allStops.map((s, i) => {
        const isDest  = destStopEntry && s.stop_idx === destStopEntry.stop_idx;
        const color   = ROUND_COLORS[s.roundIdx] ?? ROUND_COLORS[ROUND_COLORS.length - 1];
        const radius  = isDest ? 8 : (s.roundIdx === 0 ? 3 : 5);

        const circle = L.circleMarker([s.lat, s.lon], {
            renderer:    canvasRenderer,
            radius,
            fillColor:   color,
            color:       isDest ? "#fff" : "transparent",
            weight:      isDest ? 2 : 0,
            fillOpacity: 0,
            opacity:     0,
        });

        const arrStr = fmtSecs(s.arrival_secs);
        circle.bindTooltip(
            `<strong>${s.name || "Stop"}</strong><br>arr ${arrStr} · round ${s.round}`,
            { className: "stop-tooltip", direction: "top", offset: [0,-5], sticky: false }
        );
        circle.addTo(map);
        return circle;
    });
}

// ── reveal logic ──────────────────────────────────────────────────────────────
// Opacity model:
//   - Stops within TRAIL_LENGTH of the wavefront front: visible at full opacity
//   - Stops older than TRAIL_LENGTH: hidden (fillOpacity 0) — avoids overlap accumulation
//   - After dest reached: path stops light up permanently; everything else hidden

const FULL_OPACITY_ROUND0 = 0.55;
const FULL_OPACITY        = 0.85;

// Set a single circle to the correct opacity for its position relative to the trail window.
// prevFront is the old front index before this advance (or -1 on reset).
function setCircleOpacity(i) {
    const front      = revealedCount - 1;
    const trailStart = Math.max(0, front - TRAIL_LENGTH + 1);
    const circle = stopCircles[i];
    if (i > front || i < trailStart) {
        if (circle.options.fillOpacity !== 0) circle.setStyle({ fillOpacity: 0 });
    } else {
        const s      = allStops[i];
        const isDest = destStopEntry && s.stop_idx === destStopEntry.stop_idx;
        const target = isDest ? 1 : (s.roundIdx === 0 ? FULL_OPACITY_ROUND0 : FULL_OPACITY);
        if (circle.options.fillOpacity !== target) circle.setStyle({ fillOpacity: target });
    }
}

// O(1) trail update: only touch the newly-revealed circle (entering the trail front)
// and the one that just fell off the back of the trail window.
function advanceTrail(newFront) {
    // show the newly revealed stop
    setCircleOpacity(newFront);
    // hide the one that just fell off the back of the trail (if any)
    const dropped = newFront - TRAIL_LENGTH;
    if (dropped >= 0) setCircleOpacity(dropped);
}

// Full O(n) rebuild used only after backward scrub or dest-reached dim.
function rebuildTrail() {
    const front      = revealedCount - 1;
    const trailStart = Math.max(0, front - TRAIL_LENGTH + 1);
    for (let i = 0; i < stopCircles.length; i++) {
        const circle = stopCircles[i];
        if (i > front || i < trailStart) {
            if (circle.options.fillOpacity !== 0) circle.setStyle({ fillOpacity: 0 });
        } else {
            const s      = allStops[i];
            const isDest = destStopEntry && s.stop_idx === destStopEntry.stop_idx;
            const target = isDest ? 1 : (s.roundIdx === 0 ? FULL_OPACITY_ROUND0 : FULL_OPACITY);
            if (circle.options.fillOpacity !== target) circle.setStyle({ fillOpacity: target });
        }
    }
}

function revealStop(i) {
    const s = allStops[i];

    if (s.roundIdx !== activeRound) {
        activeRound = s.roundIdx;
        updateAlgoBox(activeRound);
        updateLegendHighlight(activeRound);
    }

    // destination reached?
    if (!destReached && destStopEntry && s.stop_idx === destStopEntry.stop_idx) {
        destReached = true;
        onDestinationReached(s, stopCircles[i]);
    }
}

function revealUpTo(targetIdx) {
    const clamp = Math.min(targetIdx, allStops.length - 1);

    if (clamp >= revealedCount) {
        // reveal forward — O(1) trail update per stop
        for (let i = revealedCount; i <= clamp; i++) {
            revealStop(i);
            revealedCount = i + 1;
            advanceTrail(i);
        }
    } else {
        // scrubbing backward — reset dest state and re-reveal from scratch
        destReached = false;
        for (const l of pathLayers) map.removeLayer(l);
        pathLayers = [];
        revealedCount = 0;
        activeRound = -1;
        for (let i = 0; i <= clamp; i++) {
            revealStop(i);
            revealedCount = i + 1;
        }
        rebuildTrail();
    }

    if (destReached && pathLayers.length === 0 && destStopEntry) {
        drawDestinationPath();
    }

    updateSlider();
}

// ── destination reached ───────────────────────────────────────────────────────

function onDestinationReached(s, circle) {
    // stop playback immediately
    stopPlayback();

    // hide all wavefront canvas circles — the path will be drawn as planner-style layers
    for (const c of stopCircles) {
        if (c.options.fillOpacity !== 0) c.setStyle({ fillOpacity: 0, opacity: 0 });
    }

    drawDestinationPath();

    // smooth CSS pulse via a divIcon overlay placed on top
    spawnPulseMarker(s.lat, s.lon, circle.options.fillColor);

    map.panTo([s.lat, s.lon], { animate: true, duration: 0.7 });
}


// Spawn a temporary divIcon that plays a CSS pulse animation, then removes itself
function spawnPulseMarker(lat, lon, color) {
    const el = document.createElement("div");
    el.className = "dest-pulse";
    el.style.setProperty("--pulse-color", color);
    const icon = L.divIcon({ className: "", html: el, iconSize: [0, 0], iconAnchor: [0, 0] });
    const m = L.marker([lat, lon], { icon, interactive: false, zIndexOffset: 1000 }).addTo(map);
    el.addEventListener("animationend", () => map.removeLayer(m), { once: true });
}

// Call router.plan() and draw the result using the same style as the main planner.
function drawDestinationPath() {
    if (!originLatLon || !destLatLon || !router) return;

    // Reset pane to invisible before redrawing
    const pathPane = map.getPane("pathPane");
    pathPane.classList.remove("path-visible");

    for (const l of pathLayers) map.removeLayer(l);
    pathLayers = [];

    let result;
    try {
        result = router.plan({
            origin:           `${originLatLon[0]},${originLatLon[1]}`,
            destination:      `${destLatLon[0]},${destLatLon[1]}`,
            depart_at:        `${dateInput.value}T${timeInput.value}:00Z`,
            walking_speed:    walkspeedSel.value,
            max_walk_distance: 1200,
        });
    } catch (e) {
        console.error("drawDestinationPath: plan() failed:", e);
        return;
    }

    const plan = result?.plans?.[0];
    if (!plan) return;

    const paneOpts = { pane: "pathPane" };

    const walkStyles = {
        station_access:   { color: "#ff9500", weight: 3, dashArray: "5,7", opacity: 0.85 },
        station_egress:   { color: "#ff3b30", weight: 3, dashArray: "5,7", opacity: 0.85 },
        station_transfer: { color: "#af52de", weight: 3, dashArray: "4,5", opacity: 0.85 },
        walk:             { color: "#ff9500", weight: 3, dashArray: "5,7", opacity: 0.85 },
    };

    for (const leg of plan.legs) {
        if (leg.type === "walk") {
            const coords = [];
            if (leg.path?.length >= 2) {
                for (const p of leg.path) coords.push([p.lat, p.lon]);
            } else {
                const from = leg.from?.location, to = leg.to?.location;
                if (from && to) coords.push([from.lat, from.lon], [to.lat, to.lon]);
            }
            if (coords.length >= 2) {
                const style = { ...(walkStyles[leg.walk_type] ?? walkStyles.walk), ...paneOpts };
                pathLayers.push(L.polyline(coords, style).addTo(map));
            }
        } else if (leg.type === "transit") {
            const ro = leg.route_options[0];
            const color = ro.color ? `#${ro.color}` : "#007aff";
            const stops = ro.stops ?? [];
            const coords = stops.filter(s => s.location).map(s => [s.location.lat, s.location.lon]);
            if (coords.length >= 2) {
                pathLayers.push(L.polyline(coords, { color, weight: 4, opacity: 0.95, ...paneOpts }).addTo(map));
                coords.forEach((c, i) => {
                    const dot = L.circleMarker(c, {
                        radius: 3.5, fillColor: "#fff", color, weight: 2, fillOpacity: 1, ...paneOpts,
                    });
                    const name = stops[i]?.stop_name;
                    if (name) dot.bindTooltip(name, { className: "stop-tooltip", direction: "top", offset: [0, -6] });
                    dot.addTo(map);
                    pathLayers.push(dot);
                });
            }
        }
    }

    // Trigger fade-in + scale-grow after a frame so the browser registers the reset first
    requestAnimationFrame(() => {
        requestAnimationFrame(() => pathPane.classList.add("path-visible"));
    });
}

// ── animation engine ──────────────────────────────────────────────────────────
// Uses requestAnimationFrame + timestamp-based pacing.
// lastFrameTime is reset at round boundaries so elapsed doesn't accumulate
// during the pause and cause a catch-up burst.

function startPlayback() {
    if (isPlaying) return;
    isPlaying = true;
    lastFrameTime = performance.now();
    updatePlayBtn();
    animHandle = requestAnimationFrame(animFrame);
}

function stopPlayback() {
    isPlaying = false;
    if (animHandle !== null) { cancelAnimationFrame(animHandle); animHandle = null; }
    updatePlayBtn();
}

// State for round-boundary pause
let roundPauseStart = -1;

function animFrame(now) {
    if (!isPlaying || !vizData) return;

    const msPerStop = SPEED_MS[speedSel.value] ?? 8;

    if (msPerStop === 0) {
        revealUpTo(allStops.length - 1);
        stopPlayback();
        return;
    }

    // Check round boundary pause
    const nextIdx = revealedCount;
    if (nextIdx < allStops.length && nextIdx > 0) {
        const curRound  = allStops[nextIdx - 1].roundIdx;
        const nextRound = allStops[nextIdx].roundIdx;
        if (nextRound !== curRound) {
            if (roundPauseStart < 0) {
                // just hit the boundary — start the pause, reset lastFrameTime for after
                roundPauseStart = now;
                animHandle = requestAnimationFrame(animFrame);
                return;
            } else if (now - roundPauseStart < ROUND_PAUSE) {
                // still pausing
                animHandle = requestAnimationFrame(animFrame);
                return;
            } else {
                // pause over — reset both clocks so we don't burst
                roundPauseStart = -1;
                lastFrameTime = now;
            }
        } else {
            roundPauseStart = -1;
        }
    }

    const elapsed   = now - lastFrameTime;
    const batchSize = Math.max(1, Math.floor(elapsed / msPerStop));
    const target    = Math.min(revealedCount + batchSize - 1, allStops.length - 1);

    revealUpTo(target);
    lastFrameTime = now;

    if (revealedCount >= allStops.length || destReached) {
        stopPlayback();
        return;
    }

    animHandle = requestAnimationFrame(animFrame);
}

function updatePlayBtn() {
    pbPlayBtn.textContent = isPlaying ? "⏸" : "▶";
}

pbPlayBtn.addEventListener("click", () => {
    if (!vizData) return;
    if (isPlaying) { stopPlayback(); return; }
    if (revealedCount >= allStops.length) {
        // restart
        resetVizState();
    }
    startPlayback();
});

pbPrevBtn.addEventListener("click", () => {
    if (!vizData) return;
    stopPlayback();
    const jump = Math.max(1, Math.floor(allStops.length * 0.05));
    revealUpTo(Math.max(0, revealedCount - jump - 1));
});

pbNextBtn.addEventListener("click", () => {
    if (!vizData) return;
    stopPlayback();
    const jump = Math.max(1, Math.floor(allStops.length * 0.05));
    revealUpTo(Math.min(allStops.length - 1, revealedCount + jump - 1));
});

pbSlider.addEventListener("input", () => {
    if (!vizData) return;
    stopPlayback();
    revealUpTo(parseInt(pbSlider.value));
});

// ── clear / reset ─────────────────────────────────────────────────────────────

function clearVizState() {
    revealedCount = 0;
    activeRound   = -1;
    destReached   = false;
}

function resetVizState() {
    for (const l of pathLayers) map.removeLayer(l);
    pathLayers = [];
    destReached = false;
    // hide all circles
    for (const c of stopCircles) c.setStyle({ fillOpacity: 0, opacity: 0 });
    clearVizState();
    updateSlider();
}

function clearViz() {
    stopPlayback();
    for (const c of stopCircles) { try { map.removeLayer(c); } catch(_){} }
    for (const l of pathLayers) { try { map.removeLayer(l); } catch(_){} }
    stopCircles = [];
    pathLayers  = [];
    vizData     = null;
    allStops    = [];
    stopByIdx.clear();
    routeTable      = null;
    destStopEntry   = null;
    clearVizState();

    algoBox.classList.add("hidden");
    roundLegend.innerHTML = "";
    pbSlider.disabled  = true;
    pbPlayBtn.disabled = true;
    pbPrevBtn.disabled = true;
    pbNextBtn.disabled = true;
    pbRoundLabel.textContent = "—";
    updatePlayBtn();
}

// ── UI updates ────────────────────────────────────────────────────────────────

function updateAlgoBox(roundIdx) {
    if (!vizData) { algoBox.classList.add("hidden"); return; }
    algoBox.classList.remove("hidden");
    const stops = vizData.rounds[roundIdx] || [];
    const color = ROUND_COLORS[roundIdx] ?? ROUND_COLORS[ROUND_COLORS.length - 1];
    algoRound.textContent = ROUND_LABELS[roundIdx] ?? `Round ${roundIdx}`;
    algoRound.style.color = color;
    algoDesc.textContent  = stops.length > 0
        ? `${stops.length.toLocaleString()} stops newly reached in this round`
        : "No stops reached in this round";
}

function updateSlider() {
    if (!vizData || allStops.length === 0) return;
    pbSlider.value = Math.max(0, revealedCount - 1);
    pbRoundLabel.textContent = `${revealedCount} / ${allStops.length}`;
}

function buildLegend() {
    roundLegend.innerHTML = "";
    if (!vizData) return;
    vizData.rounds.forEach((stops, k) => {
        if (stops.length === 0) return;
        const color = ROUND_COLORS[k] ?? ROUND_COLORS[ROUND_COLORS.length - 1];
        const row = document.createElement("div");
        row.className = "legend-row";
        row.dataset.round = k;
        row.innerHTML = `
            <span class="legend-dot" style="background:${color};color:${color}"></span>
            <span class="legend-text">${ROUND_LABELS[k] ?? "Round " + k}</span>
            <span class="legend-count">${stops.length.toLocaleString()}</span>`;
        row.addEventListener("click", () => {
            stopPlayback();
            const lastInRound = allStops.findLastIndex(s => s.roundIdx === k);
            if (lastInRound >= 0) revealUpTo(lastInRound);
        });
        roundLegend.appendChild(row);
    });
}

function updateLegendHighlight(roundIdx) {
    roundLegend.querySelectorAll(".legend-row").forEach(row => {
        row.classList.toggle("active", parseInt(row.dataset.round) === roundIdx);
    });
}

// ── startup ────────────────────────────────────────────────────────────────────

const now = new Date();
dateInput.value = now.toISOString().slice(0, 10);
timeInput.value = now.toTimeString().slice(0, 5);
setClickMode("origin");

(async () => {
    try {
        loadingText.textContent = "initializing wasm engine...";
        await init();
        $("cityHK").classList.add("active");
        await loadCity("hk");
    } catch (e) {
        loadingText.textContent = `error: ${e.message}`;
        setStatus("error", "error");
        console.error(e);
    }
})();
