/// OSM walking graph extraction.
/// Supports both PBF (via `osmpbf`) and OSM XML (via `quick-xml`).
/// Matches the semantics of build_walk_graph() in pipeline/build.py.
use crate::gtfs::haversine;
use anyhow::{Context, Result};
use osmpbf::{Element, ElementReader};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::Path;

// Highway tags that are walkable
const PEDESTRIAN_PRIORITY: &[&str] = &[
    "footway",
    "pedestrian",
    "path",
    "steps",
    "living_street",
    "cycleway",
];
const ROAD_CLASS: &[&str] = &[
    "residential",
    "service",
    "tertiary",
    "secondary",
    "primary",
    "trunk",
    "unclassified",
    "track",
];
const ROAD_PENALTY: u32 = 3;

fn is_pedestrian_priority(hw: &str) -> bool {
    PEDESTRIAN_PRIORITY.contains(&hw)
}
fn is_road_class(hw: &str) -> bool {
    ROAD_CLASS.contains(&hw)
}
fn is_walkable(hw: &str) -> bool {
    is_pedestrian_priority(hw) || is_road_class(hw)
}

/// highway classes a bus may travel on (directed road graph)
const VEHICLE_CLASS: &[&str] = &[
    "motorway",
    "motorway_link",
    "trunk",
    "trunk_link",
    "primary",
    "primary_link",
    "secondary",
    "secondary_link",
    "tertiary",
    "tertiary_link",
    "unclassified",
    "residential",
    "living_street",
    "service",
    "busway",
];

fn is_vehicle_class(hw: &str) -> bool {
    VEHICLE_CLASS.contains(&hw)
}

/// A raw OSM way we care about.
struct WalkWay {
    node_refs: Vec<i64>,
    is_pedestrian: bool,
    penalty: u32,
    /// vehicle-usable road (directed road graph source)
    is_vehicle: bool,
    /// 1 = forward only, -1 = reverse only, 0 = both directions
    oneway: i8,
}

/// Walk graph node.
#[derive(Clone, Copy)]
pub struct WalkNode {
    pub lat: f32,
    pub lon: f32,
}

/// Walk graph edge with optional geometry (for compressed edges).
#[derive(Clone)]
pub struct WalkEdge {
    pub to_node_idx: u32,
    pub dist_meters: u16,
    pub geometry: Vec<(f32, f32)>, // intermediate coord pairs
}

/// Detect if a file is OSM XML (vs PBF) by extension or magic bytes.
fn is_osm_xml(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if ext == "xml" || ext == "osm" {
            return true;
        }
        if ext == "pbf" {
            return false;
        }
    }
    if let Ok(f) = std::fs::File::open(path) {
        let mut buf = [0u8; 1];
        use std::io::Read;
        if BufReader::new(f).read_exact(&mut buf).is_ok() {
            return buf[0] == b'<';
        }
    }
    false
}

/// Load node_coords and walkable ways from OSM XML.
fn load_osm_xml(path: &Path) -> Result<(HashMap<i64, (f32, f32)>, Vec<WalkWay>)> {
    let file =
        std::fs::File::open(path).with_context(|| format!("open OSM XML: {}", path.display()))?;
    let buf = BufReader::new(file);
    let mut reader = Reader::from_reader(buf);
    reader.config_mut().trim_text(true);

    let mut node_coords: HashMap<i64, (f32, f32)> = HashMap::new();
    let mut ways: Vec<WalkWay> = Vec::new();

    // State for current <way> element
    let mut in_way = false;
    let mut way_node_refs: Vec<i64> = Vec::new();
    let mut way_highway: Option<String> = None;
    let mut way_foot_no = false;
    let mut way_access_private = false;
    let mut way_oneway: i8 = 0;
    let mut way_motor_no = false;

    let mut buf_ev = Vec::new();
    loop {
        match reader.read_event_into(&mut buf_ev) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => match e.name().as_ref() {
                b"node" => {
                    let mut id: Option<i64> = None;
                    let mut lat: Option<f32> = None;
                    let mut lon: Option<f32> = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => {
                                id = std::str::from_utf8(&attr.value)
                                    .ok()
                                    .and_then(|s| s.parse().ok());
                            }
                            b"lat" => {
                                lat = std::str::from_utf8(&attr.value)
                                    .ok()
                                    .and_then(|s| s.parse().ok());
                            }
                            b"lon" => {
                                lon = std::str::from_utf8(&attr.value)
                                    .ok()
                                    .and_then(|s| s.parse().ok());
                            }
                            _ => {}
                        }
                    }
                    if let (Some(id), Some(lat), Some(lon)) = (id, lat, lon) {
                        node_coords.insert(id, (lat, lon));
                    }
                }
                b"way" => {
                    in_way = true;
                    way_node_refs.clear();
                    way_highway = None;
                    way_foot_no = false;
                    way_access_private = false;
                    way_oneway = 0;
                    way_motor_no = false;
                }
                b"nd" if in_way => {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"ref" {
                            if let Ok(s) = std::str::from_utf8(&attr.value) {
                                if let Ok(id) = s.parse::<i64>() {
                                    way_node_refs.push(id);
                                }
                            }
                        }
                    }
                }
                b"tag" if in_way => {
                    let mut k = String::new();
                    let mut v = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"k" => {
                                k = std::str::from_utf8(&attr.value).unwrap_or("").to_string();
                            }
                            b"v" => {
                                v = std::str::from_utf8(&attr.value).unwrap_or("").to_string();
                            }
                            _ => {}
                        }
                    }
                    if k == "highway" {
                        way_highway = Some(v);
                    } else if k == "foot" && v == "no" {
                        way_foot_no = true;
                    } else if k == "access" && v == "private" {
                        way_access_private = true;
                    } else if k == "oneway" {
                        way_oneway = match v.as_str() {
                            "yes" | "1" | "true" => 1,
                            "-1" => -1,
                            _ => 0,
                        };
                    } else if k == "junction" && v == "roundabout" {
                        way_oneway = 1;
                    } else if k == "motor_vehicle" && v == "no" {
                        way_motor_no = true;
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"way" && in_way {
                    in_way = false;
                    if let Some(hw) = way_highway.take() {
                        if is_walkable(&hw) && !way_foot_no && !way_access_private {
                            let is_ped = is_pedestrian_priority(&hw);
                            let penalty = if is_ped { 1 } else { ROAD_PENALTY };
                            let is_vehicle = is_vehicle_class(&hw) && !way_motor_no;
                            if way_node_refs.len() >= 2 {
                                ways.push(WalkWay {
                                    node_refs: way_node_refs.clone(),
                                    is_pedestrian: is_ped,
                                    penalty,
                                    is_vehicle,
                                    oneway: way_oneway,
                                });
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "XML parse error at position {}: {}",
                    reader.buffer_position(),
                    e
                ))
            }
            _ => {}
        }
        buf_ev.clear();
    }
    Ok((node_coords, ways))
}

/// Load node_coords and walkable ways from OSM PBF.
fn load_osm_pbf(path: &Path) -> Result<(HashMap<i64, (f32, f32)>, Vec<WalkWay>)> {
    let reader = ElementReader::from_path(path)?;
    let mut node_coords: HashMap<i64, (f32, f32)> = HashMap::new();
    let mut ways: Vec<WalkWay> = Vec::new();
    reader.for_each(|element| {
        match element {
            Element::Node(n) => {
                node_coords.insert(n.id(), (n.lat() as f32, n.lon() as f32));
            }
            Element::DenseNode(n) => {
                node_coords.insert(n.id(), (n.lat() as f32, n.lon() as f32));
            }
            Element::Way(w) => {
                let mut hw: Option<String> = None;
                let mut oneway: i8 = 0;
                let mut motor_no = false;
                let mut foot_no = false;
                let mut access_private = false;
                for (k, v) in w.tags() {
                    match k {
                        "highway" => hw = Some(v.to_string()),
                        "oneway" => {
                            oneway = match v {
                                "yes" | "1" | "true" => 1,
                                "-1" => -1,
                                _ => 0,
                            }
                        }
                        "junction" if v == "roundabout" => oneway = 1,
                        "motor_vehicle" if v == "no" => motor_no = true,
                        "foot" if v == "no" => foot_no = true,
                        "access" if v == "private" => access_private = true,
                        _ => {}
                    }
                }
                let hw = hw.as_deref().unwrap_or("");
                if !is_walkable(hw) || foot_no || access_private {
                    return;
                }
                let is_ped = is_pedestrian_priority(hw);
                let penalty = if is_ped { 1 } else { ROAD_PENALTY };
                let node_refs: Vec<i64> = w.refs().collect();
                if node_refs.len() >= 2 {
                    ways.push(WalkWay {
                        node_refs,
                        is_pedestrian: is_ped,
                        penalty,
                        is_vehicle: is_vehicle_class(hw) && !motor_no,
                        oneway,
                    });
                }
            }
            _ => {}
        }
    })?;
    Ok((node_coords, ways))
}

pub fn build_walk_graph(
    osm_path: &Path,
    stop_coords: &[(f64, f64)],
    max_walk_radius: f64,
) -> Result<(Vec<WalkNode>, HashMap<u32, Vec<WalkEdge>>, HashSet<u32>)> {
    // --- Pass 1: collect all node coords and walkable ways ---
    let (node_coords, ways) = if is_osm_xml(osm_path) {
        load_osm_xml(osm_path)?
    } else {
        load_osm_pbf(osm_path)?
    };

    // Build compact node index (only nodes referenced by walkable ways)
    let mut used_nodes: HashSet<i64> = HashSet::new();
    for way in &ways {
        for &nref in &way.node_refs {
            used_nodes.insert(nref);
        }
    }

    // Sort node IDs for deterministic indexing
    let mut sorted_nids: Vec<i64> = used_nodes.into_iter().collect();
    sorted_nids.sort_unstable();

    let mut node_idx_map: HashMap<i64, u32> = HashMap::with_capacity(sorted_nids.len());
    let mut nodes: Vec<WalkNode> = Vec::with_capacity(sorted_nids.len());
    for nid in &sorted_nids {
        if let Some(&(lat, lon)) = node_coords.get(nid) {
            node_idx_map.insert(*nid, nodes.len() as u32);
            nodes.push(WalkNode { lat, lon });
        }
    }

    // Build edge list and track pedestrian nodes
    let mut edge_list: HashMap<u32, Vec<(u32, u16)>> = HashMap::new();
    let mut pedestrian_node_set: HashSet<u32> = HashSet::new();

    for way in &ways {
        for k in 0..way.node_refs.len() - 1 {
            let a = way.node_refs[k];
            let b = way.node_refs[k + 1];
            let ai = match node_idx_map.get(&a) {
                Some(&i) => i,
                None => continue,
            };
            let bi = match node_idx_map.get(&b) {
                Some(&i) => i,
                None => continue,
            };
            let na = &nodes[ai as usize];
            let nb = &nodes[bi as usize];
            let raw_dist = haversine(na.lat as f64, na.lon as f64, nb.lat as f64, nb.lon as f64);
            let dist = (raw_dist * way.penalty as f64).min(u16::MAX as f64) as u16;
            edge_list.entry(ai).or_default().push((bi, dist));
            edge_list.entry(bi).or_default().push((ai, dist));
            if way.is_pedestrian {
                pedestrian_node_set.insert(ai);
                pedestrian_node_set.insert(bi);
            }
        }
    }

    // --- Prune nodes far from any transit stop ---
    if !stop_coords.is_empty() {
        const GRID_SIZE: f64 = 0.005; // ~550m
        let mut stop_grid: HashMap<(i32, i32), Vec<(f64, f64)>> = HashMap::new();
        for &(slat, slon) in stop_coords {
            let gx = (slon / GRID_SIZE) as i32;
            let gy = (slat / GRID_SIZE) as i32;
            stop_grid.entry((gx, gy)).or_default().push((slat, slon));
        }
        let radius_cells = ((max_walk_radius / 550.0).ceil() as i32).max(1);

        let mut keep: HashSet<u32> = HashSet::new();
        for (ni, n) in nodes.iter().enumerate() {
            let nlat = n.lat as f64;
            let nlon = n.lon as f64;
            let gx = (nlon / GRID_SIZE) as i32;
            let gy = (nlat / GRID_SIZE) as i32;
            'outer: for dx in -radius_cells..=radius_cells {
                for dy in -radius_cells..=radius_cells {
                    if let Some(cell) = stop_grid.get(&(gx + dx, gy + dy)) {
                        for &(slat, slon) in cell {
                            if haversine(nlat, nlon, slat, slon) <= max_walk_radius {
                                keep.insert(ni as u32);
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }

        if keep.len() < nodes.len() {
            // Reindex to only kept nodes
            let mut sorted_keep: Vec<u32> = keep.into_iter().collect();
            sorted_keep.sort_unstable();
            let mut old_to_new: HashMap<u32, u32> = HashMap::with_capacity(sorted_keep.len());
            let mut new_nodes: Vec<WalkNode> = Vec::with_capacity(sorted_keep.len());
            for &old_i in &sorted_keep {
                old_to_new.insert(old_i, new_nodes.len() as u32);
                new_nodes.push(nodes[old_i as usize]);
            }
            let mut new_edge_list: HashMap<u32, Vec<(u32, u16)>> = HashMap::new();
            for &old_i in &sorted_keep {
                if let Some(edges) = edge_list.get(&old_i) {
                    let new_i = old_to_new[&old_i];
                    for &(old_to, dist) in edges {
                        if let Some(&new_to) = old_to_new.get(&old_to) {
                            new_edge_list.entry(new_i).or_default().push((new_to, dist));
                        }
                    }
                }
            }
            let new_ped_set: HashSet<u32> = pedestrian_node_set
                .iter()
                .filter_map(|old_i| old_to_new.get(old_i).copied())
                .collect();
            nodes = new_nodes;
            // Convert to WalkEdge (no geometry yet — added by compress step)
            let final_edges: HashMap<u32, Vec<WalkEdge>> = new_edge_list
                .into_iter()
                .map(|(ni, edges)| {
                    let we: Vec<WalkEdge> = edges
                        .into_iter()
                        .map(|(to, dist)| WalkEdge {
                            to_node_idx: to,
                            dist_meters: dist,
                            geometry: vec![],
                        })
                        .collect();
                    (ni, we)
                })
                .collect();
            return Ok((nodes, final_edges, new_ped_set));
        }
    }

    // Convert edge_list to WalkEdge
    let final_edges: HashMap<u32, Vec<WalkEdge>> = edge_list
        .into_iter()
        .map(|(ni, edges)| {
            let we: Vec<WalkEdge> = edges
                .into_iter()
                .map(|(to, dist)| WalkEdge {
                    to_node_idx: to,
                    dist_meters: dist,
                    geometry: vec![],
                })
                .collect();
            (ni, we)
        })
        .collect();

    Ok((nodes, final_edges, pedestrian_node_set))
}


/// Directed vehicle road graph (oneway-aware, pedestrian ways excluded).
/// Same shape as the walk graph so routing code is shared; edges are stored
/// one-way per OSM direction, one edge per OSM node pair (no contraction —
/// the dense segmentation is what gives the road assistant its precision).
pub fn build_road_graph(
    osm_path: &Path,
    stop_coords: &[(f64, f64)],
    max_radius: f64,
) -> Result<(Vec<WalkNode>, HashMap<u32, Vec<WalkEdge>>)> {
    let (node_coords, ways) = if is_osm_xml(osm_path) {
        load_osm_xml(osm_path)?
    } else {
        load_osm_pbf(osm_path)?
    };

    let mut used_nodes: HashSet<i64> = HashSet::new();
    for way in ways.iter().filter(|w| w.is_vehicle) {
        for &nref in &way.node_refs {
            used_nodes.insert(nref);
        }
    }
    let mut sorted_nids: Vec<i64> = used_nodes.into_iter().collect();
    sorted_nids.sort_unstable();
    let mut node_idx_map: HashMap<i64, u32> = HashMap::with_capacity(sorted_nids.len());
    let mut nodes: Vec<WalkNode> = Vec::with_capacity(sorted_nids.len());
    for nid in &sorted_nids {
        if let Some(&(lat, lon)) = node_coords.get(nid) {
            node_idx_map.insert(*nid, nodes.len() as u32);
            nodes.push(WalkNode { lat, lon });
        }
    }

    let mut edge_list: HashMap<u32, Vec<(u32, u16)>> = HashMap::new();
    for way in ways.iter().filter(|w| w.is_vehicle) {
        for k in 0..way.node_refs.len() - 1 {
            let a = way.node_refs[k];
            let b = way.node_refs[k + 1];
            let ai = match node_idx_map.get(&a) {
                Some(&i) => i,
                None => continue,
            };
            let bi = match node_idx_map.get(&b) {
                Some(&i) => i,
                None => continue,
            };
            let na = &nodes[ai as usize];
            let nb = &nodes[bi as usize];
            let dist = haversine(na.lat as f64, na.lon as f64, nb.lat as f64, nb.lon as f64)
                .min(u16::MAX as f64) as u16;
            // 1 = forward only, -1 = reverse only, 0 = both directions
            if way.oneway >= 0 {
                edge_list.entry(ai).or_default().push((bi, dist));
            }
            if way.oneway <= 0 {
                edge_list.entry(bi).or_default().push((ai, dist));
            }
        }
    }

    // --- Prune nodes far from any transit stop (same rule as walk graph) ---
    if !stop_coords.is_empty() {
        const GRID_SIZE: f64 = 0.005; // ~550m
        let mut stop_grid: HashMap<(i32, i32), Vec<(f64, f64)>> = HashMap::new();
        for &(slat, slon) in stop_coords {
            let gx = (slon / GRID_SIZE) as i32;
            let gy = (slat / GRID_SIZE) as i32;
            stop_grid.entry((gx, gy)).or_default().push((slat, slon));
        }
        let radius_cells = ((max_radius / 550.0).ceil() as i32).max(1);

        let mut keep: HashSet<u32> = HashSet::new();
        for (ni, n) in nodes.iter().enumerate() {
            let nlat = n.lat as f64;
            let nlon = n.lon as f64;
            let gx = (nlon / GRID_SIZE) as i32;
            let gy = (nlat / GRID_SIZE) as i32;
            'outer: for dx in -radius_cells..=radius_cells {
                for dy in -radius_cells..=radius_cells {
                    if let Some(cell) = stop_grid.get(&(gx + dx, gy + dy)) {
                        for &(slat, slon) in cell {
                            if haversine(nlat, nlon, slat, slon) <= max_radius {
                                keep.insert(ni as u32);
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }

        if keep.len() < nodes.len() {
            let mut sorted_keep: Vec<u32> = keep.into_iter().collect();
            sorted_keep.sort_unstable();
            let mut old_to_new: HashMap<u32, u32> = HashMap::with_capacity(sorted_keep.len());
            let mut new_nodes: Vec<WalkNode> = Vec::with_capacity(sorted_keep.len());
            for &old_i in &sorted_keep {
                old_to_new.insert(old_i, new_nodes.len() as u32);
                new_nodes.push(nodes[old_i as usize]);
            }
            let mut new_edge_list: HashMap<u32, Vec<(u32, u16)>> = HashMap::new();
            for &old_i in &sorted_keep {
                if let Some(edges) = edge_list.get(&old_i) {
                    let new_i = old_to_new[&old_i];
                    for &(old_to, dist) in edges {
                        if let Some(&new_to) = old_to_new.get(&old_to) {
                            new_edge_list.entry(new_i).or_default().push((new_to, dist));
                        }
                    }
                }
            }
            nodes = new_nodes;
            edge_list = new_edge_list;
        }
    }

    let final_edges: HashMap<u32, Vec<WalkEdge>> = edge_list
        .into_iter()
        .map(|(ni, edges)| {
            let we: Vec<WalkEdge> = edges
                .into_iter()
                .map(|(to, dist)| WalkEdge {
                    to_node_idx: to,
                    dist_meters: dist,
                    geometry: vec![],
                })
                .collect();
            (ni, we)
        })
        .collect();

    Ok((nodes, final_edges))
}
