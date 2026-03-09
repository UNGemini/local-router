// pedestrian routing using dijkstra on the osm walk graph
// used for first/last mile walking from origin/destination to transit stops

use crate::data::{WalkGraph, NOT_SET};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const WALK_SPEED_MPS: f32 = 1.2;
const EARTH_RADIUS: f32 = 6_371_000.0;

// result of a walk search: reachable node index + distance in meters + time in seconds
#[derive(Debug, Clone, Copy)]
pub struct WalkResult {
    pub node_idx: u32,
    pub dist_meters: u32,
    pub walk_seconds: u32,
}

// haversine distance in meters
pub fn haversine(lat1: f32, lon1: f32, lat2: f32, lon2: f32) -> f32 {
    let rlat1 = lat1.to_radians();
    let rlat2 = lat2.to_radians();
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + rlat1.cos() * rlat2.cos() * (dlon / 2.0).sin().powi(2);
    EARTH_RADIUS * 2.0 * a.sqrt().asin()
}

// find the nearest walk graph node to a lat/lon
pub fn nearest_node(graph: &WalkGraph, lat: f32, lon: f32) -> Option<u32> {
    if graph.nodes.is_empty() {
        return None;
    }
    let mut best_idx = 0u32;
    let mut best_dist = f32::MAX;
    for (i, n) in graph.nodes.iter().enumerate() {
        let d = haversine(lat, lon, n.lat, n.lon);
        if d < best_dist {
            best_dist = d;
            best_idx = i as u32;
        }
    }
    if best_dist > 1000.0 {
        None
    } else {
        Some(best_idx)
    }
}

// dijkstra from a single source node, up to a max distance
// returns distances in meters to all reachable nodes within max_dist
pub fn dijkstra(graph: &WalkGraph, start: u32, max_dist: u32) -> Vec<WalkResult> {
    let n = graph.nodes.len();
    let mut dist = vec![u32::MAX; n];
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
                heap.push(Reverse((new_dist, e.to_node_idx)));
            }
        }
    }

    let mut results = Vec::new();
    for (i, &d) in dist.iter().enumerate() {
        if d < u32::MAX && i != start as usize {
            results.push(WalkResult {
                node_idx: i as u32,
                dist_meters: d,
                walk_seconds: (d as f32 / WALK_SPEED_MPS) as u32,
            });
        }
    }
    results
}

// find stops reachable by walking from a lat/lon coordinate
// returns (stop_idx, walk_seconds, dist_meters) pairs
pub fn reachable_stops_from_coord(
    graph: &WalkGraph,
    stops: &[crate::data::Stop],
    lat: f32,
    lon: f32,
    max_walk_meters: u32,
) -> Vec<(u32, u32, u32)> {
    // if no walk graph, fall back to straight-line distance to stops
    if graph.nodes.is_empty() {
        return stops
            .iter()
            .enumerate()
            .filter(|(_, s)| s.location_type == 0)
            .filter_map(|(i, s)| {
                let d = haversine(lat, lon, s.lat, s.lon) as u32;
                if d <= max_walk_meters {
                    let secs = (d as f32 / WALK_SPEED_MPS) as u32;
                    Some((i as u32, secs, d))
                } else {
                    None
                }
            })
            .collect();
    }

    let start = match nearest_node(graph, lat, lon) {
        Some(n) => n,
        None => {
            // fall back to straight-line
            return stops
                .iter()
                .enumerate()
                .filter(|(_, s)| s.location_type == 0)
                .filter_map(|(i, s)| {
                    let d = haversine(lat, lon, s.lat, s.lon) as u32;
                    if d <= max_walk_meters {
                        let secs = (d as f32 / WALK_SPEED_MPS) as u32;
                        Some((i as u32, secs, d))
                    } else {
                        None
                    }
                })
                .collect();
        }
    };

    // add distance from coord to nearest node
    let sn = &graph.nodes[start as usize];
    let extra_dist = haversine(lat, lon, sn.lat, sn.lon) as u32;

    let walk_results = dijkstra(graph, start, max_walk_meters);

    // map walk nodes back to stops
    let mut reachable = Vec::new();
    for (stop_idx, stop) in stops.iter().enumerate() {
        if stop.location_type != 0 {
            continue;
        }
        if stop.walk_node_idx == NOT_SET {
            // try straight-line as fallback
            let d = haversine(lat, lon, stop.lat, stop.lon) as u32;
            if d <= max_walk_meters {
                let secs = (d as f32 / WALK_SPEED_MPS) as u32;
                reachable.push((stop_idx as u32, secs, d));
            }
            continue;
        }
        // check if this stop's walk node was reached
        for wr in &walk_results {
            if wr.node_idx == stop.walk_node_idx {
                let total_dist = wr.dist_meters + extra_dist;
                if total_dist <= max_walk_meters {
                    let secs = (total_dist as f32 / WALK_SPEED_MPS) as u32;
                    reachable.push((stop_idx as u32, secs, total_dist));
                }
                break;
            }
        }
    }
    reachable
}
