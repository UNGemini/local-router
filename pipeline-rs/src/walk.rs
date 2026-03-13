/// Walk graph compression (degree-2 node contraction) and stop linking.
/// Rust port of compress_walk_graph() and link_stops_to_walk_graph() in build.py.
use crate::gtfs::{haversine, RawStop, NOT_SET};
use crate::osm::{WalkEdge, WalkNode};
use indicatif::ProgressBar;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Degree-2 contraction
// ---------------------------------------------------------------------------

pub fn compress_walk_graph(
    nodes: &[WalkNode],
    edge_list: &HashMap<u32, Vec<WalkEdge>>,
    protected_nodes: &HashSet<u32>,
) -> (Vec<WalkNode>, HashMap<u32, Vec<WalkEdge>>) {
    let n = nodes.len();

    // Build adjacency as sets (dedup parallel edges, keep shortest)
    let mut adj: Vec<HashSet<u32>> = vec![HashSet::new(); n];
    let mut edge_dist: HashMap<(u32, u32), u16> = HashMap::new();

    for ni in 0..n as u32 {
        if let Some(edges) = edge_list.get(&ni) {
            for e in edges {
                adj[ni as usize].insert(e.to_node_idx);
                let key = (ni.min(e.to_node_idx), ni.max(e.to_node_idx));
                let entry = edge_dist.entry(key).or_insert(u16::MAX);
                if e.dist_meters < *entry {
                    *entry = e.dist_meters;
                }
            }
        }
    }

    // Identify contractable (degree-2, not protected) nodes
    let contractable: HashSet<u32> = (0..n as u32)
        .filter(|&ni| adj[ni as usize].len() == 2 && !protected_nodes.contains(&ni))
        .collect();

    // Trace chains of degree-2 nodes
    let mut visited: HashSet<u32> = HashSet::new();
    let mut chains: Vec<(Vec<u32>, u32)> = Vec::new(); // (chain_nodes, total_dist_u32)

    for start in 0..n as u32 {
        if contractable.contains(&start) || visited.contains(&start) {
            continue;
        }
        // start is a junction/endpoint/protected node
        for &neighbor in &adj[start as usize] {
            if !contractable.contains(&neighbor) || visited.contains(&neighbor) {
                continue;
            }
            let mut chain: Vec<u32> = vec![start, neighbor];
            visited.insert(neighbor);
            let mut cur = neighbor;
            let mut prev = start;
            let mut total: u32 = {
                let key = (prev.min(cur), prev.max(cur));
                edge_dist.get(&key).copied().unwrap_or(0) as u32
            };
            loop {
                // cur is degree-2 contractable; find the next node
                let nexts: Vec<u32> = adj[cur as usize]
                    .iter()
                    .filter(|&&x| x != prev)
                    .copied()
                    .collect();
                if nexts.is_empty() {
                    break;
                }
                let nxt = nexts[0];
                let key = (cur.min(nxt), cur.max(nxt));
                total = total.saturating_add(edge_dist.get(&key).copied().unwrap_or(0) as u32);
                prev = cur;
                cur = nxt;
                chain.push(cur);
                if !contractable.contains(&cur) {
                    break;
                }
                visited.insert(cur);
            }
            chains.push((chain, total));
        }
    }

    // Build the new graph: keep non-contractable nodes
    let keep: Vec<u32> = (0..n as u32)
        .filter(|ni| !contractable.contains(ni))
        .collect();
    let mut old_to_new: HashMap<u32, u32> = HashMap::with_capacity(keep.len());
    for (new_i, &old_i) in keep.iter().enumerate() {
        old_to_new.insert(old_i, new_i as u32);
    }
    let new_nodes: Vec<WalkNode> = keep.iter().map(|&i| nodes[i as usize]).collect();

    // Collect chain endpoint pairs
    let chain_endpoint_pairs: HashSet<(u32, u32)> = chains
        .iter()
        .flat_map(|(chain, _)| {
            let a = chain[0];
            let b = *chain.last().unwrap();
            [(a, b), (b, a)]
        })
        .collect();

    let mut new_edge_list: HashMap<u32, Vec<WalkEdge>> = HashMap::new();

    // Add direct edges between kept nodes (not part of any chain)
    for &ni in &keep {
        let new_ni = old_to_new[&ni];
        for &to in &adj[ni as usize] {
            if contractable.contains(&to) {
                continue;
            }
            if chain_endpoint_pairs.contains(&(ni, to)) {
                continue;
            }
            if let Some(&new_to) = old_to_new.get(&to) {
                let key = (ni.min(to), ni.max(to));
                let dist = edge_dist.get(&key).copied().unwrap_or(0);
                new_edge_list.entry(new_ni).or_default().push(WalkEdge {
                    to_node_idx: new_to,
                    dist_meters: dist,
                    geometry: vec![],
                });
            }
        }
    }

    // Add chain edges with geometry
    for (chain, total_dist) in &chains {
        let a = chain[0];
        let b = *chain.last().unwrap();
        let new_a = match old_to_new.get(&a) {
            Some(&i) => i,
            None => continue,
        };
        let new_b = match old_to_new.get(&b) {
            Some(&i) => i,
            None => continue,
        };
        // geometry = intermediate nodes excluding endpoints
        let geom_fwd: Vec<(f32, f32)> = chain[1..chain.len() - 1]
            .iter()
            .map(|&i| (nodes[i as usize].lat, nodes[i as usize].lon))
            .collect();
        let geom_rev: Vec<(f32, f32)> = geom_fwd.iter().rev().copied().collect();
        let dist = (*total_dist).min(u16::MAX as u32) as u16;
        new_edge_list.entry(new_a).or_default().push(WalkEdge {
            to_node_idx: new_b,
            dist_meters: dist,
            geometry: geom_fwd,
        });
        new_edge_list.entry(new_b).or_default().push(WalkEdge {
            to_node_idx: new_a,
            dist_meters: dist,
            geometry: geom_rev,
        });
    }

    (new_nodes, new_edge_list)
}

// ---------------------------------------------------------------------------
// Stop-to-graph linking
// ---------------------------------------------------------------------------

pub fn link_stops_to_walk_graph(
    stops: &[RawStop],
    walk_nodes: &[WalkNode],
    pedestrian_node_set: Option<&HashSet<u32>>,
    pb: &ProgressBar,
) -> Vec<u32> {
    if walk_nodes.is_empty() {
        pb.inc(stops.len() as u64);
        return vec![NOT_SET; stops.len()];
    }

    const GRID_SIZE: f64 = 0.001; // ~111m cells

    let mut grid: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
    for (i, n) in walk_nodes.iter().enumerate() {
        let gx = (n.lon as f64 / GRID_SIZE) as i32;
        let gy = (n.lat as f64 / GRID_SIZE) as i32;
        grid.entry((gx, gy)).or_default().push(i as u32);
    }

    let mut result = Vec::with_capacity(stops.len());
    for s in stops {
        pb.inc(1);
        let lt = s.location_type;
        let slat = s.stop_lat as f64;
        let slon = s.stop_lon as f64;
        if lt > 2 || (slat == 0.0 && slon == 0.0) {
            result.push(NOT_SET);
            continue;
        }
        let gx = (slon / GRID_SIZE) as i32;
        let gy = (slat / GRID_SIZE) as i32;

        let mut best_ped_idx = NOT_SET;
        let mut best_ped_dist = 500.0f64;
        let mut best_any_idx = NOT_SET;
        let mut best_any_dist = 500.0f64;

        for dx in -2i32..=2 {
            for dy in -2i32..=2 {
                if let Some(cell) = grid.get(&(gx + dx, gy + dy)) {
                    for &ni in cell {
                        let n = &walk_nodes[ni as usize];
                        let d = haversine(slat, slon, n.lat as f64, n.lon as f64);
                        if d < best_any_dist {
                            best_any_dist = d;
                            best_any_idx = ni;
                        }
                        if let Some(ped_set) = pedestrian_node_set {
                            if ped_set.contains(&ni) && d < best_ped_dist {
                                best_ped_dist = d;
                                best_ped_idx = ni;
                            }
                        }
                    }
                }
            }
        }

        result.push(
            if pedestrian_node_set.is_some() && best_ped_idx != NOT_SET {
                best_ped_idx
            } else {
                best_any_idx
            },
        );
    }
    result
}
