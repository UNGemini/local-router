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
import osmium

# load the capnp schema
SCHEMA_PATH = os.path.join(os.path.dirname(__file__), "..", "schema", "transit.capnp")
transit_capnp = capnp.load(SCHEMA_PATH)

# constants
MAX_U32 = 0xFFFFFFFF
MAX_U16 = 0xFFFF
EARTH_RADIUS_M = 6_371_000
EPOCH = date(1970, 1, 1)
# max walking transfer distance in meters
MAX_TRANSFER_DIST = 300
# walking speed in m/s
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


class GtfsData:
    """parsed gtfs data with indexed lookups"""

    def __init__(self, gtfs_path):
        zf = zipfile.ZipFile(gtfs_path, "r")

        # agency
        self.agencies = read_csv(zf, "agency.txt")
        self.agency_id_map = {}
        for i, a in enumerate(self.agencies):
            aid = a.get("agency_id", "")
            self.agency_id_map[aid] = i

        # timezone from first agency
        self.timezone = self.agencies[0].get("agency_timezone", "UTC") if self.agencies else "UTC"

        # stops - only location_type 0 (or empty) and 1
        raw_stops = read_csv(zf, "stops.txt")
        self.stops = []
        self.stop_id_map = {}
        # first pass: index all stops
        for s in raw_stops:
            lt = int(s.get("location_type", "0") or "0")
            if lt > 2:
                continue
            idx = len(self.stops)
            self.stop_id_map[s["stop_id"]] = idx
            self.stops.append(s)

        # routes
        raw_routes = read_csv(zf, "routes.txt")
        self.route_id_map = {}
        self.routes = []
        for r in raw_routes:
            idx = len(self.routes)
            self.route_id_map[r["route_id"]] = idx
            self.routes.append(r)

        # trips
        raw_trips = read_csv(zf, "trips.txt")
        self.trip_id_map = {}
        self.trips = []
        for t in raw_trips:
            if t["route_id"] not in self.route_id_map:
                continue
            idx = len(self.trips)
            self.trip_id_map[t["trip_id"]] = idx
            self.trips.append(t)

        # stop_times - group by trip_id, sorted by stop_sequence
        raw_st = read_csv(zf, "stop_times.txt")
        self.stop_times_by_trip = defaultdict(list)
        for st in raw_st:
            tid = st["trip_id"]
            if tid not in self.trip_id_map:
                continue
            sid = st.get("stop_id", "")
            if sid not in self.stop_id_map:
                continue
            self.stop_times_by_trip[tid].append(st)
        for tid in self.stop_times_by_trip:
            self.stop_times_by_trip[tid].sort(key=lambda x: int(x["stop_sequence"]))

        # calendar
        self.services = {}
        for row in read_csv(zf, "calendar.txt"):
            sid = row["service_id"]
            self.services[sid] = {
                "start": parse_date(row["start_date"]),
                "end": parse_date(row["end_date"]),
                "days": [
                    int(row.get("monday", "0")),
                    int(row.get("tuesday", "0")),
                    int(row.get("wednesday", "0")),
                    int(row.get("thursday", "0")),
                    int(row.get("friday", "0")),
                    int(row.get("saturday", "0")),
                    int(row.get("sunday", "0")),
                ],
                "additions": set(),
                "removals": set(),
            }

        # calendar_dates
        for row in read_csv(zf, "calendar_dates.txt"):
            sid = row["service_id"]
            d = parse_date(row["date"])
            etype = int(row["exception_type"])
            if sid not in self.services:
                self.services[sid] = {
                    "start": d,
                    "end": d,
                    "days": [0, 0, 0, 0, 0, 0, 0],
                    "additions": set(),
                    "removals": set(),
                }
            if etype == 1:
                self.services[sid]["additions"].add(d)
                self.services[sid]["end"] = max(self.services[sid]["end"], d)
                self.services[sid]["start"] = min(self.services[sid]["start"], d)
            elif etype == 2:
                self.services[sid]["removals"].add(d)

        # fare_attributes + fare_rules (v1, kept simple)
        self.fare_attrs = {}
        for row in read_csv(zf, "fare_attributes.txt"):
            self.fare_attrs[row["fare_id"]] = row
        self.fare_rules = read_csv(zf, "fare_rules.txt")

        # transfers.txt
        self.raw_transfers = read_csv(zf, "transfers.txt")

        zf.close()


def build_route_patterns(gtfs):
    """
    build route patterns: for each route, determine the ordered stop sequence
    and group trips that share the same stop pattern.
    returns dict: route_idx -> { stop_idxs: [int], trip_order: [(trip_idx, first_departure)] }
    """
    # group trips by route
    trips_by_route = defaultdict(list)
    for i, t in enumerate(gtfs.trips):
        ridx = gtfs.route_id_map[t["route_id"]]
        trips_by_route[ridx].append(i)

    patterns = {}
    for ridx, trip_idxs in trips_by_route.items():
        # find the most common stop pattern
        pattern_counts = defaultdict(list)
        for tidx in trip_idxs:
            trip = gtfs.trips[tidx]
            sts = gtfs.stop_times_by_trip.get(trip["trip_id"], [])
            if not sts:
                continue
            stop_pattern = tuple(gtfs.stop_id_map[st["stop_id"]] for st in sts)
            pattern_counts[stop_pattern].append(tidx)

        if not pattern_counts:
            continue

        # use the most common pattern
        best_pattern = max(pattern_counts.keys(), key=lambda p: len(pattern_counts[p]))
        matched_trips = []
        for pattern, tids in pattern_counts.items():
            if pattern == best_pattern:
                matched_trips.extend(tids)

        # sort trips by first departure time
        trip_order = []
        for tidx in matched_trips:
            trip = gtfs.trips[tidx]
            sts = gtfs.stop_times_by_trip.get(trip["trip_id"], [])
            first_dep = parse_time(sts[0].get("departure_time", ""))
            trip_order.append((tidx, first_dep))
        trip_order.sort(key=lambda x: x[1])

        patterns[ridx] = {
            "stop_idxs": list(best_pattern),
            "trip_order": trip_order,
        }

    return patterns


def build_service_bitfields(gtfs):
    """
    convert calendar info to bitfield representation
    returns list of (service_id, start_epoch_days, bytes)
    """
    result = []
    svc_id_map = {}
    for sid, svc in gtfs.services.items():
        start = svc["start"]
        end = svc["end"]
        num_days = (end - start).days + 1
        # cap at 1024 days
        num_days = min(num_days, 1024)
        bits = bytearray((num_days + 7) // 8)

        for i in range(num_days):
            d = start + timedelta(days=i)
            active = False
            dow = d.weekday()  # 0=monday
            if svc["days"][dow]:
                active = True
            if d in svc["additions"]:
                active = True
            if d in svc["removals"]:
                active = False
            if active:
                bits[i // 8] |= 1 << (i % 8)

        idx = len(result)
        svc_id_map[sid] = idx
        result.append((sid, date_to_epoch_days(start), bytes(bits)))

    return result, svc_id_map


def compute_transfers(gtfs):
    """
    compute walking transfers between nearby stops (location_type=0 only).
    returns list of (from_stop_idx, to_stop_idx, walk_seconds, dist_meters)
    """
    # collect platform/stop positions
    stop_positions = []
    for i, s in enumerate(gtfs.stops):
        lt = int(s.get("location_type", "0") or "0")
        if lt != 0:
            stop_positions.append(None)
            continue
        lat = float(s.get("stop_lat", "0"))
        lon = float(s.get("stop_lon", "0"))
        stop_positions.append((lat, lon))

    transfers = []

    # also add explicit transfers from transfers.txt
    explicit = set()
    for t in gtfs.raw_transfers:
        fid = t.get("from_stop_id", "")
        tid = t.get("to_stop_id", "")
        if fid not in gtfs.stop_id_map or tid not in gtfs.stop_id_map:
            continue
        fidx = gtfs.stop_id_map[fid]
        tidx = gtfs.stop_id_map[tid]
        ttype = int(t.get("transfer_type", "0") or "0")
        if ttype == 3:
            continue  # not possible
        min_time = int(t.get("min_transfer_time", "0") or "0")
        if stop_positions[fidx] and stop_positions[tidx]:
            dist = haversine(*stop_positions[fidx], *stop_positions[tidx])
        else:
            dist = int(min_time * WALK_SPEED)
        walk_sec = min_time if min_time > 0 else max(int(dist / WALK_SPEED), 60)
        transfers.append((fidx, tidx, walk_sec, int(dist)))
        explicit.add((fidx, tidx))

    # auto-generate transfers for nearby stops
    # use a simple grid-based approach for speed
    grid = defaultdict(list)
    grid_size = 0.003  # ~300m in degrees
    for i, pos in enumerate(stop_positions):
        if pos is None:
            continue
        gx = int(pos[1] / grid_size)
        gy = int(pos[0] / grid_size)
        grid[(gx, gy)].append(i)

    for i, pos in enumerate(stop_positions):
        if pos is None:
            continue
        gx = int(pos[1] / grid_size)
        gy = int(pos[0] / grid_size)
        for dx in (-1, 0, 1):
            for dy in (-1, 0, 1):
                for j in grid.get((gx + dx, gy + dy), []):
                    if i >= j:
                        continue
                    if (i, j) in explicit or (j, i) in explicit:
                        continue
                    dist = haversine(pos[0], pos[1], stop_positions[j][0], stop_positions[j][1])
                    if dist <= MAX_TRANSFER_DIST:
                        walk_sec = int(dist / WALK_SPEED)
                        d = int(dist)
                        transfers.append((i, j, walk_sec, d))
                        transfers.append((j, i, walk_sec, d))

    return transfers


class WalkGraphBuilder(osmium.SimpleHandler):
    """extracts pedestrian-walkable ways from osm and builds a graph"""

    # osm highway types that are walkable
    WALKABLE = {
        "footway", "pedestrian", "path", "steps", "living_street",
        "residential", "service", "tertiary", "secondary", "primary",
        "trunk", "unclassified", "track", "cycleway",
    }

    def __init__(self):
        super().__init__()
        self.node_coords = {}
        self.edges = []
        self._needed_nodes = set()
        self._ways = []

    def way(self, w):
        hw = w.tags.get("highway", "")
        if hw not in self.WALKABLE:
            return
        # skip if explicitly not walkable
        foot = w.tags.get("foot", "")
        if foot == "no":
            return
        access = w.tags.get("access", "")
        if access == "private":
            return
        nodes = [n.ref for n in w.nodes]
        self._ways.append(nodes)
        self._needed_nodes.update(nodes)

    def node(self, n):
        if n.id in self._needed_nodes or not self._needed_nodes:
            self.node_coords[n.id] = (n.location.lat, n.location.lon)

    def build(self):
        """build compact graph after processing osm file"""
        # assign compact indexes to nodes that we actually have coords for
        node_idx_map = {}
        nodes = []
        for nid in sorted(self._needed_nodes):
            if nid not in self.node_coords:
                continue
            node_idx_map[nid] = len(nodes)
            nodes.append(self.node_coords[nid])

        # build edges
        edge_list = defaultdict(list)
        for way_nodes in self._ways:
            for k in range(len(way_nodes) - 1):
                a, b = way_nodes[k], way_nodes[k + 1]
                if a not in node_idx_map or b not in node_idx_map:
                    continue
                ai, bi = node_idx_map[a], node_idx_map[b]
                lat1, lon1 = nodes[ai]
                lat2, lon2 = nodes[bi]
                dist = int(haversine(lat1, lon1, lat2, lon2))
                if dist > MAX_U16:
                    dist = MAX_U16
                edge_list[ai].append((bi, dist))
                edge_list[bi].append((ai, dist))

        return nodes, edge_list


def link_stops_to_walk_graph(stops, walk_nodes):
    """for each stop, find the nearest walk graph node"""
    if not walk_nodes:
        return [MAX_U32] * len(stops)

    # build grid for walk nodes
    grid = defaultdict(list)
    grid_size = 0.001  # ~100m
    for i, (lat, lon) in enumerate(walk_nodes):
        gx = int(lon / grid_size)
        gy = int(lat / grid_size)
        grid[(gx, gy)].append(i)

    result = []
    for s in stops:
        lt = int(s.get("location_type", "0") or "0")
        if lt != 0:
            result.append(MAX_U32)
            continue
        slat = float(s.get("stop_lat", "0"))
        slon = float(s.get("stop_lon", "0"))
        gx = int(slon / grid_size)
        gy = int(slat / grid_size)
        best_idx = MAX_U32
        best_dist = 500  # max 500m link distance
        for dx in range(-2, 3):
            for dy in range(-2, 3):
                for ni in grid.get((gx + dx, gy + dy), []):
                    d = haversine(slat, slon, walk_nodes[ni][0], walk_nodes[ni][1])
                    if d < best_dist:
                        best_dist = d
                        best_idx = ni
        result.append(best_idx)
    return result


def build_capnp(gtfs, patterns, services, svc_id_map, transfers, walk_nodes, walk_edges, stop_walk_links):
    """assemble everything into capnp message"""
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

    # stops
    s_list = msg.init("stops", len(gtfs.stops))
    for i, s in enumerate(gtfs.stops):
        s_list[i].id = s["stop_id"]
        s_list[i].name = s.get("stop_name", "")
        s_list[i].lat = float(s.get("stop_lat", "0"))
        s_list[i].lon = float(s.get("stop_lon", "0"))
        s_list[i].locationType = int(s.get("location_type", "0") or "0")
        parent_id = s.get("parent_station", "")
        s_list[i].parentIdx = gtfs.stop_id_map.get(parent_id, MAX_U32)
        s_list[i].platformCode = s.get("platform_code", "")
        s_list[i].zoneId = s.get("zone_id", "")
        toff, tnum = transfer_offsets.get(i, (0, 0))
        s_list[i].transfersOffset = toff
        s_list[i].numTransfers = min(tnum, MAX_U16)
        s_list[i].walkNodeIdx = stop_walk_links[i]

    # build route_idxs per stop
    stop_routes = defaultdict(set)
    for ridx, pat in patterns.items():
        for sidx in pat["stop_idxs"]:
            stop_routes[sidx].add(ridx)
    for i in range(len(gtfs.stops)):
        rlist = sorted(stop_routes.get(i, []))
        ri = s_list[i].init("routeIdxs", len(rlist))
        for k, r in enumerate(rlist):
            ri[k] = r

    # build flat stop_times and routes
    all_stop_times = []
    r_list = msg.init("routes", len(gtfs.routes))
    for i, r in enumerate(gtfs.routes):
        r_list[i].id = r["route_id"]
        r_list[i].shortName = r.get("route_short_name", "")
        r_list[i].longName = r.get("route_long_name", "")
        r_list[i].type = int(r.get("route_type", "3"))
        r_list[i].color = parse_color(r.get("route_color", ""))
        r_list[i].textColor = parse_color(r.get("route_text_color", ""))
        aid = r.get("agency_id", "")
        r_list[i].agencyIdx = gtfs.agency_id_map.get(aid, 0)

        pat = patterns.get(i)
        if not pat:
            si = r_list[i].init("stopIdxs", 0)
            r_list[i].stopTimesOffset = len(all_stop_times)
            r_list[i].numTrips = 0
            r_list[i].init("tripIdxs", 0)
            continue

        si = r_list[i].init("stopIdxs", len(pat["stop_idxs"]))
        for k, sidx in enumerate(pat["stop_idxs"]):
            si[k] = sidx

        r_list[i].stopTimesOffset = len(all_stop_times)
        r_list[i].numTrips = len(pat["trip_order"])

        ti = r_list[i].init("tripIdxs", len(pat["trip_order"]))
        for k, (tidx, _) in enumerate(pat["trip_order"]):
            ti[k] = tidx
            # add stop times for this trip
            trip = gtfs.trips[tidx]
            sts = gtfs.stop_times_by_trip.get(trip["trip_id"], [])
            for st in sts:
                all_stop_times.append({
                    "arrival": parse_time(st.get("arrival_time", "")),
                    "departure": parse_time(st.get("departure_time", "")),
                    "pickup_type": int(st.get("pickup_type", "0") or "0"),
                    "drop_off_type": int(st.get("drop_off_type", "0") or "0"),
                })

    # stop times
    st_list = msg.init("stopTimes", len(all_stop_times))
    for i, st in enumerate(all_stop_times):
        st_list[i].arrival = st["arrival"]
        st_list[i].departure = st["departure"]
        st_list[i].pickupType = st["pickup_type"]
        st_list[i].dropOffType = st["drop_off_type"]

    # transfers
    t_list = msg.init("transfers", len(transfers))
    for i, (fidx, tidx, ws, dm) in enumerate(transfers):
        t_list[i].toStopIdx = tidx
        t_list[i].walkSeconds = min(ws, MAX_U16)
        t_list[i].distMeters = min(dm, MAX_U16)

    # services
    svc_list = msg.init("services", len(services))
    for i, (sid, start_days, bits) in enumerate(services):
        svc_list[i].id = sid
        svc_list[i].startDate = min(start_days, MAX_U16)
        svc_list[i].dayBits = bits

    # trips
    t_list = msg.init("trips", len(gtfs.trips))
    for i, t in enumerate(gtfs.trips):
        t_list[i].id = t["trip_id"]
        t_list[i].routeIdx = gtfs.route_id_map[t["route_id"]]
        sid = t.get("service_id", "")
        t_list[i].serviceIdx = svc_id_map.get(sid, MAX_U32)
        t_list[i].headsign = t.get("trip_headsign", "")
        t_list[i].directionId = int(t.get("direction_id", "0") or "0")

    # fare rules
    fr_out = []
    for fr in gtfs.fare_rules:
        fid = fr.get("fare_id", "")
        fa = gtfs.fare_attrs.get(fid)
        if not fa:
            continue
        rid = fr.get("route_id", "")
        ridx = gtfs.route_id_map.get(rid, MAX_U32)
        price_f = float(fa.get("price", "0"))
        currency = fa.get("currency_type", "")
        # convert to smallest unit (assume 2 decimal places)
        price = int(round(price_f * 100))
        fr_out.append((ridx, fr.get("origin_id", ""), fr.get("destination_id", ""), price, currency))

    fr_list = msg.init("fareRules", len(fr_out))
    for i, (ridx, oz, dz, price, cur) in enumerate(fr_out):
        fr_list[i].routeIdx = ridx
        fr_list[i].originZone = oz
        fr_list[i].destZone = dz
        fr_list[i].price = price
        fr_list[i].currency = cur

    # walk graph
    wg = msg.init("walkGraph")
    # flatten edges
    flat_edges = []
    edge_offsets = []
    for ni in range(len(walk_nodes)):
        offset = len(flat_edges)
        node_edges = walk_edges.get(ni, [])
        edge_offsets.append((offset, len(node_edges)))
        flat_edges.extend(node_edges)

    wn_list = wg.init("nodes", len(walk_nodes))
    for i, (lat, lon) in enumerate(walk_nodes):
        wn_list[i].lat = lat
        wn_list[i].lon = lon
        off, num = edge_offsets[i]
        wn_list[i].edgesOffset = off
        wn_list[i].numEdges = min(num, MAX_U16)

    we_list = wg.init("edges", len(flat_edges))
    for i, (to_idx, dist) in enumerate(flat_edges):
        we_list[i].toNodeIdx = to_idx
        we_list[i].distMeters = dist

    return msg


def main():
    parser = argparse.ArgumentParser(description="build .wheelsrouter from gtfs + osm")
    parser.add_argument("--gtfs", required=True, help="path to gtfs zip file")
    parser.add_argument("--osm", required=False, help="path to osm pbf file")
    parser.add_argument("-o", "--output", required=True, help="output .wheelsrouter file path")
    args = parser.parse_args()

    print("reading gtfs...")
    gtfs = GtfsData(args.gtfs)
    print(f"  {len(gtfs.stops)} stops, {len(gtfs.routes)} routes, {len(gtfs.trips)} trips")

    print("building route patterns...")
    patterns = build_route_patterns(gtfs)
    print(f"  {len(patterns)} route patterns")

    print("building service bitfields...")
    services, svc_id_map = build_service_bitfields(gtfs)
    print(f"  {len(services)} services")

    print("computing transfers...")
    transfers = compute_transfers(gtfs)
    print(f"  {len(transfers)} transfers")

    walk_nodes = []
    walk_edges = {}
    if args.osm:
        print("reading osm pedestrian graph...")
        builder = WalkGraphBuilder()
        builder.apply_file(args.osm, locations=True)
        walk_nodes, walk_edges = builder.build()
        print(f"  {len(walk_nodes)} walk nodes, {sum(len(v) for v in walk_edges.values())} walk edges")
    else:
        print("no osm file provided, skipping walk graph")

    print("linking stops to walk graph...")
    stop_walk_links = link_stops_to_walk_graph(gtfs.stops, walk_nodes)
    linked = sum(1 for x in stop_walk_links if x != MAX_U32)
    print(f"  {linked}/{len(gtfs.stops)} stops linked")

    print("assembling capnp message...")
    msg = build_capnp(gtfs, patterns, services, svc_id_map, transfers, walk_nodes, walk_edges, stop_walk_links)

    print(f"writing {args.output}...")
    with open(args.output, "wb") as f:
        msg.write(f)

    size = os.path.getsize(args.output)
    print(f"done. output size: {size / 1024 / 1024:.1f} mb")


if __name__ == "__main__":
    main()
