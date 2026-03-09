// raptor (round-based public transit optimized router)
//
// key speed techniques (inspired by motis/nigiri):
// - contiguous stop_times array for cache-friendly sequential scanning
// - route-stop index for o(1) route lookup per stop
// - earliest arrival arrays avoid redundant exploration
// - binary search for earliest valid trip on each route
// - bitfield calendar for o(1) service day check
// - rounds limited by max_transfers to bound computation

use crate::data::{TransitData, NOT_SET};

// a raptor journey leg: board at a stop, alight at a stop, on a specific trip
#[derive(Debug, Clone, Copy)]
pub struct RaptorLeg {
    pub route_idx: u32,
    pub trip_num: u32,       // index within route's trips
    pub board_stop_pos: u32, // position within route's stop sequence
    pub alight_stop_pos: u32,
    pub board_time: u32,
    pub alight_time: u32,
}

// a complete raptor journey from origin to destination
#[derive(Debug, Clone)]
pub struct RaptorJourney {
    pub legs: Vec<RaptorLeg>,
    pub departure_time: u32,
    pub arrival_time: u32,
    // walk time to first stop and from last stop (seconds)
    pub access_walk_secs: u32,
    pub egress_walk_secs: u32,
    pub access_stop_idx: u32,
    pub egress_stop_idx: u32,
}

// query parameters
pub struct RaptorQuery {
    // (stop_idx, walk_seconds) pairs for origin/destination stops
    pub origin_stops: Vec<(u32, u32)>,
    pub dest_stops: Vec<(u32, u32)>,
    pub departure_time: u32, // seconds since midnight
    pub service_day: u16,    // days since epoch
    pub max_transfers: u32,
    pub max_results: usize,
}

pub fn run(data: &TransitData, query: &RaptorQuery) -> Vec<RaptorJourney> {
    let num_stops = data.stops.len();
    let max_rounds = query.max_transfers as usize + 1;

    // tau[k][s] = earliest arrival at stop s using at most k transit legs
    // we use two arrays and swap to save memory
    let mut best = vec![NOT_SET; num_stops]; // overall best arrival per stop
    let mut best_per_round = vec![vec![NOT_SET; num_stops]; max_rounds + 1];

    // parent pointers for journey reconstruction
    // parent[k][s] = how we reached stop s in round k
    #[derive(Clone, Copy)]
    struct Parent {
        route_idx: u32,
        trip_num: u32,
        board_stop_pos: u32,
        board_time: u32,
        prev_stop_idx: u32, // stop we transferred from (for transfer legs)
    }
    let empty_parent = Parent {
        route_idx: NOT_SET,
        trip_num: NOT_SET,
        board_stop_pos: NOT_SET,
        board_time: NOT_SET,
        prev_stop_idx: NOT_SET,
    };
    let mut parents = vec![vec![empty_parent; num_stops]; max_rounds + 1];

    // initialize: set origin stops with walking time
    let mut marked = vec![false; num_stops];
    for &(stop_idx, walk_secs) in &query.origin_stops {
        let arr = query.departure_time + walk_secs;
        if arr < best[stop_idx as usize] {
            best[stop_idx as usize] = arr;
            best_per_round[0][stop_idx as usize] = arr;
            marked[stop_idx as usize] = true;
        }
    }

    // raptor rounds
    for k in 1..=max_rounds {
        // collect routes to scan from marked stops
        let mut routes_to_scan: Vec<(u32, u32)> = Vec::new(); // (route_idx, earliest_stop_pos)
        let mut route_seen = vec![NOT_SET; data.routes.len()]; // earliest stop pos per route

        for stop_idx in 0..num_stops {
            if !marked[stop_idx] {
                continue;
            }
            for &route_idx in &data.stops[stop_idx].route_idxs {
                let route = &data.routes[route_idx as usize];
                // find position of this stop in the route
                for (pos, &sidx) in route.stop_idxs.iter().enumerate() {
                    if sidx == stop_idx as u32 {
                        if route_seen[route_idx as usize] == NOT_SET
                            || (pos as u32) < route_seen[route_idx as usize]
                        {
                            route_seen[route_idx as usize] = pos as u32;
                        }
                        break;
                    }
                }
            }
        }

        for (ridx, &earliest_pos) in route_seen.iter().enumerate() {
            if earliest_pos != NOT_SET {
                routes_to_scan.push((ridx as u32, earliest_pos));
            }
        }

        let mut new_marked = vec![false; num_stops];

        // scan each route
        for &(route_idx, earliest_pos) in &routes_to_scan {
            let route = &data.routes[route_idx as usize];
            let num_stops_in_route = route.stop_idxs.len();
            if num_stops_in_route == 0 || route.num_trips == 0 {
                continue;
            }

            // find the earliest trip we can board
            let mut current_trip: Option<u32> = None;
            let mut board_stop_pos: u32 = 0;
            let mut board_time: u32 = NOT_SET;

            for pos in earliest_pos as usize..num_stops_in_route {
                let stop_idx = route.stop_idxs[pos] as usize;

                // try to alight here if we're on a trip
                if let Some(trip_num) = current_trip {
                    let arr = route.arrival_at(&data.arrivals, trip_num, pos);
                    if arr != NOT_SET && arr < best[stop_idx] {
                        best[stop_idx] = arr;
                        best_per_round[k][stop_idx] = arr;
                        new_marked[stop_idx] = true;
                        parents[k][stop_idx] = Parent {
                            route_idx,
                            trip_num,
                            board_stop_pos,
                            board_time,
                            prev_stop_idx: NOT_SET,
                        };
                    }
                }

                // try to board an earlier trip at this stop
                let prev_arr = best_per_round[k - 1][stop_idx];
                if prev_arr == NOT_SET {
                    continue;
                }

                // binary search for earliest trip departing after prev_arr
                let trip_num = earliest_trip(data, route, pos, prev_arr, query.service_day);
                if let Some(tn) = trip_num {
                    let dep = route.departure_at(&data.departures, tn, pos);
                    if current_trip.is_none() || dep < board_time {
                        current_trip = Some(tn);
                        board_stop_pos = pos as u32;
                        board_time = dep;
                    }
                }
            }
        }

        // footpath transfers: extend arrivals via walking
        for stop_idx in 0..num_stops {
            if !new_marked[stop_idx] {
                continue;
            }
            let stop = &data.stops[stop_idx];
            let t_start = stop.transfers_offset as usize;
            let t_end = t_start + stop.num_transfers as usize;
            for t in &data.transfers[t_start..t_end] {
                let new_arr = best_per_round[k][stop_idx] + t.walk_seconds as u32;
                let to = t.to_stop_idx as usize;
                if new_arr < best[to] {
                    best[to] = new_arr;
                    best_per_round[k][to] = new_arr;
                    new_marked[to] = true;
                    parents[k][to] = Parent {
                        route_idx: NOT_SET,
                        trip_num: NOT_SET,
                        board_stop_pos: NOT_SET,
                        board_time: NOT_SET,
                        prev_stop_idx: stop_idx as u32,
                    };
                }
            }
        }

        marked = new_marked;
        if !marked.iter().any(|&m| m) {
            break;
        }
    }

    // extract journeys: for each destination stop, reconstruct the path per round
    let mut journeys = Vec::new();
    for &(dest_stop, egress_walk) in &query.dest_stops {
        for k in 1..=max_rounds {
            let arr = best_per_round[k][dest_stop as usize];
            if arr == NOT_SET {
                continue;
            }
            let total_arr = arr + egress_walk;

            // reconstruct legs backwards
            let mut legs = Vec::new();
            let mut cur_stop = dest_stop as usize;
            let mut cur_round = k;

            while cur_round > 0 {
                let p = &parents[cur_round][cur_stop];
                if p.route_idx == NOT_SET && p.prev_stop_idx != NOT_SET {
                    // this was a transfer, go to the previous stop
                    cur_stop = p.prev_stop_idx as usize;
                    continue;
                }
                if p.route_idx == NOT_SET {
                    break;
                }
                let alight_pos = find_stop_pos(
                    &data.routes[p.route_idx as usize],
                    cur_stop as u32,
                    p.board_stop_pos,
                );
                let actual_alight_time = data.routes[p.route_idx as usize].arrival_at(
                    &data.arrivals,
                    p.trip_num,
                    alight_pos as usize,
                );
                legs.push(RaptorLeg {
                    route_idx: p.route_idx,
                    trip_num: p.trip_num,
                    board_stop_pos: p.board_stop_pos,
                    alight_stop_pos: alight_pos,
                    board_time: p.board_time,
                    alight_time: actual_alight_time,
                });

                // go to the boarding stop in the previous round
                let board_stop =
                    data.routes[p.route_idx as usize].stop_idxs[p.board_stop_pos as usize];
                cur_stop = board_stop as usize;
                cur_round -= 1;
            }

            if legs.is_empty() {
                continue;
            }
            legs.reverse();

            // find which origin stop was used
            let first_board_stop =
                data.routes[legs[0].route_idx as usize].stop_idxs[legs[0].board_stop_pos as usize];
            let access_walk = query
                .origin_stops
                .iter()
                .find(|(s, _)| *s == first_board_stop)
                .map(|(_, w)| *w)
                .unwrap_or(0);

            let dep_time = legs[0].board_time.saturating_sub(access_walk);

            journeys.push(RaptorJourney {
                legs,
                departure_time: dep_time,
                arrival_time: total_arr,
                access_walk_secs: access_walk,
                egress_walk_secs: egress_walk,
                access_stop_idx: first_board_stop,
                egress_stop_idx: dest_stop,
            });
        }
    }

    // sort by arrival time and deduplicate
    journeys.sort_by_key(|j| j.arrival_time);
    // pareto-filter: remove dominated journeys (later departure AND later arrival)
    let mut filtered = Vec::new();
    for j in journeys {
        let dominated = filtered.iter().any(|f: &RaptorJourney| {
            f.departure_time >= j.departure_time && f.arrival_time <= j.arrival_time
        });
        if !dominated {
            filtered.push(j);
        }
    }
    filtered.truncate(query.max_results);
    filtered
}

// find the earliest trip on a route at a given stop position that departs at or after `min_dep`
// and runs on the given service day. uses binary search for speed.
fn earliest_trip(
    data: &TransitData,
    route: &crate::data::Route,
    stop_pos: usize,
    min_dep: u32,
    service_day: u16,
) -> Option<u32> {
    let num_trips = route.num_trips;
    if num_trips == 0 {
        return None;
    }

    // binary search: trips are ordered by departure time
    let mut lo = 0u32;
    let mut hi = num_trips;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let dep = route.departure_at(&data.departures, mid, stop_pos);
        if dep < min_dep {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    // linear scan from lo to find first trip that runs on this service day
    for t in lo..num_trips {
        let dep = route.departure_at(&data.departures, t, stop_pos);
        if dep == NOT_SET {
            continue;
        }
        if dep < min_dep {
            continue;
        }
        let trip_idx = route.trip_idxs[t as usize];
        let trip = &data.trips[trip_idx as usize];
        if trip.service_idx == NOT_SET {
            continue;
        }
        if data.services[trip.service_idx as usize].runs_on(service_day) {
            return Some(t);
        }
    }
    None
}

fn find_stop_pos(route: &crate::data::Route, stop_idx: u32, min_pos: u32) -> u32 {
    // search from min_pos onward to handle loop routes
    for pos in min_pos as usize..route.stop_idxs.len() {
        if route.stop_idxs[pos] == stop_idx {
            return pos as u32;
        }
    }
    // fallback: search from beginning
    for (pos, &sidx) in route.stop_idxs.iter().enumerate() {
        if sidx == stop_idx {
            return pos as u32;
        }
    }
    0
}
