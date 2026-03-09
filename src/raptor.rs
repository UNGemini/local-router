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

// fraction of each transit leg's duration added as perceived cost.
// discourages marginal transit plans (e.g. bus+mtr when walking takes about the same time).
// 0.2 = 20%: a 10-min bus ride costs 12 min in the routing decision.
const TRANSIT_PENALTY_FRACTION: f32 = 0.2;

// a raptor journey leg: board at a stop, alight at a stop, on a specific trip
#[derive(Debug, Clone, Copy)]
pub struct RaptorLeg {
    pub route_idx: u32,
    pub trip_num: u32,       // index within route's trips
    pub board_stop_pos: u32, // position within route's stop sequence
    pub alight_stop_pos: u32,
    pub board_time: u32,
    pub alight_time: u32,
    // for frequency trips: seconds added to all template stop times
    pub freq_delta: u32,
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

    // dual arrays: real = physical arrival time, penalized = real + accumulated transit penalty.
    // routing decisions (is this alight better?) use penalized times.
    // boarding feasibility (can I catch this trip?) uses real times.
    // journey reconstruction uses real times only.
    let mut best_real = vec![NOT_SET; num_stops];
    let mut best_penalized = vec![NOT_SET; num_stops];
    let mut best_per_round_real = vec![vec![NOT_SET; num_stops]; max_rounds + 1];
    let mut best_per_round_penalized = vec![vec![NOT_SET; num_stops]; max_rounds + 1];

    // parent pointers for journey reconstruction
    #[derive(Clone, Copy)]
    struct Parent {
        route_idx: u32,
        trip_num: u32,
        board_stop_pos: u32,
        board_time: u32,
        freq_delta: u32,
        prev_stop_idx: u32,
    }
    let empty_parent = Parent {
        route_idx: NOT_SET,
        trip_num: NOT_SET,
        board_stop_pos: NOT_SET,
        board_time: NOT_SET,
        freq_delta: 0,
        prev_stop_idx: NOT_SET,
    };
    let mut parents = vec![vec![empty_parent; num_stops]; max_rounds + 1];

    // initialize: set origin stops with walking time (no transit penalty on walking)
    let mut marked = vec![false; num_stops];
    for &(stop_idx, walk_secs) in &query.origin_stops {
        let arr = query.departure_time + walk_secs;
        if arr < best_real[stop_idx as usize] {
            best_real[stop_idx as usize] = arr;
            best_penalized[stop_idx as usize] = arr;
            best_per_round_real[0][stop_idx as usize] = arr;
            best_per_round_penalized[0][stop_idx as usize] = arr;
            marked[stop_idx as usize] = true;
        }
    }

    // raptor rounds
    for k in 1..=max_rounds {
        // collect routes to scan from marked stops
        let mut routes_to_scan: Vec<(u32, u32)> = Vec::new();
        let mut route_seen = vec![NOT_SET; data.routes.len()];

        for stop_idx in 0..num_stops {
            if !marked[stop_idx] {
                continue;
            }
            for &route_idx in &data.stops[stop_idx].route_idxs {
                let route = &data.routes[route_idx as usize];
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

            let mut current_trip: Option<(u32, u32)> = None; // (trip_num, freq_delta)
            let mut board_stop_pos: u32 = 0;
            let mut board_time: u32 = NOT_SET;
            // penalized time at the board stop (carried forward for penalty calculation)
            let mut board_penalized: u32 = NOT_SET;

            for pos in earliest_pos as usize..num_stops_in_route {
                let stop_idx = route.stop_idxs[pos] as usize;

                // try to alight here if we're on a trip
                if let Some((trip_num, freq_delta)) = current_trip {
                    let real_arr = route
                        .arrival_at(&data.arrivals, trip_num, pos)
                        .saturating_add(freq_delta);
                    if real_arr != NOT_SET {
                        // penalized arrival = board_penalized + real_ride_time + 20% of real_ride_time
                        let ride_time = real_arr.saturating_sub(board_time);
                        let penalty = (ride_time as f32 * TRANSIT_PENALTY_FRACTION) as u32;
                        let penalized_arr = board_penalized
                            .saturating_add(ride_time)
                            .saturating_add(penalty);
                        // use penalized for comparison, store both
                        if penalized_arr < best_penalized[stop_idx] {
                            best_real[stop_idx] = best_real[stop_idx].min(real_arr);
                            best_penalized[stop_idx] = penalized_arr;
                            best_per_round_real[k][stop_idx] = real_arr;
                            best_per_round_penalized[k][stop_idx] = penalized_arr;
                            new_marked[stop_idx] = true;
                            parents[k][stop_idx] = Parent {
                                route_idx,
                                trip_num,
                                board_stop_pos,
                                board_time,
                                freq_delta,
                                prev_stop_idx: NOT_SET,
                            };
                        }
                    }
                }

                // try to board an earlier trip at this stop.
                // use REAL arrival from previous round (must physically be there to board).
                let prev_real = best_per_round_real[k - 1][stop_idx];
                if prev_real == NOT_SET {
                    continue;
                }

                if let Some((tn, fd)) =
                    earliest_trip(data, route, pos, prev_real, query.service_day)
                {
                    let dep = route
                        .departure_at(&data.departures, tn, pos)
                        .saturating_add(fd);
                    if current_trip.is_none() || dep < board_time {
                        current_trip = Some((tn, fd));
                        board_stop_pos = pos as u32;
                        board_time = dep;
                        // carry forward the penalized time at this stop
                        board_penalized = best_per_round_penalized[k - 1][stop_idx];
                        // if we wait, the wait time is real (no penalty on waiting)
                        let wait = dep.saturating_sub(prev_real);
                        board_penalized = board_penalized.saturating_add(wait);
                    }
                }
            }
        }

        // footpath transfers: extend arrivals via walking (no transit penalty on walking)
        for stop_idx in 0..num_stops {
            if !new_marked[stop_idx] {
                continue;
            }
            let stop = &data.stops[stop_idx];
            let t_start = stop.transfers_offset as usize;
            let t_end = t_start + stop.num_transfers as usize;
            for t in &data.transfers[t_start..t_end] {
                let walk_s = t.walk_seconds as u32;
                let new_real = best_per_round_real[k][stop_idx].saturating_add(walk_s);
                let new_penalized = best_per_round_penalized[k][stop_idx].saturating_add(walk_s);
                let to = t.to_stop_idx as usize;
                if new_penalized < best_penalized[to] {
                    best_real[to] = best_real[to].min(new_real);
                    best_penalized[to] = new_penalized;
                    best_per_round_real[k][to] = new_real;
                    best_per_round_penalized[k][to] = new_penalized;
                    new_marked[to] = true;
                    parents[k][to] = Parent {
                        route_idx: NOT_SET,
                        trip_num: NOT_SET,
                        board_stop_pos: NOT_SET,
                        board_time: NOT_SET,
                        freq_delta: 0,
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

    // extract journeys using REAL times (penalty was only for routing decisions)
    let mut journeys = Vec::new();
    for &(dest_stop, egress_walk) in &query.dest_stops {
        for k in 1..=max_rounds {
            let arr = best_per_round_real[k][dest_stop as usize];
            if arr == NOT_SET {
                continue;
            }
            let total_arr = arr + egress_walk;

            let mut legs = Vec::new();
            let mut cur_stop = dest_stop as usize;
            let mut cur_round = k;

            while cur_round > 0 {
                let p = &parents[cur_round][cur_stop];
                if p.route_idx == NOT_SET && p.prev_stop_idx != NOT_SET {
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
                let actual_alight_time = data.routes[p.route_idx as usize]
                    .arrival_at(&data.arrivals, p.trip_num, alight_pos as usize)
                    .saturating_add(p.freq_delta);
                legs.push(RaptorLeg {
                    route_idx: p.route_idx,
                    trip_num: p.trip_num,
                    board_stop_pos: p.board_stop_pos,
                    alight_stop_pos: alight_pos,
                    board_time: p.board_time,
                    alight_time: actual_alight_time,
                    freq_delta: p.freq_delta,
                });

                let board_stop =
                    data.routes[p.route_idx as usize].stop_idxs[p.board_stop_pos as usize];
                cur_stop = board_stop as usize;
                cur_round -= 1;
            }

            if legs.is_empty() {
                continue;
            }
            legs.reverse();

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
// and runs on the given service day.
// returns (trip_num_within_route, freq_delta) where freq_delta is added to all stop times.
// for schedule trips: freq_delta = 0, uses binary search.
// for frequency template trips: computes next headway departure mathematically.
fn earliest_trip(
    data: &TransitData,
    route: &crate::data::Route,
    stop_pos: usize,
    min_dep: u32,
    service_day: u16,
) -> Option<(u32, u32)> {
    let num_trips = route.num_trips;
    if num_trips == 0 {
        return None;
    }

    let mut best: Option<(u32, u32)> = None; // (trip_num, freq_delta), earliest departure wins

    // binary search to find the first schedule trip that could depart >= min_dep.
    // frequency trips have template times earlier than their actual service window,
    // so we cannot use the binary search to skip them — scan all trips.
    // for schedule-only routes the binary search provides the fast path.
    let sched_lo = {
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
        lo
    };

    for t in 0..num_trips {
        let trip_idx = route.trip_idxs[t as usize];
        let trip = &data.trips[trip_idx as usize];
        if trip.service_idx == NOT_SET {
            continue;
        }
        if !data.services[trip.service_idx as usize].runs_on(service_day) {
            continue;
        }

        if trip.frequency_rules.is_empty() {
            // schedule-based: skip before binary search result
            if t < sched_lo {
                continue;
            }
            let dep = route.departure_at(&data.departures, t, stop_pos);
            if dep == NOT_SET || dep < min_dep {
                continue;
            }
            let candidate = (t, 0u32);
            if best.is_none()
                || dep
                    < route
                        .departure_at(&data.departures, best.unwrap().0, stop_pos)
                        .saturating_add(best.unwrap().1)
            {
                best = Some(candidate);
            }
            // sorted order: no later trip can be earlier
            break;
        } else {
            // frequency-based template trip: compute next departure mathematically
            let template_dep = route.departure_at(&data.departures, t, stop_pos);
            if template_dep == NOT_SET {
                continue;
            }
            for rule in &trip.frequency_rules {
                // actual departures in this window: rule.start_time + offset, ..., in steps of headway
                // where offset = template_dep - template_first_departure
                let offset_from_first = template_dep.saturating_sub(trip.template_first_departure);
                let rule_dep_base = rule.start_time + offset_from_first;
                let rule_dep_end = rule.end_time + offset_from_first;
                if rule_dep_base >= rule_dep_end || rule.headway_secs == 0 {
                    continue;
                }
                let next_dep = if rule_dep_base >= min_dep {
                    rule_dep_base
                } else {
                    let steps =
                        (min_dep - rule_dep_base + rule.headway_secs - 1) / rule.headway_secs;
                    rule_dep_base + steps * rule.headway_secs
                };
                if next_dep >= rule_dep_end {
                    continue;
                }
                // freq_delta: shift to apply to all template stop times for this run
                let freq_delta = next_dep.saturating_sub(template_dep);
                let actual_dep = template_dep + freq_delta;
                if best.is_none()
                    || actual_dep
                        < route
                            .departure_at(&data.departures, best.unwrap().0, stop_pos)
                            .saturating_add(best.unwrap().1)
                {
                    best = Some((t, freq_delta));
                }
            }
        }
    }

    best
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
