@0xb1c4a7e9d3f20186;

# wheels router nano transit data schema
# this is the binary format for .wheelsrouter files
# designed for cache-friendly raptor routing on mobile/wasm

struct TransitData {
  # metadata
  feedId @0 :Text;
  timezone @1 :Text;

  # core entities - all indexed by position
  stops @2 :List(Stop);
  routes @3 :List(Route);
  trips @4 :List(Trip);
  agencies @5 :List(Agency);

  # packed stop times as raw bytes (4 bytes per u32 little-endian)
  # each route references a contiguous slice: [stopTimesOffset .. stopTimesOffset + numStopTimes)
  # within that slice, times are grouped by trip, each group has route.numStops entries
  # arrival[i] = read_u32_le(stopArrivals, i*4), departure[i] = read_u32_le(stopDepartures, i*4)
  stopArrivals @6 :Data;
  stopDepartures @11 :Data;

  # pre-computed walking transfers between nearby stops
  transfers @7 :List(Transfer);

  # calendar service bitfields
  services @8 :List(Service);

  # pedestrian graph from osm
  walkGraph @9 :WalkGraph;

  # fare data (v1 style, kept simple)
  fareRules @10 :List(FareRule);
}

struct Agency {
  id @0 :Text;
  name @1 :Text;
  url @2 :Text;
}

struct Stop {
  id @0 :Text;
  name @1 :Text;
  lat @2 :Float32;
  lon @3 :Float32;
  locationType @4 :UInt8;  # 0=stop, 1=station, 2=entrance
  parentIdx @5 :UInt32;    # index into stops list, max_val = no parent
  platformCode @6 :Text;
  zoneId @7 :Text;

  # index into transfers list: [transfersOffset .. transfersOffset + numTransfers)
  transfersOffset @8 :UInt32;
  numTransfers @9 :UInt16;

  # which routes serve this stop (indexes into routes list)
  routeIdxs @10 :List(UInt32);

  # nearest walk graph node index, max_val = none
  walkNodeIdx @11 :UInt32;
}

struct Route {
  id @0 :Text;
  shortName @1 :Text;
  longName @2 :Text;
  type @3 :UInt8;  # gtfs route_type
  color @4 :UInt32;       # rgb packed
  textColor @5 :UInt32;   # rgb packed
  agencyIdx @6 :UInt16;

  # ordered stop indexes for this route pattern
  stopIdxs @7 :List(UInt32);

  # into the flat stopTimes array
  # layout: for each trip on this route, there are stopIdxs.size() consecutive StopTime entries
  # total entries = numTrips * stopIdxs.size()
  stopTimesOffset @8 :UInt32;
  numTrips @9 :UInt32;

  # trip indexes for this route (into trips list), ordered by departure time
  tripIdxs @10 :List(UInt32);
}

struct Trip {
  id @0 :Text;
  routeIdx @1 :UInt32;
  serviceIdx @2 :UInt32;  # index into services list
  headsign @3 :Text;
  directionId @4 :UInt8;
}

struct Transfer {
  # index of target stop
  toStopIdx @0 :UInt32;
  # walking time in seconds
  walkSeconds @1 :UInt16;
  # walking distance in meters
  distMeters @2 :UInt16;
}

struct Service {
  id @0 :Text;
  # start date as days since unix epoch
  startDate @1 :UInt16;
  # one bit per day from startDate, up to 1024 days (~2.8 years)
  # bit i = 1 means service runs on startDate + i
  dayBits @2 :Data;
}

struct FareRule {
  routeIdx @0 :UInt32;     # max_val = any route
  originZone @1 :Text;
  destZone @2 :Text;
  price @3 :UInt32;         # in smallest currency unit
  currency @4 :Text;
}

# pedestrian walking graph extracted from osm
struct WalkGraph {
  nodes @0 :List(WalkNode);
  edges @1 :List(WalkEdge);
  # flat packed geometry array for compressed edges
  # alternating float32 lat,lon pairs: [lat0,lon0,lat1,lon1,...]
  # edges reference slices via geometryOffset + geometryLen
  geometry @2 :Data;
}

struct WalkNode {
  lat @0 :Float32;
  lon @1 :Float32;
  # index into edges: [edgesOffset .. edgesOffset + numEdges)
  edgesOffset @2 :UInt32;
  numEdges @3 :UInt16;
}

struct WalkEdge {
  toNodeIdx @0 :UInt32;
  distMeters @1 :UInt16;
  # offset (in coord pairs, not bytes) into WalkGraph.geometry
  geometryOffset @2 :UInt32;
  # number of intermediate coordinate pairs
  geometryLen @3 :UInt16;
}
