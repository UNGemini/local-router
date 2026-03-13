/// Cap'n Proto message assembly and serialization.
/// Produces the same binary format as build_capnp() in pipeline/build.py.
///
/// Key improvements over Python:
///  - flat_geometry uses Vec<u8> with extend_from_slice (O(1) amortised)
///  - all_stop_times are written directly as u32 LE bytes, no intermediate dict
///  - zero intermediate allocations for the packed arrays
use crate::gtfs::{EncodedService, GtfsData, RoutePattern, NOT_SET};
use crate::osm::{WalkEdge, WalkNode};
use crate::transfers::Transfer;
use crate::transit_capnp::transit_data;
use anyhow::Result;
use capnp::message::{Builder, HeapAllocator};
use capnp::serialize;
use std::collections::{HashMap, HashSet};

pub fn build_capnp(
    gtfs: &GtfsData,
    patterns: &[RoutePattern],
    services: &[EncodedService],
    svc_id_map: &HashMap<String, u32>,
    transfers_in: &[Transfer],
    walk_nodes: &[WalkNode],
    walk_edges: &HashMap<u32, Vec<WalkEdge>>,
    stop_walk_links: &[u32],
) -> Result<Vec<u8>> {
    let mut message = Builder::new(HeapAllocator::new());
    let mut root = message.init_root::<transit_data::Builder>();

    root.set_feed_id(&gtfs.feed_id);
    root.set_timezone(&gtfs.timezone);

    // ---- agencies ----
    {
        let mut ag_list = root.reborrow().init_agencies(gtfs.agencies.len() as u32);
        for (i, a) in gtfs.agencies.iter().enumerate() {
            let mut entry = ag_list.reborrow().get(i as u32);
            entry.set_id(&a.agency_id);
            entry.set_name(&a.agency_name);
            entry.set_url(&a.agency_url);
        }
    }

    // ---- sort transfers by from_stop for contiguous slice lookup ----
    let mut transfers: Vec<Transfer> = transfers_in.to_vec();
    transfers.sort_unstable_by_key(|t| t.from_stop_idx);

    // Build transfer offsets per stop
    let mut transfer_offsets: Vec<(u32, u32)> = vec![(0u32, 0u32); gtfs.stops.len()];
    if !transfers.is_empty() {
        let mut cur_from = transfers[0].from_stop_idx;
        let mut off_start = 0u32;
        for (i, t) in transfers.iter().enumerate() {
            if t.from_stop_idx != cur_from {
                if (cur_from as usize) < transfer_offsets.len() {
                    transfer_offsets[cur_from as usize] = (off_start, i as u32 - off_start);
                }
                cur_from = t.from_stop_idx;
                off_start = i as u32;
            }
        }
        // Last group
        if (cur_from as usize) < transfer_offsets.len() {
            transfer_offsets[cur_from as usize] = (off_start, transfers.len() as u32 - off_start);
        }
    }

    // ---- route index per stop ----
    let mut stop_routes: Vec<HashSet<u32>> = vec![HashSet::new(); gtfs.stops.len()];
    for (pidx, pat) in patterns.iter().enumerate() {
        for &sidx in &pat.stop_idxs {
            if (sidx as usize) < stop_routes.len() {
                stop_routes[sidx as usize].insert(pidx as u32);
            }
        }
    }

    // ---- trip -> pattern index ----
    let mut trip_to_pattern: Vec<u32> = vec![NOT_SET; gtfs.trips.len()];
    for (pidx, pat) in patterns.iter().enumerate() {
        for &(tidx, _) in &pat.trip_order {
            if (tidx as usize) < trip_to_pattern.len() {
                trip_to_pattern[tidx as usize] = pidx as u32;
            }
        }
    }

    // ---- stops ----
    {
        let mut s_list = root.reborrow().init_stops(gtfs.stops.len() as u32);
        for (i, s) in gtfs.stops.iter().enumerate() {
            let mut entry = s_list.reborrow().get(i as u32);
            entry.set_id(&s.stop_id);
            entry.set_name(&s.stop_name);
            entry.set_lat(s.stop_lat);
            entry.set_lon(s.stop_lon);
            entry.set_location_type(s.location_type);
            let parent_idx = if s.parent_station.is_empty() {
                NOT_SET
            } else {
                gtfs.stop_id_map
                    .get(&s.parent_station)
                    .copied()
                    .unwrap_or(NOT_SET)
            };
            entry.set_parent_idx(parent_idx);
            entry.set_platform_code(&s.platform_code);
            entry.set_zone_id(&s.zone_id);
            let (toff, tnum) = transfer_offsets[i];
            entry.set_transfers_offset(toff);
            entry.set_num_transfers(tnum.min(u16::MAX as u32) as u16);
            entry.set_walk_node_idx(stop_walk_links.get(i).copied().unwrap_or(NOT_SET));
            let mut rlist: Vec<u32> = stop_routes[i].iter().copied().collect();
            rlist.sort_unstable();
            let mut ri = entry.init_route_idxs(rlist.len() as u32);
            for (k, &r) in rlist.iter().enumerate() {
                ri.set(k as u32, r);
            }
        }
    }

    // ---- routes + packed stop times ----
    // We build the packed arrivals/departures as raw bytes (Vec<u8>) here
    // to avoid any intermediate allocation patterns.
    let mut st_arrivals_bytes: Vec<u8> = Vec::new();
    let mut st_departures_bytes: Vec<u8> = Vec::new();

    {
        let mut r_list = root.reborrow().init_routes(patterns.len() as u32);
        for (pidx, pat) in patterns.iter().enumerate() {
            let gidx = pat.gtfs_route_idx as usize;
            let r = &gtfs.routes[gidx];
            let mut entry = r_list.reborrow().get(pidx as u32);
            entry.set_id(&r.route_id);
            entry.set_short_name(&r.short_name);
            entry.set_long_name(&r.long_name);
            entry.set_type(r.route_type);
            entry.set_color(r.color);
            entry.set_text_color(r.text_color);
            let aid_idx = gtfs.agency_id_map.get(&r.agency_id).copied().unwrap_or(0);
            entry.set_agency_idx(aid_idx as u16);

            let mut si = entry.reborrow().init_stop_idxs(pat.stop_idxs.len() as u32);
            for (k, &sidx) in pat.stop_idxs.iter().enumerate() {
                si.set(k as u32, sidx);
            }

            let sto = (st_arrivals_bytes.len() / 4) as u32;
            entry.set_stop_times_offset(sto);
            entry.set_num_trips(pat.trip_order.len() as u32);

            let mut ti = entry.reborrow().init_trip_idxs(pat.trip_order.len() as u32);
            for (k, &(tidx, _)) in pat.trip_order.iter().enumerate() {
                ti.set(k as u32, tidx);
                let trip = &gtfs.trips[tidx as usize];
                if let Some(st) = gtfs.stop_times.get(&trip.trip_id) {
                    for (&arr, &dep) in st.arrivals.iter().zip(st.departures.iter()) {
                        st_arrivals_bytes.extend_from_slice(&arr.to_le_bytes());
                        st_departures_bytes.extend_from_slice(&dep.to_le_bytes());
                    }
                } else {
                    // frequency template: pad with NOT_SET for each stop slot
                    for _ in &pat.stop_idxs {
                        st_arrivals_bytes.extend_from_slice(&NOT_SET.to_le_bytes());
                        st_departures_bytes.extend_from_slice(&NOT_SET.to_le_bytes());
                    }
                }
            }
        }
    }

    root.reborrow().set_stop_arrivals(&st_arrivals_bytes);
    root.reborrow().set_stop_departures(&st_departures_bytes);

    // ---- transfers ----
    {
        let mut tf_list = root.reborrow().init_transfers(transfers.len() as u32);
        for (i, t) in transfers.iter().enumerate() {
            let mut entry = tf_list.reborrow().get(i as u32);
            entry.set_to_stop_idx(t.to_stop_idx);
            entry.set_walk_seconds(t.walk_seconds.min(u16::MAX as u32) as u16);
            entry.set_dist_meters(t.dist_meters.min(u16::MAX as u32) as u16);
        }
    }

    // ---- services ----
    {
        let mut svc_list = root.reborrow().init_services(services.len() as u32);
        for (i, svc) in services.iter().enumerate() {
            let mut entry = svc_list.reborrow().get(i as u32);
            entry.set_id(&svc.id);
            entry.set_start_date(svc.start_epoch_days.min(u16::MAX as u32) as u16);
            entry.set_day_bits(&svc.day_bits);
        }
    }

    // ---- trips ----
    {
        let mut tr_list = root.reborrow().init_trips(gtfs.trips.len() as u32);
        for (i, t) in gtfs.trips.iter().enumerate() {
            let mut entry = tr_list.reborrow().get(i as u32);
            entry.set_id(&t.trip_id);
            entry.set_route_idx(trip_to_pattern[i]);
            let svc_idx = svc_id_map.get(&t.service_id).copied().unwrap_or(NOT_SET);
            entry.set_service_idx(svc_idx);
            entry.set_headsign(&t.headsign);
            entry.set_direction_id(t.direction_id);
            if !t.frequency_rules.is_empty() {
                let mut rules = entry
                    .reborrow()
                    .init_frequency_rules(t.frequency_rules.len() as u32);
                for (fi, fe) in t.frequency_rules.iter().enumerate() {
                    let mut rule = rules.reborrow().get(fi as u32);
                    rule.set_start_time(fe.start_time);
                    rule.set_end_time(fe.end_time);
                    rule.set_headway_secs(fe.headway_secs);
                    rule.set_exact_times(fe.exact_times);
                }
                entry.set_template_first_departure(t.template_first_departure);
            }
        }
    }

    // ---- fare rules ----
    {
        let mut gtfs_route_to_patterns: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pidx, pat) in patterns.iter().enumerate() {
            gtfs_route_to_patterns
                .entry(pat.gtfs_route_idx)
                .or_default()
                .push(pidx as u32);
        }
        let mut fr_out: Vec<(u32, String, String, u32, String)> = Vec::new();
        for fr in &gtfs.fare_rules {
            let fa = match gtfs.fare_attrs.get(&fr.fare_id) {
                Some(f) => f,
                None => continue,
            };
            let price = (fa.price * 100.0).round() as u32;
            if fr.route_id.is_empty() {
                fr_out.push((
                    NOT_SET,
                    fr.origin_id.clone(),
                    fr.destination_id.clone(),
                    price,
                    fa.currency_type.clone(),
                ));
            } else if let Some(&gidx) = gtfs.route_id_map.get(&fr.route_id) {
                for &pidx in gtfs_route_to_patterns
                    .get(&gidx)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                {
                    fr_out.push((
                        pidx,
                        fr.origin_id.clone(),
                        fr.destination_id.clone(),
                        price,
                        fa.currency_type.clone(),
                    ));
                }
            }
        }
        let mut frl = root.reborrow().init_fare_rules(fr_out.len() as u32);
        for (i, (ridx, oz, dz, price, cur)) in fr_out.iter().enumerate() {
            let mut entry = frl.reborrow().get(i as u32);
            entry.set_route_idx(*ridx);
            entry.set_origin_zone(oz);
            entry.set_dest_zone(dz);
            entry.set_price(*price);
            entry.set_currency(cur);
        }
    }

    // ---- walk graph ----
    {
        // Build flat edge list and geometry blob.
        // Uses Vec<u8> with extend_from_slice — O(1) amortised, no quadratic copies.
        let mut flat_edges: Vec<(u32, u16, u32, u16)> = Vec::new(); // (to_node, dist, geom_off, geom_len)
        let mut edge_offsets: Vec<(u32, u32)> = Vec::with_capacity(walk_nodes.len()); // (offset, count)
        let mut flat_geometry: Vec<u8> = Vec::new(); // packed f32 lat/lon pairs
        let mut geom_pair_offset: u32 = 0;

        for ni in 0..walk_nodes.len() as u32 {
            let offset = flat_edges.len() as u32;
            let node_edges = walk_edges.get(&ni).map(|v| v.as_slice()).unwrap_or(&[]);
            edge_offsets.push((offset, node_edges.len() as u32));
            for edge in node_edges {
                let g_off = geom_pair_offset;
                let g_len = edge.geometry.len() as u16;
                for &(glat, glon) in &edge.geometry {
                    flat_geometry.extend_from_slice(&glat.to_le_bytes());
                    flat_geometry.extend_from_slice(&glon.to_le_bytes());
                }
                geom_pair_offset += g_len as u32;
                flat_edges.push((edge.to_node_idx, edge.dist_meters, g_off, g_len));
            }
        }

        let mut wg = root.reborrow().init_walk_graph();
        wg.reborrow().set_geometry(&flat_geometry);

        {
            let mut wn_list = wg.reborrow().init_nodes(walk_nodes.len() as u32);
            for (i, n) in walk_nodes.iter().enumerate() {
                let mut entry = wn_list.reborrow().get(i as u32);
                entry.set_lat(n.lat);
                entry.set_lon(n.lon);
                let (off, num) = edge_offsets.get(i).copied().unwrap_or((0, 0));
                entry.set_edges_offset(off);
                entry.set_num_edges(num.min(u16::MAX as u32) as u16);
            }
        }

        {
            let mut we_list = wg.reborrow().init_edges(flat_edges.len() as u32);
            for (i, &(to, dist, g_off, g_len)) in flat_edges.iter().enumerate() {
                let mut entry = we_list.reborrow().get(i as u32);
                entry.set_to_node_idx(to);
                entry.set_dist_meters(dist);
                entry.set_geometry_offset(g_off);
                entry.set_geometry_len(g_len);
            }
        }
    }

    // Serialize to bytes
    let mut out: Vec<u8> = Vec::new();
    serialize::write_message(&mut out, &message)?;
    Ok(out)
}
