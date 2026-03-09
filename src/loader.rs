// loads a .wheelsrouter capnp file into in-memory data structures

use crate::data::*;
use crate::transit_capnp::transit_data;
use capnp::serialize;

// simple string hash for currency codes (avoids storing strings in fare rules)
fn hash_str(s: &str) -> u32 {
    let mut h: u32 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    h
}

// read packed u32 little-endian values from a Data blob
fn unpack_u32_le(data: &[u8]) -> Vec<u32> {
    let count = data.len() / 4;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * 4;
        let v = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        out.push(v);
    }
    out
}

// read packed float32 lat/lon pairs from a Data blob
fn unpack_geometry(data: &[u8]) -> Vec<(f32, f32)> {
    let count = data.len() / 8;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = i * 8;
        let lat = f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        let lon = f32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]);
        out.push((lat, lon));
    }
    out
}

pub fn load(bytes: &[u8]) -> Result<TransitData, capnp::Error> {
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(None); // unlimited traversal for large data files
    let reader = serialize::read_message_from_flat_slice(&mut &bytes[..], opts)?;
    let root = reader.get_root::<transit_data::Reader>()?;

    let feed_id = root.get_feed_id()?.to_string()?;
    let timezone = root.get_timezone()?.to_string()?;

    // agencies
    let ag_reader = root.get_agencies()?;
    let mut agencies = Vec::with_capacity(ag_reader.len() as usize);
    for a in ag_reader.iter() {
        agencies.push(Agency {
            id: a.get_id()?.to_string()?,
            name: a.get_name()?.to_string()?,
            url: a.get_url()?.to_string()?,
        });
    }

    // stops
    let s_reader = root.get_stops()?;
    let mut stops = Vec::with_capacity(s_reader.len() as usize);
    for s in s_reader.iter() {
        let ri = s.get_route_idxs()?;
        let mut route_idxs = Vec::with_capacity(ri.len() as usize);
        for i in 0..ri.len() {
            route_idxs.push(ri.get(i));
        }
        stops.push(Stop {
            id: s.get_id()?.to_string()?,
            name: s.get_name()?.to_string()?,
            lat: s.get_lat(),
            lon: s.get_lon(),
            location_type: s.get_location_type(),
            parent_idx: s.get_parent_idx(),
            platform_code: s.get_platform_code()?.to_string()?,
            zone_id: s.get_zone_id()?.to_string()?,
            transfers_offset: s.get_transfers_offset(),
            num_transfers: s.get_num_transfers(),
            route_idxs,
            walk_node_idx: s.get_walk_node_idx(),
        });
    }

    // routes
    let r_reader = root.get_routes()?;
    let mut routes = Vec::with_capacity(r_reader.len() as usize);
    for r in r_reader.iter() {
        let si = r.get_stop_idxs()?;
        let mut stop_idxs = Vec::with_capacity(si.len() as usize);
        for i in 0..si.len() {
            stop_idxs.push(si.get(i));
        }
        let ti = r.get_trip_idxs()?;
        let mut trip_idxs = Vec::with_capacity(ti.len() as usize);
        for i in 0..ti.len() {
            trip_idxs.push(ti.get(i));
        }
        routes.push(Route {
            id: r.get_id()?.to_string()?,
            short_name: r.get_short_name()?.to_string()?,
            long_name: r.get_long_name()?.to_string()?,
            route_type: r.get_type(),
            color: r.get_color(),
            text_color: r.get_text_color(),
            agency_idx: r.get_agency_idx(),
            stop_idxs,
            stop_times_offset: r.get_stop_times_offset(),
            num_trips: r.get_num_trips(),
            trip_idxs,
        });
    }

    // packed stop times
    let arrivals = unpack_u32_le(root.get_stop_arrivals()?);
    let departures = unpack_u32_le(root.get_stop_departures()?);

    // transfers
    let t_reader = root.get_transfers()?;
    let mut transfers = Vec::with_capacity(t_reader.len() as usize);
    for t in t_reader.iter() {
        transfers.push(Transfer {
            to_stop_idx: t.get_to_stop_idx(),
            walk_seconds: t.get_walk_seconds(),
            dist_meters: t.get_dist_meters(),
        });
    }

    // services
    let sv_reader = root.get_services()?;
    let mut services = Vec::with_capacity(sv_reader.len() as usize);
    for sv in sv_reader.iter() {
        services.push(Service {
            id: sv.get_id()?.to_string()?,
            start_date: sv.get_start_date(),
            day_bits: sv.get_day_bits()?.to_vec(),
        });
    }

    // trips
    let tr_reader = root.get_trips()?;
    let mut trips = Vec::with_capacity(tr_reader.len() as usize);
    for t in tr_reader.iter() {
        trips.push(Trip {
            id: t.get_id()?.to_string()?,
            route_idx: t.get_route_idx(),
            service_idx: t.get_service_idx(),
            headsign: t.get_headsign()?.to_string()?,
            direction_id: t.get_direction_id(),
        });
    }

    // fare rules
    let fr_reader = root.get_fare_rules()?;
    let mut fare_rules = Vec::with_capacity(fr_reader.len() as usize);
    for fr in fr_reader.iter() {
        fare_rules.push(FareRule {
            route_idx: fr.get_route_idx(),
            price: fr.get_price(),
            currency_hash: hash_str(fr.get_currency()?.to_str()?),
        });
    }

    // walk graph
    let wg_reader = root.get_walk_graph()?;
    let wn_reader = wg_reader.get_nodes()?;
    let mut walk_nodes = Vec::with_capacity(wn_reader.len() as usize);
    for n in wn_reader.iter() {
        walk_nodes.push(WalkNode {
            lat: n.get_lat(),
            lon: n.get_lon(),
            edges_offset: n.get_edges_offset(),
            num_edges: n.get_num_edges(),
        });
    }
    let we_reader = wg_reader.get_edges()?;
    let mut walk_edges = Vec::with_capacity(we_reader.len() as usize);
    for e in we_reader.iter() {
        walk_edges.push(WalkEdge {
            to_node_idx: e.get_to_node_idx(),
            dist_meters: e.get_dist_meters(),
            geometry_offset: e.get_geometry_offset(),
            geometry_len: e.get_geometry_len(),
        });
    }
    let geom_data = wg_reader.get_geometry()?;
    let geometry = unpack_geometry(geom_data);

    Ok(TransitData {
        feed_id,
        timezone,
        stops,
        routes,
        trips,
        agencies,
        arrivals,
        departures,
        transfers,
        services,
        walk_graph: WalkGraph {
            nodes: walk_nodes,
            edges: walk_edges,
            geometry,
        },
        fare_rules,
    })
}
