/// Transfer computation: explicit, intra-station, and nearby-stop.
use crate::gtfs::{haversine, GtfsData, NOT_SET};
use indicatif::ProgressBar;
use std::collections::{HashMap, HashSet};

pub const MAX_TRANSFER_DIST_DEFAULT: f64 = 300.0;
pub const MAX_TRANSFER_DIST_NO_OSM: f64 = 3000.0;
const WALK_SPEED: f64 = 1.2;
const DEFAULT_INTRA_STATION_SECS: u32 = 120;

#[derive(Debug, Clone, Copy)]
pub struct Transfer {
    pub from_stop_idx: u32,
    pub to_stop_idx: u32,
    pub walk_seconds: u32,
    pub dist_meters: u32,
}

pub fn compute_transfers(
    gtfs: &GtfsData,
    max_transfer_dist: f64,
    pb: &ProgressBar,
) -> Vec<Transfer> {
    // Build pathway_time lookup: (from_stop_id, to_stop_id) -> secs
    let mut pathway_time: HashMap<(String, String), u32> = HashMap::new();
    for pw in &gtfs.pathways {
        let key = (pw.from_stop_id.clone(), pw.to_stop_id.clone());
        pathway_time.insert(key.clone(), pw.traversal_time);
        if pw.is_bidirectional {
            pathway_time.insert(
                (pw.to_stop_id.clone(), pw.from_stop_id.clone()),
                pw.traversal_time,
            );
        }
    }

    // station_platforms[station_idx] -> Vec<platform_idx>
    // station_entrances[station_idx] -> Vec<entrance_idx>
    let mut station_platforms: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut station_entrances: HashMap<u32, Vec<u32>> = HashMap::new();
    for (idx, stop) in gtfs.stops.iter().enumerate() {
        if stop.parent_station.is_empty() {
            continue;
        }
        let parent_idx = match gtfs.stop_id_map.get(&stop.parent_station) {
            Some(&i) => i,
            None => continue,
        };
        let parent_lt = gtfs.stops[parent_idx as usize].location_type;
        if stop.location_type == 0 && parent_lt == 1 {
            station_platforms
                .entry(parent_idx)
                .or_default()
                .push(idx as u32);
        } else if stop.location_type == 2 && parent_lt == 1 {
            station_entrances
                .entry(parent_idx)
                .or_default()
                .push(idx as u32);
        }
    }

    let mut transfers: Vec<Transfer> = Vec::new();
    let mut seen: HashSet<(u32, u32)> = HashSet::new();

    let add = |transfers: &mut Vec<Transfer>,
               seen: &mut HashSet<(u32, u32)>,
               fi: u32,
               ti: u32,
               secs: u32,
               dist: u32| {
        if fi == ti {
            return;
        }
        if seen.insert((fi, ti)) {
            transfers.push(Transfer {
                from_stop_idx: fi,
                to_stop_idx: ti,
                walk_seconds: secs,
                dist_meters: dist,
            });
        }
    };

    // 1. Explicit transfers.txt
    for t in &gtfs.raw_transfers {
        let fidx = match gtfs.stop_id_map.get(&t.from_stop_id) {
            Some(&i) => i,
            None => continue,
        };
        let tidx = match gtfs.stop_id_map.get(&t.to_stop_id) {
            Some(&i) => i,
            None => continue,
        };
        if t.transfer_type == 3 {
            continue;
        } // not possible
        let (secs, dist) = if t.min_transfer_time > 0 {
            (
                t.min_transfer_time,
                (t.min_transfer_time as f64 * WALK_SPEED) as u32,
            )
        } else {
            let fs = &gtfs.stops[fidx as usize];
            let ts = &gtfs.stops[tidx as usize];
            let d = haversine(
                fs.stop_lat as f64,
                fs.stop_lon as f64,
                ts.stop_lat as f64,
                ts.stop_lon as f64,
            );
            let s = (d / WALK_SPEED).max(60.0) as u32;
            (s, d as u32)
        };
        add(&mut transfers, &mut seen, fidx, tidx, secs, dist);
        if t.transfer_type != 1 {
            add(&mut transfers, &mut seen, tidx, fidx, secs, dist);
        }
    }

    // 2. Intra-station transfers
    for (&station_idx, platforms) in &station_platforms {
        let entrances = station_entrances
            .get(&station_idx)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        // platform <-> platform
        for i in 0..platforms.len() {
            for j in (i + 1)..platforms.len() {
                let pi = platforms[i];
                let pj = platforms[j];
                let ps_i = &gtfs.stops[pi as usize];
                let ps_j = &gtfs.stops[pj as usize];
                let tt = pathway_time
                    .get(&(ps_i.stop_id.clone(), ps_j.stop_id.clone()))
                    .or_else(|| pathway_time.get(&(ps_j.stop_id.clone(), ps_i.stop_id.clone())))
                    .copied()
                    .unwrap_or_else(|| {
                        let dist = haversine(
                            ps_i.stop_lat as f64,
                            ps_i.stop_lon as f64,
                            ps_j.stop_lat as f64,
                            ps_j.stop_lon as f64,
                        );
                        ((dist / WALK_SPEED).max(DEFAULT_INTRA_STATION_SECS as f64)) as u32
                    });
                let dist = (tt as f64 * WALK_SPEED) as u32;
                add(&mut transfers, &mut seen, pi, pj, tt, dist);
                add(&mut transfers, &mut seen, pj, pi, tt, dist);
            }
        }

        // entrance <-> platform
        for &ei in entrances {
            let es = &gtfs.stops[ei as usize];
            for &pi in platforms {
                let ps = &gtfs.stops[pi as usize];
                let tt = pathway_time
                    .get(&(es.stop_id.clone(), ps.stop_id.clone()))
                    .or_else(|| pathway_time.get(&(ps.stop_id.clone(), es.stop_id.clone())))
                    .copied()
                    .unwrap_or_else(|| {
                        let dist = haversine(
                            es.stop_lat as f64,
                            es.stop_lon as f64,
                            ps.stop_lat as f64,
                            ps.stop_lon as f64,
                        );
                        ((dist / WALK_SPEED).max(60.0)) as u32
                    });
                let dist = (tt as f64 * WALK_SPEED) as u32;
                add(&mut transfers, &mut seen, ei, pi, tt, dist);
                add(&mut transfers, &mut seen, pi, ei, tt, dist);
            }
        }
    }

    // 3. Nearby-stop transfers: only between location_type=0 stops
    // Grid cell ~330m
    const GRID_SIZE: f64 = 0.003;

    // collect platform stops
    let platform_stops: Vec<(u32, f64, f64)> = gtfs
        .stops
        .iter()
        .enumerate()
        .filter(|(_, s)| s.location_type == 0)
        .map(|(i, s)| (i as u32, s.stop_lat as f64, s.stop_lon as f64))
        .collect();

    // build grid
    let mut grid: HashMap<(i32, i32), Vec<(u32, f64, f64)>> = HashMap::new();
    for &(idx, lat, lon) in &platform_stops {
        let gx = (lon / GRID_SIZE) as i32;
        let gy = (lat / GRID_SIZE) as i32;
        grid.entry((gx, gy)).or_default().push((idx, lat, lon));
    }

    // build parent-station lookup for platform stops
    let parent_of: Vec<u32> = gtfs
        .stops
        .iter()
        .map(|s| {
            if s.parent_station.is_empty() {
                NOT_SET
            } else {
                gtfs.stop_id_map
                    .get(&s.parent_station)
                    .copied()
                    .unwrap_or(NOT_SET)
            }
        })
        .collect();

    for &(si, slat, slon) in &platform_stops {
        pb.inc(1);
        let gx = (slon / GRID_SIZE) as i32;
        let gy = (slat / GRID_SIZE) as i32;
        let sparent = parent_of[si as usize];
        for dx in -1i32..=1 {
            for dy in -1i32..=1 {
                if let Some(cell) = grid.get(&(gx + dx, gy + dy)) {
                    for &(sj, jlat, jlon) in cell {
                        if si >= sj {
                            continue;
                        }
                        // skip if same station
                        if sparent != NOT_SET && sparent == parent_of[sj as usize] {
                            continue;
                        }
                        let dist = haversine(slat, slon, jlat, jlon);
                        if dist <= max_transfer_dist {
                            let secs = ((dist / WALK_SPEED).max(30.0)) as u32;
                            add(&mut transfers, &mut seen, si, sj, secs, dist as u32);
                            add(&mut transfers, &mut seen, sj, si, secs, dist as u32);
                        }
                    }
                }
            }
        }
    }

    transfers
}
