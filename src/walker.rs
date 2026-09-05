// pedestrian routing using dijkstra on the osm walk graph
// used for first/last mile walking from origin/destination to transit stops

use crate::data::{WalkEdge, WalkGraph, NOT_SET};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashSet;

pub const DEFAULT_WALK_SPEED_MPS: f32 = 1.2;
const EARTH_RADIUS: f32 = 6_371_000.0;
// grid cell size for spatial indexing (~111m per 0.001 degree)
const GRID_CELL: f32 = 0.001;

// haversine distance in meters
pub fn haversine(lat1: f32, lon1: f32, lat2: f32, lon2: f32) -> f32 {
    let rlat1 = lat1.to_radians();
    let rlat2 = lat2.to_radians();
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + rlat1.cos() * rlat2.cos() * (dlon / 2.0).sin().powi(2);
    EARTH_RADIUS * 2.0 * a.sqrt().asin()
}

fn grid_key(lat: f32, lon: f32) -> (i32, i32) {
    ((lat / GRID_CELL) as i32, (lon / GRID_CELL) as i32)
}

// find the nearest walk graph node to a lat/lon using the precomputed grid index
pub fn nearest_node(graph: &WalkGraph, lat: f32, lon: f32) -> Option<u32> {
    if graph.nodes.is_empty() {
        return None;
    }

    let (gy, gx) = grid_key(lat, lon);
    let mut best_idx = 0u32;
    let mut best_dist = f32::MAX;
    for radius in [1i32, 2, 3] {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if let Some(nodes) = graph.node_grid.get(&(gy + dy, gx + dx)) {
                    for &ni in nodes {
                        let n = &graph.nodes[ni as usize];
                        let d = haversine(lat, lon, n.lat, n.lon);
                        if d < best_dist {
                            best_dist = d;
                            best_idx = ni;
                        }
                    }
                }
            }
        }
        if best_dist < (radius as f32) * 111.0 {
            break;
        }
    }
    if best_dist > 1000.0 {
        None
    } else {
        Some(best_idx)
    }
}

// dijkstra result with parent pointers for path reconstruction
pub struct DijkstraResult {
    pub dist: Vec<u32>,
    parent: Vec<u32>,
    start: u32,
    walk_speed: f32,
}

impl DijkstraResult {
    // get (dist_meters, walk_seconds) for a node, or None if unreachable
    pub fn get(&self, node: u32) -> Option<(u32, u32)> {
        let d = self.dist[node as usize];
        if d == u32::MAX {
            None
        } else {
            let secs = (d as f32 / self.walk_speed) as u32;
            Some((d, secs))
        }
    }

    // reconstruct path from start to target as (lat, lon) coordinates
    // includes intermediate geometry from compressed edges
    pub fn path_coords(&self, graph: &WalkGraph, target: u32) -> Vec<(f32, f32)> {
        if self.dist[target as usize] == u32::MAX {
            return vec![];
        }
        // build node sequence
        let mut node_seq = vec![target];
        let mut cur = target;
        while cur != self.start {
            cur = self.parent[cur as usize];
            if cur == u32::MAX {
                return vec![];
            }
            node_seq.push(cur);
        }
        node_seq.reverse();

        // build coordinate path with edge geometry
        let mut path = Vec::new();
        for i in 0..node_seq.len() {
            let ni = node_seq[i];
            let n = &graph.nodes[ni as usize];
            path.push((n.lat, n.lon));
            // if there's a next node, find the edge and insert geometry
            if i + 1 < node_seq.len() {
                let next_ni = node_seq[i + 1];
                let wn = &graph.nodes[ni as usize];
                let edges_start = wn.edges_offset as usize;
                let edges_end = edges_start + wn.num_edges as usize;
                for e in &graph.edges[edges_start..edges_end] {
                    if e.to_node_idx == next_ni && e.geometry_len > 0 {
                        let start = e.geometry_offset as usize;
                        let end = start + e.geometry_len as usize;
                        if end <= graph.geometry.len() {
                            path.extend_from_slice(&graph.geometry[start..end]);
                        }
                        break;
                    }
                }
            }
        }
        path
    }
}

// dijkstra from a single source node, up to a max distance
// returns DijkstraResult with distances and parent pointers
pub fn dijkstra(graph: &WalkGraph, start: u32, max_dist: u32, walk_speed: f32) -> DijkstraResult {
    let n = graph.nodes.len();
    let mut dist = vec![u32::MAX; n];
    let mut parent = vec![u32::MAX; n];
    let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();

    dist[start as usize] = 0;
    heap.push(Reverse((0, start)));

    while let Some(Reverse((d, node))) = heap.pop() {
        if d > max_dist {
            break;
        }
        if d > dist[node as usize] {
            continue;
        }
        let wn = &graph.nodes[node as usize];
        let edges_start = wn.edges_offset as usize;
        let edges_end = edges_start + wn.num_edges as usize;
        for e in &graph.edges[edges_start..edges_end] {
            let new_dist = d + e.dist_meters as u32;
            if new_dist < dist[e.to_node_idx as usize] && new_dist <= max_dist {
                dist[e.to_node_idx as usize] = new_dist;
                parent[e.to_node_idx as usize] = node;
                heap.push(Reverse((new_dist, e.to_node_idx)));
            }
        }
    }

    DijkstraResult {
        dist,
        parent,
        start,
        walk_speed,
    }
}

// result of walking from a coordinate: reachable stops + dijkstra state for path extraction
pub struct WalkReach {
    // (stop_idx, walk_seconds, dist_meters) for each reachable stop
    pub stops: Vec<(u32, u32, u32)>,
    // the dijkstra result, if a walk graph was used (None = straight-line fallback)
    dijk: Option<DijkstraResult>,
    // the nearest graph node to the source coordinate
    #[allow(dead_code)]
    start_node: Option<u32>,
}

impl WalkReach {
    // get the walk path coordinates from the source to a specific graph node
    pub fn path_to_node(&self, graph: &WalkGraph, node_idx: u32) -> Vec<(f32, f32)> {
        match &self.dijk {
            Some(d) => d.path_coords(graph, node_idx),
            None => vec![],
        }
    }

    // get the walk path to a stop (looks up the stop's walk_node_idx)
    // for stops without a walk_node_idx, falls back to nearest graph node
    pub fn path_to_stop(
        &self,
        graph: &WalkGraph,
        stops: &[crate::data::Stop],
        stop_idx: u32,
    ) -> Vec<(f32, f32)> {
        let stop = &stops[stop_idx as usize];
        if stop.walk_node_idx != NOT_SET {
            return self.path_to_node(graph, stop.walk_node_idx);
        }
        // stop has no walk graph node - find nearest graph node as fallback
        if let Some(near) = nearest_node(graph, stop.lat, stop.lon) {
            let path = self.path_to_node(graph, near);
            if !path.is_empty() {
                return path;
            }
        }
        vec![]
    }
}

// compute walking distance and path between two coordinates via the walk graph
// returns (dist_meters, walk_seconds, path_coords) or None if not reachable
pub fn walk_between(
    graph: &WalkGraph,
    lat1: f32,
    lon1: f32,
    lat2: f32,
    lon2: f32,
    max_dist: u32,
    walk_speed: f32,
) -> Option<(u32, u32, Vec<(f32, f32)>)> {
    let straight = haversine(lat1, lon1, lat2, lon2) as u32;
    if straight > max_dist {
        return None;
    }

    if graph.nodes.is_empty() {
        let secs = (straight as f32 / walk_speed) as u32;
        return Some((straight, secs, vec![(lat1, lon1), (lat2, lon2)]));
    }

    let start = nearest_node(graph, lat1, lon1)?;
    let end = nearest_node(graph, lat2, lon2)?;

    let sn = &graph.nodes[start as usize];
    let en = &graph.nodes[end as usize];
    let extra_start = haversine(lat1, lon1, sn.lat, sn.lon) as u32;
    let extra_end = haversine(lat2, lon2, en.lat, en.lon) as u32;

    // graph distance is typically 1.3-1.5x haversine, so search wider
    let search_radius = max_dist + max_dist / 2;
    let dijk = dijkstra(graph, start, search_radius, walk_speed);
    if let Some((dist_m, _)) = dijk.get(end) {
        let total = dist_m + extra_start + extra_end;
        if total <= search_radius {
            let secs = (total as f32 / walk_speed) as u32;
            let mut path = vec![(lat1, lon1)];
            let graph_path = dijk.path_coords(graph, end);
            path.extend_from_slice(&graph_path);
            path.push((lat2, lon2));
            return Some((total, secs, path));
        }
    }

    // graph didn't connect them; fall back to straight-line
    let secs = (straight as f32 / walk_speed) as u32;
    Some((straight, secs, vec![(lat1, lon1), (lat2, lon2)]))
}

// walk between two stops using the walk graph, returns path coordinates
// used for transfer walks between consecutive transit legs
pub fn walk_between_stops(
    graph: &WalkGraph,
    stops: &[crate::data::Stop],
    from_idx: u32,
    to_idx: u32,
    walk_speed: f32,
) -> Vec<(f32, f32)> {
    let from = &stops[from_idx as usize];
    let to = &stops[to_idx as usize];
    if graph.nodes.is_empty() {
        return vec![(from.lat, from.lon), (to.lat, to.lon)];
    }
    // resolve walk graph nodes, falling back to nearest node if NOT_SET
    let from_node = if from.walk_node_idx != NOT_SET {
        Some(from.walk_node_idx)
    } else {
        nearest_node(graph, from.lat, from.lon)
    };
    let to_node = if to.walk_node_idx != NOT_SET {
        Some(to.walk_node_idx)
    } else {
        nearest_node(graph, to.lat, to.lon)
    };
    let (Some(fn_idx), Some(tn_idx)) = (from_node, to_node) else {
        return vec![(from.lat, from.lon), (to.lat, to.lon)];
    };
    // short dijkstra between the two walk nodes (transfers are typically <500m)
    let dijk = dijkstra(graph, fn_idx, 2000, walk_speed);
    if dijk.dist[tn_idx as usize] == u32::MAX {
        return vec![(from.lat, from.lon), (to.lat, to.lon)];
    }
    let mut path = vec![(from.lat, from.lon)];
    path.extend_from_slice(&dijk.path_coords(graph, tn_idx));
    path.push((to.lat, to.lon));
    path
}

// find stops reachable by walking from a lat/lon coordinate
// returns WalkReach with reachable stops and path reconstruction capability
pub fn reachable_stops_from_coord(
    graph: &WalkGraph,
    stops: &[crate::data::Stop],
    lat: f32,
    lon: f32,
    max_walk_meters: u32,
    walk_speed: f32,
) -> WalkReach {
    // if no walk graph, fall back to straight-line distance to stops
    if graph.nodes.is_empty() {
        let stops_vec = stops
            .iter()
            .enumerate()
            .filter(|(_, s)| s.location_type == 0)
            .filter_map(|(i, s)| {
                let d = haversine(lat, lon, s.lat, s.lon) as u32;
                if d <= max_walk_meters {
                    let secs = (d as f32 / walk_speed) as u32;
                    Some((i as u32, secs, d))
                } else {
                    None
                }
            })
            .collect();
        return WalkReach {
            stops: stops_vec,
            dijk: None,
            start_node: None,
        };
    }

    let start = match nearest_node(graph, lat, lon) {
        Some(n) => n,
        None => {
            let stops_vec = stops
                .iter()
                .enumerate()
                .filter(|(_, s)| s.location_type == 0)
                .filter_map(|(i, s)| {
                    let d = haversine(lat, lon, s.lat, s.lon) as u32;
                    if d <= max_walk_meters {
                        let secs = (d as f32 / walk_speed) as u32;
                        Some((i as u32, secs, d))
                    } else {
                        None
                    }
                })
                .collect();
            return WalkReach {
                stops: stops_vec,
                dijk: None,
                start_node: None,
            };
        }
    };

    let sn = &graph.nodes[start as usize];
    let extra_dist = haversine(lat, lon, sn.lat, sn.lon) as u32;

    let dijk = dijkstra(graph, start, max_walk_meters, walk_speed);

    let mut reachable = Vec::new();
    for (stop_idx, stop) in stops.iter().enumerate() {
        if stop.location_type != 0 {
            continue;
        }
        if stop.walk_node_idx == NOT_SET {
            let d = haversine(lat, lon, stop.lat, stop.lon) as u32;
            if d <= max_walk_meters {
                let secs = (d as f32 / walk_speed) as u32;
                reachable.push((stop_idx as u32, secs, d));
            }
            continue;
        }
        if let Some((dist_m, _)) = dijk.get(stop.walk_node_idx) {
            let total_dist = dist_m + extra_dist;
            if total_dist <= max_walk_meters {
                let secs = (total_dist as f32 / walk_speed) as u32;
                reachable.push((stop_idx as u32, secs, total_dist));
            }
        }
    }

    WalkReach {
        stops: reachable,
        dijk: Some(dijk),
        start_node: Some(start),
    }
}

// ── Road assistant extensions ────────────────────────────────────────────────
// Snap + waypoint routing over the OSM walk/street graph. Exposed to wasm so
// the app's road assistant (follow roads / recalculate) works fully locally;
// the online OSRM proxy stays as the fallback.

/// A point the route must keep away from (junction blocker).
pub struct Blocker {
    pub lat: f32,
    pub lon: f32,
    pub radius_m: f32,
}

impl Blocker {
    fn hits(&self, lat: f32, lon: f32) -> bool {
        haversine(lat, lon, self.lat, self.lon) <= self.radius_m
    }
}

/// project (lat, lon) onto segment a→b in local equirectangular metres
/// returns (dist_m, lat, lon) of the closest point
fn project_segment(
    lat: f32,
    lon: f32,
    alat: f32,
    alon: f32,
    blat: f32,
    blon: f32,
) -> (f32, f32, f32) {
    let mlat = ((lat + alat + blat) / 3.0).to_radians();
    let m_per_deg_lat = 111_320.0f32;
    let m_per_deg_lon = 111_320.0 * mlat.cos();
    let px = (lon - alon) * m_per_deg_lon;
    let py = (lat - alat) * m_per_deg_lat;
    let bx = (blon - alon) * m_per_deg_lon;
    let by = (blat - alat) * m_per_deg_lat;
    let len2 = bx * bx + by * by;
    let t = if len2 < 1e-9 {
        0.0
    } else {
        ((px * bx + py * by) / len2).clamp(0.0, 1.0)
    };
    let dx = px - t * bx;
    let dy = py - t * by;
    let dist = (dx * dx + dy * dy).sqrt();
    (dist, alat + t * (blat - alat), alon + t * (blon - alon))
}

/// result of snapping a coordinate onto the walk/street graph
pub struct SnapResult {
    pub lat: f32,
    pub lon: f32,
    /// distance from the query point to the snapped position (metres)
    pub dist_m: f32,
}

/// Snap a coordinate onto the nearest walk/street edge (endpoints + packed
/// geometry), scanning spatial-grid cells outward like `nearest_node`.
pub fn snap_to_graph(
    graph: &WalkGraph,
    lat: f32,
    lon: f32,
    max_dist_m: f32,
) -> Option<SnapResult> {
    if graph.nodes.is_empty() {
        return None;
    }
    let (gy, gx) = grid_key(lat, lon);
    let mut best: Option<SnapResult> = None;
    let mut seen_edges: HashSet<(u32, u32)> = HashSet::new();
    for radius in [1i32, 2, 4, 8] {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let cells = match graph.node_grid.get(&(gy + dy, gx + dx)) {
                    Some(n) => n,
                    None => continue,
                };
                for &ni in cells {
                    let n = &graph.nodes[ni as usize];
                    let d = haversine(lat, lon, n.lat, n.lon);
                    if d <= max_dist_m && best.as_ref().map_or(true, |b| d < b.dist_m) {
                        best = Some(SnapResult {
                            lat: n.lat,
                            lon: n.lon,
                            dist_m: d,
                        });
                    }
                    // project onto incident edge geometry (each edge once)
                    let edges_start = n.edges_offset as usize;
                    let edges_end = edges_start + n.num_edges as usize;
                    for e in &graph.edges[edges_start..edges_end] {
                        let key = (ni.min(e.to_node_idx), ni.max(e.to_node_idx));
                        if !seen_edges.insert(key) {
                            continue;
                        }
                        let to = &graph.nodes[e.to_node_idx as usize];
                        let mut prev = (n.lat, n.lon);
                        let gstart = e.geometry_offset as usize;
                        let gend =
                            (gstart + e.geometry_len as usize).min(graph.geometry.len());
                        for &(glat, glon) in &graph.geometry[gstart..gend] {
                            let (d2, clat, clon) =
                                project_segment(lat, lon, prev.0, prev.1, glat, glon);
                            if d2 <= max_dist_m
                                && best.as_ref().map_or(true, |b| d2 < b.dist_m)
                            {
                                best = Some(SnapResult {
                                    lat: clat,
                                    lon: clon,
                                    dist_m: d2,
                                });
                            }
                            prev = (glat, glon);
                        }
                        let (d2, clat, clon) =
                            project_segment(lat, lon, prev.0, prev.1, to.lat, to.lon);
                        if d2 <= max_dist_m && best.as_ref().map_or(true, |b| d2 < b.dist_m) {
                            best = Some(SnapResult {
                                lat: clat,
                                lon: clon,
                                dist_m: d2,
                            });
                        }
                    }
                }
            }
        }
        // tight snap inside the scanned band — stop widening the search
        if let Some(b) = &best {
            if b.dist_m < 25.0 {
                break;
            }
        }
    }
    best
}

/// does this edge (endpoints + packed geometry) pass near any blocker?
fn edge_blocked(graph: &WalkGraph, from_node: u32, e: &WalkEdge, blockers: &[Blocker]) -> bool {
    if blockers.is_empty() {
        return false;
    }
    let from = &graph.nodes[from_node as usize];
    let to = &graph.nodes[e.to_node_idx as usize];
    for b in blockers {
        if b.hits(from.lat, from.lon) || b.hits(to.lat, to.lon) {
            return true;
        }
    }
    if e.geometry_len > 0 {
        let gstart = e.geometry_offset as usize;
        let gend = (gstart + e.geometry_len as usize).min(graph.geometry.len());
        for &(glat, glon) in &graph.geometry[gstart..gend] {
            for b in blockers {
                if b.hits(glat, glon) {
                    return true;
                }
            }
        }
    }
    false
}

/// dijkstra that skips edges passing near any blocker
pub fn dijkstra_avoiding(
    graph: &WalkGraph,
    start: u32,
    max_dist: u32,
    walk_speed: f32,
    blockers: &[Blocker],
) -> DijkstraResult {
    let n = graph.nodes.len();
    let mut dist = vec![u32::MAX; n];
    let mut parent = vec![u32::MAX; n];
    let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();
    dist[start as usize] = 0;
    heap.push(Reverse((0, start)));
    while let Some(Reverse((d, node))) = heap.pop() {
        if d > max_dist {
            break;
        }
        if d > dist[node as usize] {
            continue;
        }
        let wn = &graph.nodes[node as usize];
        let edges_start = wn.edges_offset as usize;
        let edges_end = edges_start + wn.num_edges as usize;
        for e in &graph.edges[edges_start..edges_end] {
            if edge_blocked(graph, node, e, blockers) {
                continue;
            }
            let new_dist = d + e.dist_meters as u32;
            if new_dist < dist[e.to_node_idx as usize] && new_dist <= max_dist {
                dist[e.to_node_idx as usize] = new_dist;
                parent[e.to_node_idx as usize] = node;
                heap.push(Reverse((new_dist, e.to_node_idx)));
            }
        }
    }
    DijkstraResult {
        dist,
        parent,
        start,
        walk_speed,
    }
}

/// route along the walk/street graph through waypoints, keeping away from
/// blockers. Each consecutive pair is a radius-bounded dijkstra; contracted
/// edge geometry is included in the result. None when any leg fails.
pub fn route_waypoints(
    graph: &WalkGraph,
    waypoints: &[(f32, f32)],
    blockers: &[Blocker],
    max_leg_m: u32,
) -> Option<Vec<(f32, f32)>> {
    if waypoints.len() < 2 || graph.nodes.is_empty() {
        return None;
    }
    let mut out: Vec<(f32, f32)> = vec![waypoints[0]];
    for w in waypoints.windows(2) {
        let (lat1, lon1) = w[0];
        let (lat2, lon2) = w[1];
        let start = nearest_node(graph, lat1, lon1)?;
        let end = nearest_node(graph, lat2, lon2)?;
        // graph detours run ~1.3–1.5× haversine — search wider
        let search = max_leg_m.saturating_add(max_leg_m / 2).min(40_000);
        let dijk = if blockers.is_empty() {
            dijkstra(graph, start, search, DEFAULT_WALK_SPEED_MPS)
        } else {
            dijkstra_avoiding(graph, start, search, DEFAULT_WALK_SPEED_MPS, blockers)
        };
        if dijk.dist[end as usize] == u32::MAX {
            return None;
        }
        let mut coords = dijk.path_coords(graph, end);
        if coords.is_empty() {
            return None;
        }
        coords.insert(0, (lat1, lon1));
        coords.push((lat2, lon2));
        for c in coords {
            match out.last() {
                Some(last) if haversine(last.0, last.1, c.0, c.1) < 0.5 => continue,
                _ => out.push(c),
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod road_tests {
    use super::*;
    use crate::data::{WalkEdge, WalkGraph, WalkNode};
    use std::collections::HashMap;

    /// two parallel E-W roads linked in the middle:
    ///   A(22.300,114.160) - B(22.300,114.165) - C(22.300,114.170)
    ///   D(22.304,114.165) - E(22.304,114.170), connector B-D
    fn build_graph() -> WalkGraph {
        let mut nodes = vec![
            WalkNode { lat: 22.300, lon: 114.160, edges_offset: 0, num_edges: 0 },
            WalkNode { lat: 22.300, lon: 114.165, edges_offset: 0, num_edges: 0 },
            WalkNode { lat: 22.300, lon: 114.170, edges_offset: 0, num_edges: 0 },
            WalkNode { lat: 22.304, lon: 114.165, edges_offset: 0, num_edges: 0 },
            WalkNode { lat: 22.304, lon: 114.170, edges_offset: 0, num_edges: 0 },
        ];
        let pairs: Vec<(usize, usize)> = vec![(0, 1), (1, 2), (1, 3), (3, 4)];
        let mut adj: Vec<Vec<WalkEdge>> = vec![Vec::new(); nodes.len()];
        for (a, b) in &pairs {
            let d =
                haversine(nodes[*a].lat, nodes[*a].lon, nodes[*b].lat, nodes[*b].lon) as u16;
            adj[*a].push(WalkEdge {
                to_node_idx: *b as u32,
                dist_meters: d,
                geometry_offset: 0,
                geometry_len: 0,
            });
            adj[*b].push(WalkEdge {
                to_node_idx: *a as u32,
                dist_meters: d,
                geometry_offset: 0,
                geometry_len: 0,
            });
        }
        let mut flat: Vec<WalkEdge> = Vec::new();
        for (i, list) in adj.iter().enumerate() {
            nodes[i].edges_offset = flat.len() as u32;
            nodes[i].num_edges = list.len() as u16;
            flat.extend_from_slice(list);
        }
        let mut node_grid: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            node_grid.entry(grid_key(n.lat, n.lon)).or_default().push(i as u32);
        }
        WalkGraph {
            nodes,
            edges: flat,
            geometry: vec![],
            node_grid,
        }
    }

    #[test]
    fn snap_finds_nearby_edge() {
        let g = build_graph();
        let s = snap_to_graph(&g, 22.3005, 114.1625, 200.0)
            .expect("snap should land on the A-B road");
        assert!(s.dist_m < 60.0, "dist {}", s.dist_m);
    }

    #[test]
    fn route_waypoints_follows_roads() {
        let g = build_graph();
        let path = route_waypoints(&g, &[(22.300, 114.160), (22.300, 114.170)], &[], 4000)
            .expect("A to C should route");
        assert!(path.len() >= 3);
    }

    #[test]
    fn route_avoids_blocker() {
        let g = build_graph();
        // blocker on node C kills every path touching C
        let blockers = vec![Blocker {
            lat: 22.300,
            lon: 114.170,
            radius_m: 80.0,
        }];
        assert!(route_waypoints(&g, &[(22.300, 114.160), (22.300, 114.170)], &blockers, 4000)
            .is_none());
        // north detour A-B-D-E still exists and never touches C
        let detour =
            route_waypoints(&g, &[(22.300, 114.160), (22.304, 114.170)], &blockers, 8000)
                .expect("A to E should detour via D");
        assert!(detour
            .iter()
            .any(|&(la, lo)| (la - 22.304).abs() < 0.001 && (lo - 114.165).abs() < 0.001));
        assert!(detour
            .iter()
            .all(|&(la, lo)| !((la - 22.300).abs() < 0.0005 && (lo - 114.170).abs() < 0.0005)));
    }

    #[test]
    fn route_respects_oneway() {
        // one-way street A -> B only
        let dist = haversine(22.300, 114.160, 22.300, 114.170) as u16;
        let nodes = vec![
            WalkNode { lat: 22.300, lon: 114.160, edges_offset: 0, num_edges: 1 },
            WalkNode { lat: 22.300, lon: 114.170, edges_offset: 0, num_edges: 0 },
        ];
        let edges = vec![WalkEdge {
            to_node_idx: 1,
            dist_meters: dist,
            geometry_offset: 0,
            geometry_len: 0,
        }];
        let mut node_grid: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            node_grid.entry(grid_key(n.lat, n.lon)).or_default().push(i as u32);
        }
        let g = WalkGraph { nodes, edges, geometry: vec![], node_grid };
        assert!(route_waypoints(&g, &[(22.300, 114.160), (22.300, 114.170)], &[], 4000).is_some());
        assert!(route_waypoints(&g, &[(22.300, 114.170), (22.300, 114.160)], &[], 4000).is_none());
    }
}
