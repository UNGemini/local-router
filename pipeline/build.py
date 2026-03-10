#!/usr/bin/env python3
"""
wheels router pipeline
converts gtfs zip + osm pbf into a .wheelsrouter capnp file

usage: python -m pipeline.build --gtfs path/to/gtfs.zip --osm path/to/region.osm.pbf -o output.wheelsrouter
"""

import argparse
import csv
import io
import math
import os
import struct
import sys
import zipfile
from collections import defaultdict
from datetime import date, timedelta

import capnp

# load the capnp schema
SCHEMA_PATH = os.path.join(os.path.dirname(__file__), "..", "schema", "transit.capnp")
transit_capnp = capnp.load(SCHEMA_PATH)

MAX_U32 = 0xFFFFFFFF
MAX_U16 = 0xFFFF
EARTH_RADIUS_M = 6_371_000
EPOCH = date(1970, 1, 1)
# max walking transfer distance between separate stops (meters)
MAX_TRANSFER_DIST = 300
# default intra-station transfer time when pathways are absent (seconds)
DEFAULT_INTRA_STATION_SECS = 120
WALK_SPEED = 1.2


def haversine(lat1, lon1, lat2, lon2):
    """distance in meters between two lat/lon points"""
    rlat1, rlon1 = math.radians(lat1), math.radians(lon1)
    rlat2, rlon2 = math.radians(lat2), math.radians(lon2)
    dlat = rlat2 - rlat1
    dlon = rlon2 - rlon1
    a = math.sin(dlat / 2) ** 2 + math.cos(rlat1) * math.cos(rlat2) * math.sin(dlon / 2) ** 2
    return EARTH_RADIUS_M * 2 * math.asin(math.sqrt(a))


def parse_time(s):
    """parse gtfs time string (hh:mm:ss or h:mm:ss) to seconds since midnight"""
    if not s or not s.strip():
        return MAX_U32
    parts = s.strip().split(":")
    return int(parts[0]) * 3600 + int(parts[1]) * 60 + int(parts[2])


def parse_date(s):
    """parse yyyymmdd to date"""
    return date(int(s[:4]), int(s[4:6]), int(s[6:8]))


def date_to_epoch_days(d):
    """days since unix epoch"""
    return (d - EPOCH).days


def parse_color(s):
    """parse hex color string to uint32"""
    if not s or not s.strip():
        return 0
    return int(s.strip().lstrip("#"), 16)


def read_csv(zf, filename):
    """read a csv file from a zip, returning list of dicts"""
    try:
        data = zf.read(filename).decode("utf-8-sig")
    except KeyError:
        return []
    reader = csv.DictReader(io.StringIO(data))
    return list(reader)


# ---------------------------------------------------------------------------
# gtfs loader
# ---------------------------------------------------------------------------

class GtfsData:
    """parsed gtfs data with indexed lookups"""

    def __init__(self, gtfs_path):
        zf = zipfile.ZipFile(gtfs_path, "r")

        # ---- agencies ----
        self.agencies = read_csv(zf, "agency.txt")
        self.agency_id_map = {}
        for i, a in enumerate(self.agencies):
            aid = a.get("agency_id", "")
            self.agency_id_map[aid] = i
        # some feeds have a single agency with no id; map empty string
        if not self.agency_id_map and self.agencies:
            self.agency_id_map[""] = 0

        self.timezone = self.agencies[0].get("agency_timezone", "UTC") if self.agencies else "UTC"

        # ---- stops (all location_types 0-4) ----
        raw_stops = read_csv(zf, "stops.txt")
        self.stops = []
        self.stop_id_map = {}
        for s in raw_stops:
            idx = len(self.stops)
            self.stop_id_map[s["stop_id"]] = idx
            self.stops.append(s)

        # ---- routes ----
        self.routes = []
        self.route_id_map = {}
        for r in read_csv(zf, "routes.txt"):
            idx = len(self.routes)
            self.route_id_map[r["route_id"]] = idx
            self.routes.append(r)

        # ---- trips ----
        self.trips = []
        self.trip_id_map = {}
        for t in read_csv(zf, "trips.txt"):
            if t["route_id"] not in self.route_id_map:
                continue
            idx = len(self.trips)
            self.trip_id_map[t["trip_id"]] = idx
            self.trips.append(t)

        # ---- stop_times grouped by trip, sorted by stop_sequence ----
        self.stop_times_by_trip = defaultdict(list)
        for st in read_csv(zf, "stop_times.txt"):
            tid = st.get("trip_id", "")
            sid = st.get("stop_id", "")
            if tid not in self.trip_id_map or sid not in self.stop_id_map:
                continue
            self.stop_times_by_trip[tid].append(st)
        for tid in self.stop_times_by_trip:
            self.stop_times_by_trip[tid].sort(key=lambda x: int(x["stop_sequence"]))

        # ---- calendar + calendar_dates ----
        self.services = {}
        for row in read_csv(zf, "calendar.txt"):
            sid = row["service_id"]
            self.services[sid] = {
                "start": parse_date(row["start_date"]),
                "end": parse_date(row["end_date"]),
                "days": [int(row.get(d, "0") or "0") for d in
                         ["monday", "tuesday", "wednesday", "thursday",
                          "friday", "saturday", "sunday"]],
                "additions": set(),
                "removals": set(),
            }

        for row in read_csv(zf, "calendar_dates.txt"):
            sid = row["service_id"]
            d = parse_date(row["date"])
            etype = int(row["exception_type"])
            if sid not in self.services:
                # calendar_dates-only feed: create a stub entry
                self.services[sid] = {
                    "start": d, "end": d,
                    "days": [0] * 7,
                    "additions": set(), "removals": set(),
                }
            if etype == 1:
                self.services[sid]["additions"].add(d)
                self.services[sid]["end"] = max(self.services[sid]["end"], d)
                self.services[sid]["start"] = min(self.services[sid]["start"], d)
            elif etype == 2:
                self.services[sid]["removals"].add(d)

        # ---- pathways ----
        self.pathways = read_csv(zf, "pathways.txt")

        # ---- transfers.txt ----
        self.raw_transfers = read_csv(zf, "transfers.txt")

        # ---- frequencies.txt ----
        self.frequencies = defaultdict(list)
        for row in read_csv(zf, "frequencies.txt"):
            tid = row.get("trip_id", "")
            if tid in self.trip_id_map:
                self.frequencies[tid].append(row)

        # frequency trips: kept as templates, not expanded.
        # frequencyRules are stored in the capnp output and handled at query time.

        # ---- fare data (v1, kept simple) ----
        self.fare_attrs = {}
        for row in read_csv(zf, "fare_attributes.txt"):
            self.fare_attrs[row["fare_id"]] = row
        self.fare_rules = read_csv(zf, "fare_rules.txt")

        zf.close()


    def _expand_frequencies(self):
        """
        expand frequency entries into concrete trips.
        for exact_times=1: generate trips at every headway interval.
        for exact_times=0: same expansion (common approximation used by most routers).
        the original template trip is removed since its stop_times are just a template.
        """
        expanded_count = 0
        template_tids = set()
        for tid, freq_entries in self.frequencies.items():
            template_sts = self.stop_times_by_trip.get(tid, [])
            if len(template_sts) < 2:
                continue
            # compute time offsets relative to first departure
            first_dep = parse_time(template_sts[0].get("departure_time", "")
                                   or template_sts[0].get("arrival_time", ""))
            if first_dep == MAX_U32:
                continue
            offsets = []
            for st in template_sts:
                arr = parse_time(st.get("arrival_time", ""))
                dep = parse_time(st.get("departure_time", ""))
                offsets.append((
                    (arr - first_dep) if arr != MAX_U32 else MAX_U32,
                    (dep - first_dep) if dep != MAX_U32 else MAX_U32,
                    st,
                ))

            template_trip = self.trips[self.trip_id_map[tid]]
            template_tids.add(tid)

            for fentry in freq_entries:
                start = parse_time(fentry.get("start_time", ""))
                end = parse_time(fentry.get("end_time", ""))
                headway = int(fentry.get("headway_secs", "0") or "0")
                if start == MAX_U32 or end == MAX_U32 or headway <= 0:
                    continue

                t = start
                seq = 0
                while t < end:
                    new_tid = f"{tid}_freq_{seq}"
                    new_trip = dict(template_trip)
                    new_trip["trip_id"] = new_tid
                    new_idx = len(self.trips)
                    self.trip_id_map[new_tid] = new_idx
                    self.trips.append(new_trip)

                    new_sts = []
                    for arr_off, dep_off, orig_st in offsets:
                        nst = dict(orig_st)
                        nst["trip_id"] = new_tid
                        if arr_off != MAX_U32:
                            nst["arrival_time"] = _secs_to_timestr(t + arr_off)
                        if dep_off != MAX_U32:
                            nst["departure_time"] = _secs_to_timestr(t + dep_off)
                        new_sts.append(nst)
                    self.stop_times_by_trip[new_tid] = new_sts
                    expanded_count += 1
                    t += headway
                    seq += 1

        # remove template trips from stop_times (they are not real scheduled trips)
        for tid in template_tids:
            if tid in self.stop_times_by_trip:
                del self.stop_times_by_trip[tid]

        if expanded_count:
            print(f"  expanded {len(template_tids)} frequency templates into {expanded_count} concrete trips")


def _secs_to_timestr(secs):
    """convert seconds since midnight to hh:mm:ss"""
    h = secs // 3600
    m = (secs % 3600) // 60
    s = secs % 60
    return f"{h:02d}:{m:02d}:{s:02d}"


# ---------------------------------------------------------------------------
# station hierarchy helpers
# ---------------------------------------------------------------------------

def get_location_type(stop):
    return int(stop.get("location_type", "0") or "0")


def get_lat(stop):
    v = stop.get("stop_lat", "")
    return float(v) if v else 0.0


def get_lon(stop):
    v = stop.get("stop_lon", "")
    return float(v) if v else 0.0


def build_station_children(gtfs):
    """
    returns two dicts:
      station_platforms[station_idx] -> [platform_idx, ...]
      station_entrances[station_idx] -> [entrance_idx, ...]
    """
    station_platforms = defaultdict(list)
    station_entrances = defaultdict(list)
    for idx, s in enumerate(gtfs.stops):
        lt = get_location_type(s)
        parent_id = s.get("parent_station", "")
        if not parent_id or parent_id not in gtfs.stop_id_map:
            continue
        parent_idx = gtfs.stop_id_map[parent_id]
        parent_lt = get_location_type(gtfs.stops[parent_idx])
        if lt == 0 and parent_lt == 1:
            station_platforms[parent_idx].append(idx)
        elif lt == 2 and parent_lt == 1:
            station_entrances[parent_idx].append(idx)
    return station_platforms, station_entrances


# ---------------------------------------------------------------------------
# route patterns (handles multiple patterns per route correctly)
# ---------------------------------------------------------------------------

def build_route_patterns(gtfs):
    """
    for each route, find all distinct stop patterns and create a separate
    "route pattern" for each. trips with non-matching patterns or no valid
    stop_times are skipped.
    returns list of pattern dicts: { route_idx, stop_idxs, trip_order }
    also remaps gtfs.trips indices so they align with the new per-pattern
    trip lists.
    """
    trips_by_route = defaultdict(list)
    for i, t in enumerate(gtfs.trips):
        ridx = gtfs.route_id_map[t["route_id"]]
        trips_by_route[ridx].append(i)

    # patterns is a list; each entry has gtfs_route_idx + stop pattern + trips
    patterns = []
    skipped_trips = 0
    for ridx, trip_idxs in trips_by_route.items():
        # group by stop pattern
        pattern_trips = defaultdict(list)
        for tidx in trip_idxs:
            trip = gtfs.trips[tidx]
            sts = gtfs.stop_times_by_trip.get(trip["trip_id"], [])
            if len(sts) < 2:
                skipped_trips += 1
                continue
            stop_pattern = []
            valid = True
            for st in sts:
                sid = st.get("stop_id", "")
                if sid not in gtfs.stop_id_map:
                    valid = False
                    break
                stop_pattern.append(gtfs.stop_id_map[sid])
            if not valid:
                skipped_trips += 1
                continue
            pattern_trips[tuple(stop_pattern)].append(tidx)

        if not pattern_trips:
            continue

        # create one pattern entry per unique stop sequence
        for pat_stops, pat_tidxs in pattern_trips.items():
            trip_order = []
            for tidx in pat_tidxs:
                trip = gtfs.trips[tidx]
                sts = gtfs.stop_times_by_trip[trip["trip_id"]]
                dep = parse_time(sts[0].get("departure_time", ""))
                if dep == MAX_U32:
                    dep = parse_time(sts[0].get("arrival_time", ""))
                trip_order.append((tidx, dep))
            trip_order.sort(key=lambda x: x[1])
            patterns.append({
                "gtfs_route_idx": ridx,
                "stop_idxs": list(pat_stops),
                "trip_order": trip_order,
            })

    if skipped_trips:
        print(f"  (skipped {skipped_trips} trips with no valid stop sequence)")
    return patterns


# ---------------------------------------------------------------------------
# calendar bitfields
# ---------------------------------------------------------------------------

def build_service_bitfields(gtfs):
    result = []
    svc_id_map = {}
    for sid, svc in gtfs.services.items():
        start = svc["start"]
        end = svc["end"]
        num_days = (end - start).days + 1
        if num_days <= 0:
            num_days = 1
        bits = bytearray((num_days + 7) // 8)
        for i in range(num_days):
            d = start + timedelta(days=i)
            active = bool(svc["days"][d.weekday()])
            if d in svc["additions"]:
                active = True
            if d in svc["removals"]:
                active = False
            if active:
                bits[i // 8] |= 1 << (i % 8)
        svc_id_map[sid] = len(result)
        result.append((sid, date_to_epoch_days(start), bytes(bits)))
    return result, svc_id_map


# ---------------------------------------------------------------------------
# transfers: explicit, intra-station, nearby-stop
# ---------------------------------------------------------------------------

def compute_transfers(gtfs):
    """
    produce a flat list of directed transfers:
      (from_stop_idx, to_stop_idx, walk_seconds, dist_meters)

    three sources:
      1. explicit transfers.txt entries
      2. intra-station transfers (platform<->platform within same station,
         entrance<->platform within same station) via pathways or defaults
      3. nearby-stop transfers for stops within MAX_TRANSFER_DIST
    """
    station_platforms, station_entrances = build_station_children(gtfs)

    # build a pathway lookup: (from_stop_id, to_stop_id) -> traversal_time
    pathway_time = {}
    for pw in gtfs.pathways:
        fid = pw.get("from_stop_id", "")
        tid = pw.get("to_stop_id", "")
        tt = int(pw.get("traversal_time", "0") or "0")
        length = float(pw.get("length", "0") or "0")
        if tt <= 0 and length > 0:
            tt = int(length / WALK_SPEED)
        if tt <= 0:
            tt = DEFAULT_INTRA_STATION_SECS
        pathway_time[(fid, tid)] = tt
        # if bidirectional
        bidir = int(pw.get("is_bidirectional", "0") or "0")
        if bidir == 1:
            pathway_time[(tid, fid)] = tt

    transfers = []
    seen = set()

    def add_transfer(fi, ti, secs, dist):
        if fi == ti:
            return
        key = (fi, ti)
        if key in seen:
            return
        seen.add(key)
        transfers.append((fi, ti, secs, dist))

    # 1. explicit transfers.txt
    for t in gtfs.raw_transfers:
        fid = t.get("from_stop_id", "")
        tid = t.get("to_stop_id", "")
        if fid not in gtfs.stop_id_map or tid not in gtfs.stop_id_map:
            continue
        fidx = gtfs.stop_id_map[fid]
        tidx = gtfs.stop_id_map[tid]
        ttype = int(t.get("transfer_type", "0") or "0")
        if ttype == 3:
            continue  # transfer not possible
        min_time = int(t.get("min_transfer_time", "0") or "0")
        if min_time <= 0:
            fs, ts = gtfs.stops[fidx], gtfs.stops[tidx]
            dist = haversine(get_lat(fs), get_lon(fs), get_lat(ts), get_lon(ts))
            min_time = max(int(dist / WALK_SPEED), 60)
        else:
            dist = min_time * WALK_SPEED
        add_transfer(fidx, tidx, min_time, int(dist))
        # add reverse unless it's a one-way transfer
        if ttype != 1:  # type 1 = timed, might not be symmetric but usually is
            add_transfer(tidx, fidx, min_time, int(dist))

    # 2. intra-station transfers: platform<->platform, entrance<->platform
    for station_idx, platforms in station_platforms.items():
        entrances = station_entrances.get(station_idx, [])
        station_stop = gtfs.stops[station_idx]

        # platform <-> platform transfers within the same station
        for i, pi in enumerate(platforms):
            for j, pj in enumerate(platforms):
                if i >= j:
                    continue
                ps_i, ps_j = gtfs.stops[pi], gtfs.stops[pj]
                # check pathways first
                pw_key = (ps_i["stop_id"], ps_j["stop_id"])
                pw_key_r = (ps_j["stop_id"], ps_i["stop_id"])
                if pw_key in pathway_time:
                    tt = pathway_time[pw_key]
                elif pw_key_r in pathway_time:
                    tt = pathway_time[pw_key_r]
                else:
                    dist = haversine(get_lat(ps_i), get_lon(ps_i), get_lat(ps_j), get_lon(ps_j))
                    tt = max(int(dist / WALK_SPEED), DEFAULT_INTRA_STATION_SECS)
                d = int(tt * WALK_SPEED)
                add_transfer(pi, pj, tt, d)
                add_transfer(pj, pi, tt, d)

        # entrance <-> platform transfers
        for ei in entrances:
            es = gtfs.stops[ei]
            for pi in platforms:
                ps = gtfs.stops[pi]
                pw_key = (es["stop_id"], ps["stop_id"])
                pw_key_r = (ps["stop_id"], es["stop_id"])
                if pw_key in pathway_time:
                    tt = pathway_time[pw_key]
                elif pw_key_r in pathway_time:
                    tt = pathway_time[pw_key_r]
                else:
                    dist = haversine(get_lat(es), get_lon(es), get_lat(ps), get_lon(ps))
                    tt = max(int(dist / WALK_SPEED), 60)
                d = int(tt * WALK_SPEED)
                add_transfer(ei, pi, tt, d)
                add_transfer(pi, ei, tt, d)

    # 3. nearby-stop transfers (between stops of different stations or parentless stops)
    # only between location_type=0 stops
    stop_positions = []
    for i, s in enumerate(gtfs.stops):
        lt = get_location_type(s)
        if lt == 0:
            stop_positions.append((i, get_lat(s), get_lon(s)))

    grid = defaultdict(list)
    grid_size = 0.003
    for i, lat, lon in stop_positions:
        gx = int(lon / grid_size)
        gy = int(lat / grid_size)
        grid[(gx, gy)].append((i, lat, lon))

    for si, slat, slon in stop_positions:
        gx = int(slon / grid_size)
        gy = int(slat / grid_size)
        # get parent station for this stop
        sparent = gtfs.stops[si].get("parent_station", "")
        sparent_idx = gtfs.stop_id_map.get(sparent, MAX_U32) if sparent else MAX_U32
        for dx in (-1, 0, 1):
            for dy in (-1, 0, 1):
                for (sj, jlat, jlon) in grid.get((gx + dx, gy + dy), []):
                    if si >= sj:
                        continue
                    # skip if same station (already handled above)
                    jparent = gtfs.stops[sj].get("parent_station", "")
                    jparent_idx = gtfs.stop_id_map.get(jparent, MAX_U32) if jparent else MAX_U32
                    if sparent_idx != MAX_U32 and sparent_idx == jparent_idx:
                        continue
                    dist = haversine(slat, slon, jlat, jlon)
                    if dist <= MAX_TRANSFER_DIST:
                        secs = max(int(dist / WALK_SPEED), 30)
                        add_transfer(si, sj, secs, int(dist))
                        add_transfer(sj, si, secs, int(dist))

    return transfers


# ---------------------------------------------------------------------------
# osm walk graph
# ---------------------------------------------------------------------------

def build_walk_graph(osm_path, stop_coords, max_walk_radius=2000):
    """
    extract pedestrian-walkable graph from osm, pruned to within
    max_walk_radius meters of any transit stop.
    stop_coords: list of (lat, lon) for all stops.
    returns (nodes, edges_dict).
    """
    import osmium

    # pedestrian-priority ways: used as-is (penalty = 1x)
    PEDESTRIAN_PRIORITY = {
        "footway", "pedestrian", "path", "steps", "living_street", "cycleway",
    }
    # road-class ways: walkable but penalised to steer routing onto footpaths
    ROAD_CLASS = {
        "residential", "service", "tertiary", "secondary", "primary",
        "trunk", "unclassified", "track",
    }
    WALKABLE = PEDESTRIAN_PRIORITY | ROAD_CLASS
    # road edges get their distance multiplied by this factor so Dijkstra
    # strongly prefers pedestrian paths when both are available.
    ROAD_PENALTY = 3

    class Handler(osmium.SimpleHandler):
        def __init__(self):
            super().__init__()
            self.node_coords = {}
            self._ways = []  # list of (node_refs, highway_tag)

        def way(self, w):
            hw = w.tags.get("highway", "")
            if hw not in WALKABLE:
                return
            if w.tags.get("foot", "") == "no":
                return
            if w.tags.get("access", "") == "private":
                return
            nodes = []
            for n in w.nodes:
                nodes.append(n.ref)
                # with locations=True, node locations are available on way nodes
                if n.location.valid():
                    self.node_coords[n.ref] = (n.location.lat, n.location.lon)
            self._ways.append((nodes, hw))

    handler = Handler()
    handler.apply_file(osm_path, locations=True)

    # compact indexing
    node_idx_map = {}
    nodes = []
    for nid in sorted(handler.node_coords.keys()):
        node_idx_map[nid] = len(nodes)
        nodes.append(handler.node_coords[nid])

    edge_list = defaultdict(list)
    pedestrian_node_set = set()  # node indices touched by at least one pedestrian-priority way
    for way_nodes, hw in handler._ways:
        penalty = 1 if hw in PEDESTRIAN_PRIORITY else ROAD_PENALTY
        is_ped = hw in PEDESTRIAN_PRIORITY
        for k in range(len(way_nodes) - 1):
            a, b = way_nodes[k], way_nodes[k + 1]
            if a not in node_idx_map or b not in node_idx_map:
                continue
            ai, bi = node_idx_map[a], node_idx_map[b]
            lat1, lon1 = nodes[ai]
            lat2, lon2 = nodes[bi]
            dist = min(int(haversine(lat1, lon1, lat2, lon2)) * penalty, MAX_U16)
            edge_list[ai].append((bi, dist))
            edge_list[bi].append((ai, dist))
            if is_ped:
                pedestrian_node_set.add(ai)
                pedestrian_node_set.add(bi)

    # prune nodes far from any transit stop
    if stop_coords:
        grid_size = 0.005  # ~550m grid cells
        stop_grid = defaultdict(list)
        for slat, slon in stop_coords:
            gx, gy = int(slon / grid_size), int(slat / grid_size)
            stop_grid[(gx, gy)].append((slat, slon))

        radius_cells = int(math.ceil(max_walk_radius / 550.0))
        keep = set()
        for ni, (nlat, nlon) in enumerate(nodes):
            gx, gy = int(nlon / grid_size), int(nlat / grid_size)
            found = False
            for dx in range(-radius_cells, radius_cells + 1):
                if found:
                    break
                for dy in range(-radius_cells, radius_cells + 1):
                    for slat, slon in stop_grid.get((gx + dx, gy + dy), []):
                        if haversine(nlat, nlon, slat, slon) <= max_walk_radius:
                            found = True
                            break
                    if found:
                        break
            if found:
                keep.add(ni)

        if len(keep) < len(nodes):
            # reindex to only kept nodes
            old_to_new = {}
            new_nodes = []
            for old_i in sorted(keep):
                old_to_new[old_i] = len(new_nodes)
                new_nodes.append(nodes[old_i])
            new_edges = defaultdict(list)
            for ni in keep:
                for (to, dist) in edge_list.get(ni, []):
                    if to in keep:
                        new_edges[old_to_new[ni]].append((old_to_new[to], dist))
            pedestrian_node_set = {
                old_to_new[ni] for ni in pedestrian_node_set if ni in old_to_new
            }
            nodes = new_nodes
            edge_list = new_edges

    return nodes, edge_list, pedestrian_node_set


def compress_walk_graph(nodes, edge_list, protected_nodes):
    """
    contract degree-2 nodes to reduce graph size.
    protected_nodes is a set of node indices that must not be removed
    (e.g. nodes linked to transit stops).
    returns (new_nodes, new_edge_list) where edges may have geometry.
    new_edge_list values are (to_idx, dist, geometry) where geometry is
    a list of (lat, lon) for intermediate points (empty if direct edge).
    """
    # build adjacency as sets (dedup parallel edges)
    n = len(nodes)
    adj = defaultdict(set)
    # also track distances per edge
    edge_dist = {}
    for ni in range(n):
        for (to, dist) in edge_list.get(ni, []):
            adj[ni].add(to)
            key = (min(ni, to), max(ni, to))
            if key not in edge_dist or dist < edge_dist[key]:
                edge_dist[key] = dist

    # identify contractable nodes: degree 2, not protected
    contractable = set()
    for ni in range(n):
        if len(adj[ni]) == 2 and ni not in protected_nodes:
            contractable.add(ni)

    # trace chains of degree-2 nodes
    visited = set()
    chains = []  # list of (endpoints, chain_nodes, total_dist)

    for start in range(n):
        if start in contractable or start in visited:
            continue
        # start is a junction/endpoint/protected node
        for neighbor in adj[start]:
            if neighbor not in contractable or neighbor in visited:
                continue
            # trace the chain from start through degree-2 nodes
            chain = [start, neighbor]
            visited.add(neighbor)
            cur = neighbor
            prev = start
            total = edge_dist[(min(prev, cur), max(prev, cur))]
            while True:
                # cur is degree-2 (contractable), find next
                nexts = adj[cur] - {prev}
                if not nexts:
                    break
                nxt = next(iter(nexts))
                key = (min(cur, nxt), max(cur, nxt))
                total += edge_dist.get(key, 0)
                prev = cur
                cur = nxt
                chain.append(cur)
                if cur not in contractable:
                    break
                visited.add(cur)
            chains.append((chain, total))

    # build the new graph: only keep non-contractable nodes
    keep = [i for i in range(n) if i not in contractable]
    old_to_new = {}
    for new_i, old_i in enumerate(keep):
        old_to_new[old_i] = new_i
    new_nodes = [nodes[i] for i in keep]

    # add original edges between kept nodes (non-chain edges)
    new_edge_list = defaultdict(list)
    chain_endpoints = set()
    for chain, _ in chains:
        chain_endpoints.add((chain[0], chain[-1]))
        chain_endpoints.add((chain[-1], chain[0]))

    for ni in keep:
        for to in adj[ni]:
            if to in contractable:
                continue
            # direct edge between two kept nodes (not part of a chain)
            if (ni, to) in chain_endpoints:
                continue
            new_ni = old_to_new[ni]
            new_to = old_to_new[to]
            key = (min(ni, to), max(ni, to))
            dist = edge_dist.get(key, 0)
            new_edge_list[new_ni].append((new_to, dist, []))

    # add chain edges with geometry
    for chain, total_dist in chains:
        a, b = chain[0], chain[-1]
        if a not in old_to_new or b not in old_to_new:
            continue
        new_a = old_to_new[a]
        new_b = old_to_new[b]
        # geometry = intermediate nodes (excluding endpoints)
        geom = [nodes[i] for i in chain[1:-1]]
        dist = min(total_dist, MAX_U16)
        new_edge_list[new_a].append((new_b, dist, geom))
        new_edge_list[new_b].append((new_a, dist, list(reversed(geom))))

    return new_nodes, new_edge_list


def link_stops_to_walk_graph(stops, walk_nodes, pedestrian_node_set=None):
    """
    for each stop, find the nearest walk graph node.
    entrances (type 2) and platforms (type 0) are linked; stations (type 1)
    are linked if they have coordinates.
    When pedestrian_node_set is provided, nodes on pedestrian-priority ways
    are preferred: we first search for a pedestrian node within 500 m, and
    only fall back to any node (including road-class) if none is found.
    """
    if not walk_nodes:
        return [MAX_U32] * len(stops)

    grid = defaultdict(list)
    grid_size = 0.001
    for i, (lat, lon) in enumerate(walk_nodes):
        gx = int(lon / grid_size)
        gy = int(lat / grid_size)
        grid[(gx, gy)].append(i)

    result = []
    for s in stops:
        lt = get_location_type(s)
        slat, slon = get_lat(s), get_lon(s)
        # link platforms, entrances, and stations that have coords
        if lt not in (0, 1, 2) or (slat == 0.0 and slon == 0.0):
            result.append(MAX_U32)
            continue
        gx = int(slon / grid_size)
        gy = int(slat / grid_size)

        best_ped_idx = MAX_U32
        best_ped_dist = 500.0
        best_any_idx = MAX_U32
        best_any_dist = 500.0

        for dx in range(-2, 3):
            for dy in range(-2, 3):
                for ni in grid.get((gx + dx, gy + dy), []):
                    d = haversine(slat, slon, walk_nodes[ni][0], walk_nodes[ni][1])
                    if d < best_any_dist:
                        best_any_dist = d
                        best_any_idx = ni
                    if pedestrian_node_set is not None and ni in pedestrian_node_set:
                        if d < best_ped_dist:
                            best_ped_dist = d
                            best_ped_idx = ni

        if pedestrian_node_set is not None and best_ped_idx != MAX_U32:
            result.append(best_ped_idx)
        else:
            result.append(best_any_idx)

    return result


# ---------------------------------------------------------------------------
# capnp assembly
# ---------------------------------------------------------------------------

def build_capnp(gtfs, patterns, services, svc_id_map, transfers, walk_nodes, walk_edges, stop_walk_links):
    msg = transit_capnp.TransitData.new_message()
    msg.feedId = gtfs.agencies[0].get("agency_id", "unknown") if gtfs.agencies else "unknown"
    msg.timezone = gtfs.timezone

    # agencies
    ag_list = msg.init("agencies", len(gtfs.agencies))
    for i, a in enumerate(gtfs.agencies):
        ag_list[i].id = a.get("agency_id", "")
        ag_list[i].name = a.get("agency_name", "")
        ag_list[i].url = a.get("agency_url", "")

    # sort transfers by from_stop for contiguous indexing
    transfers.sort(key=lambda x: x[0])
    transfer_offsets = {}
    cur = 0
    for fidx, tidx, ws, dm in transfers:
        if fidx not in transfer_offsets:
            transfer_offsets[fidx] = (cur, 0)
        off, cnt = transfer_offsets[fidx]
        transfer_offsets[fidx] = (off, cnt + 1)
        cur += 1

    # build route_idxs per stop (pattern index, not gtfs route index)
    stop_routes = defaultdict(set)
    for pidx, pat in enumerate(patterns):
        for sidx in pat["stop_idxs"]:
            stop_routes[sidx].add(pidx)

    # build trip -> pattern index mapping
    trip_to_pattern = {}
    for pidx, pat in enumerate(patterns):
        for tidx, _ in pat["trip_order"]:
            trip_to_pattern[tidx] = pidx

    # stops
    s_list = msg.init("stops", len(gtfs.stops))
    for i, s in enumerate(gtfs.stops):
        s_list[i].id = s["stop_id"]
        s_list[i].name = s.get("stop_name", "")
        s_list[i].lat = get_lat(s)
        s_list[i].lon = get_lon(s)
        s_list[i].locationType = get_location_type(s)
        parent_id = s.get("parent_station", "")
        s_list[i].parentIdx = gtfs.stop_id_map.get(parent_id, MAX_U32) if parent_id else MAX_U32
        s_list[i].platformCode = s.get("platform_code", "")
        s_list[i].zoneId = s.get("zone_id", "")
        toff, tnum = transfer_offsets.get(i, (0, 0))
        s_list[i].transfersOffset = toff
        s_list[i].numTransfers = min(tnum, MAX_U16)
        s_list[i].walkNodeIdx = stop_walk_links[i]
        rlist = sorted(stop_routes.get(i, []))
        ri = s_list[i].init("routeIdxs", len(rlist))
        for k, r in enumerate(rlist):
            ri[k] = r

    # routes: one entry per pattern (not per gtfs route)
    all_stop_times = []
    r_list = msg.init("routes", len(patterns))
    for pidx, pat in enumerate(patterns):
        gidx = pat["gtfs_route_idx"]
        r = gtfs.routes[gidx]
        r_list[pidx].id = r["route_id"]
        r_list[pidx].shortName = r.get("route_short_name", "")
        r_list[pidx].longName = r.get("route_long_name", "")
        r_list[pidx].type = int(r.get("route_type", "3") or "3")
        r_list[pidx].color = parse_color(r.get("route_color", ""))
        r_list[pidx].textColor = parse_color(r.get("route_text_color", ""))
        aid = r.get("agency_id", "")
        r_list[pidx].agencyIdx = gtfs.agency_id_map.get(aid, 0)

        si = r_list[pidx].init("stopIdxs", len(pat["stop_idxs"]))
        for k, sidx in enumerate(pat["stop_idxs"]):
            si[k] = sidx

        r_list[pidx].stopTimesOffset = len(all_stop_times)
        r_list[pidx].numTrips = len(pat["trip_order"])

        ti = r_list[pidx].init("tripIdxs", len(pat["trip_order"]))
        for k, (tidx, _) in enumerate(pat["trip_order"]):
            ti[k] = tidx
            trip = gtfs.trips[tidx]
            sts = gtfs.stop_times_by_trip.get(trip["trip_id"], [])
            for st in sts:
                all_stop_times.append({
                    "arrival": parse_time(st.get("arrival_time", "")),
                    "departure": parse_time(st.get("departure_time", "")),
                    "pickup_type": int(st.get("pickup_type", "0") or "0"),
                    "drop_off_type": int(st.get("drop_off_type", "0") or "0"),
                })

    st_arrivals = b""
    st_departures = b""
    for st in all_stop_times:
        st_arrivals += struct.pack("<I", st["arrival"])
        st_departures += struct.pack("<I", st["departure"])
    msg.stopArrivals = st_arrivals
    msg.stopDepartures = st_departures

    # transfers
    tf_list = msg.init("transfers", len(transfers))
    for i, (fidx, tidx, ws, dm) in enumerate(transfers):
        tf_list[i].toStopIdx = tidx
        tf_list[i].walkSeconds = min(ws, MAX_U16)
        tf_list[i].distMeters = min(dm, MAX_U16)

    # services
    svc_list = msg.init("services", len(services))
    for i, (sid, start_days, bits) in enumerate(services):
        svc_list[i].id = sid
        svc_list[i].startDate = min(start_days, MAX_U16)
        svc_list[i].dayBits = bits

    # trips: routeIdx points to pattern index
    tr_list = msg.init("trips", len(gtfs.trips))
    for i, t in enumerate(gtfs.trips):
        tr_list[i].id = t["trip_id"]
        tr_list[i].routeIdx = trip_to_pattern.get(i, MAX_U32)
        sid = t.get("service_id", "")
        tr_list[i].serviceIdx = svc_id_map.get(sid, MAX_U32)
        tr_list[i].headsign = t.get("trip_headsign", "")
        tr_list[i].directionId = int(t.get("direction_id", "0") or "0")

        # write frequency rules if this is a template trip
        freq_entries = gtfs.frequencies.get(t["trip_id"], [])
        if freq_entries:
            rules = tr_list[i].init("frequencyRules", len(freq_entries))
            for fi, fe in enumerate(freq_entries):
                rules[fi].startTime = parse_time(fe.get("start_time", ""))
                rules[fi].endTime = parse_time(fe.get("end_time", ""))
                rules[fi].headwaySecs = int(fe.get("headway_secs", "0") or "0")
                rules[fi].exactTimes = fe.get("exact_times", "0") == "1"
            # compute templateFirstDeparture from first stop_time
            sts = gtfs.stop_times_by_trip.get(t["trip_id"], [])
            if sts:
                first_dep = parse_time(sts[0].get("departure_time", "")
                                       or sts[0].get("arrival_time", ""))
                tr_list[i].templateFirstDeparture = first_dep if first_dep != MAX_U32 else 0

    # fare rules: map gtfs route idx to all pattern indices that reference it
    gtfs_route_to_patterns = defaultdict(list)
    for pidx, pat in enumerate(patterns):
        gtfs_route_to_patterns[pat["gtfs_route_idx"]].append(pidx)

    fr_out = []
    for fr in gtfs.fare_rules:
        fid = fr.get("fare_id", "")
        fa = gtfs.fare_attrs.get(fid)
        if not fa:
            continue
        rid = fr.get("route_id", "")
        price_f = float(fa.get("price", "0") or "0")
        currency = fa.get("currency_type", "")
        price = int(round(price_f * 100))
        if rid:
            gidx = gtfs.route_id_map.get(rid)
            if gidx is not None:
                for pidx in gtfs_route_to_patterns.get(gidx, []):
                    fr_out.append((pidx, fr.get("origin_id", ""), fr.get("destination_id", ""), price, currency))
            # if route not found, skip
        else:
            # applies to all routes
            fr_out.append((MAX_U32, fr.get("origin_id", ""), fr.get("destination_id", ""), price, currency))

    fr_list = msg.init("fareRules", len(fr_out))
    for i, (ridx, oz, dz, price, cur) in enumerate(fr_out):
        fr_list[i].routeIdx = ridx
        fr_list[i].originZone = oz
        fr_list[i].destZone = dz
        fr_list[i].price = price
        fr_list[i].currency = cur

    # walk graph
    wg = msg.init("walkGraph")
    flat_edges = []
    edge_offsets = []
    flat_geometry = b""  # packed float32 lat,lon pairs
    geom_pair_offset = 0  # current offset in coord pairs
    for ni in range(len(walk_nodes)):
        offset = len(flat_edges)
        node_edges = walk_edges.get(ni, [])
        edge_offsets.append((offset, len(node_edges)))
        for edge in node_edges:
            if len(edge) == 3:
                to_idx, dist, geom = edge
            else:
                to_idx, dist = edge
                geom = []
            g_off = geom_pair_offset
            g_len = len(geom)
            if geom:
                for glat, glon in geom:
                    flat_geometry += struct.pack("<ff", glat, glon)
                geom_pair_offset += g_len
            flat_edges.append((to_idx, dist, g_off, g_len))

    wg.geometry = flat_geometry

    wn_list = wg.init("nodes", len(walk_nodes))
    for i, (lat, lon) in enumerate(walk_nodes):
        wn_list[i].lat = lat
        wn_list[i].lon = lon
        off, num = edge_offsets[i] if i < len(edge_offsets) else (0, 0)
        wn_list[i].edgesOffset = off
        wn_list[i].numEdges = min(num, MAX_U16)

    we_list = wg.init("edges", len(flat_edges))
    for i, (to_idx, dist, g_off, g_len) in enumerate(flat_edges):
        we_list[i].toNodeIdx = to_idx
        we_list[i].distMeters = dist
        we_list[i].geometryOffset = g_off
        we_list[i].geometryLen = min(g_len, MAX_U16)

    return msg


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="build .wheelsrouter from gtfs + osm")
    parser.add_argument("--gtfs", required=True, help="path to gtfs zip file")
    parser.add_argument("--osm", required=False, help="path to osm pbf file (optional)")
    parser.add_argument("-o", "--output", required=True, help="output .wheelsrouter file path")
    args = parser.parse_args()

    print("reading gtfs...")
    gtfs = GtfsData(args.gtfs)
    n_platforms = sum(1 for s in gtfs.stops if get_location_type(s) == 0)
    n_stations = sum(1 for s in gtfs.stops if get_location_type(s) == 1)
    n_entrances = sum(1 for s in gtfs.stops if get_location_type(s) == 2)
    print(f"  {len(gtfs.stops)} stops ({n_platforms} platforms, {n_stations} stations, {n_entrances} entrances)")
    print(f"  {len(gtfs.routes)} routes, {len(gtfs.trips)} trips, {len(gtfs.services)} services")

    print("building route patterns...")
    patterns = build_route_patterns(gtfs)
    total_trips = sum(len(p["trip_order"]) for p in patterns)
    print(f"  {len(patterns)} route patterns, {total_trips} trips included")

    print("building service bitfields...")
    services, svc_id_map = build_service_bitfields(gtfs)
    print(f"  {len(services)} services")

    print("computing transfers...")
    transfers = compute_transfers(gtfs)
    print(f"  {len(transfers)} transfers")

    walk_nodes, walk_edges, walk_ped_nodes = [], {}, set()
    if args.osm:
        print("reading osm pedestrian graph...")
        stop_coords = [(get_lat(s), get_lon(s)) for s in gtfs.stops if get_location_type(s) in (0, 1, 2)]
        walk_nodes, walk_edges, walk_ped_nodes = build_walk_graph(args.osm, stop_coords)
        print(f"  {len(walk_nodes)} walk nodes, {sum(len(v) for v in walk_edges.values())} walk edges")
    else:
        print("no osm file provided, skipping walk graph")

    print("linking stops to walk graph...")
    stop_walk_links = link_stops_to_walk_graph(gtfs.stops, walk_nodes, walk_ped_nodes if walk_nodes else None)
    linked = sum(1 for x in stop_walk_links if x != MAX_U32)
    print(f"  {linked}/{len(gtfs.stops)} stops linked")

    if walk_nodes:
        print("compressing walk graph...")
        protected = set(x for x in stop_walk_links if x != MAX_U32)
        raw_nodes = len(walk_nodes)
        raw_edges = sum(len(v) for v in walk_edges.values())
        walk_nodes, walk_edges = compress_walk_graph(walk_nodes, walk_edges, protected)
        new_edges = sum(len(v) for v in walk_edges.values())
        print(f"  {raw_nodes} -> {len(walk_nodes)} nodes, {raw_edges} -> {new_edges} edges")
        # remap stop_walk_links to new node indices
        old_to_new = {}
        # rebuild old_to_new by checking which old indices were kept
        # the compress function keeps nodes not in contractable set
        # we need to re-link stops after compression
        stop_walk_links = link_stops_to_walk_graph(gtfs.stops, walk_nodes)
        linked = sum(1 for x in stop_walk_links if x != MAX_U32)
        print(f"  {linked}/{len(gtfs.stops)} stops linked after reindex")

    print("assembling capnp message...")
    msg = build_capnp(gtfs, patterns, services, svc_id_map, transfers, walk_nodes, walk_edges, stop_walk_links)

    print(f"writing {args.output}...")
    with open(args.output, "wb") as f:
        msg.write(f)

    size = os.path.getsize(args.output)
    print(f"done. output size: {size / 1024 / 1024:.1f} mb")


if __name__ == "__main__":
    main()
