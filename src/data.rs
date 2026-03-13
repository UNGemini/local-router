// in-memory transit data structures
// designed for cache-friendly raptor traversal
// all data is flattened into contiguous arrays indexed by position

pub const NOT_SET: u32 = u32::MAX;

#[derive(Debug)]
pub struct TransitData {
    pub feed_id: String,
    pub timezone: String,
    pub stops: Vec<Stop>,
    pub routes: Vec<Route>,
    pub trips: Vec<Trip>,
    pub agencies: Vec<Agency>,
    // packed stop time arrays, indexed by route.stop_times_offset
    // layout: for each route, numTrips * numStops consecutive u32 values
    pub arrivals: Vec<u32>,
    pub departures: Vec<u32>,
    // flat array of transfers, indexed by stop.transfers_offset
    pub transfers: Vec<Transfer>,
    pub services: Vec<Service>,
    pub walk_graph: WalkGraph,
    pub fare_rules: Vec<FareRule>,
}

#[derive(Debug, Clone)]
pub struct Agency {
    pub id: String,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct Stop {
    pub id: String,
    pub name: String,
    pub lat: f32,
    pub lon: f32,
    pub location_type: u8,
    pub parent_idx: u32,
    pub platform_code: String,
    pub zone_id: String,
    pub transfers_offset: u32,
    pub num_transfers: u16,
    // indexes of routes serving this stop
    pub route_idxs: Vec<u32>,
    pub walk_node_idx: u32,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub id: String,
    pub short_name: String,
    pub long_name: String,
    pub route_type: u8,
    pub color: u32,
    pub text_color: u32,
    pub agency_idx: u16,
    // ordered stop indexes for this route pattern
    pub stop_idxs: Vec<u32>,
    // offset into flat arrivals/departures arrays
    pub stop_times_offset: u32,
    pub num_trips: u32,
    // trip indexes ordered by departure time
    pub trip_idxs: Vec<u32>,
}

impl Route {
    // get departure time at stop position `stop_pos` for trip index `trip_num`
    #[inline]
    pub fn departure_at(&self, departures: &[u32], trip_num: u32, stop_pos: usize) -> u32 {
        let num_stops = self.stop_idxs.len();
        let idx = self.stop_times_offset as usize + (trip_num as usize * num_stops) + stop_pos;
        departures[idx]
    }

    // get arrival time at stop position `stop_pos` for trip index `trip_num`
    #[inline]
    pub fn arrival_at(&self, arrivals: &[u32], trip_num: u32, stop_pos: usize) -> u32 {
        let num_stops = self.stop_idxs.len();
        let idx = self.stop_times_offset as usize + (trip_num as usize * num_stops) + stop_pos;
        arrivals[idx]
    }
}

#[derive(Debug, Clone)]
pub struct Trip {
    pub id: String,
    pub route_idx: u32,
    pub service_idx: u32,
    pub headsign: String,
    pub direction_id: u8,
    // frequency rules: if non-empty, this is a frequency-based template trip
    pub frequency_rules: Vec<FrequencyRule>,
    // first departure time of the template stop_times (seconds since midnight)
    pub template_first_departure: u32,
}

// a single frequency rule: the trip repeats at headway_secs intervals
// between start_time and end_time
#[derive(Debug, Clone, Copy)]
pub struct FrequencyRule {
    pub start_time: u32,
    pub end_time: u32,
    pub headway_secs: u32,
    pub exact_times: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Transfer {
    pub to_stop_idx: u32,
    pub walk_seconds: u16,
    pub dist_meters: u16,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub id: String,
    pub start_date: u16, // days since epoch
    pub day_bits: Vec<u8>,
}

impl Service {
    // check if service runs on a given day (days since epoch)
    #[inline]
    pub fn runs_on(&self, epoch_day: u16) -> bool {
        if epoch_day < self.start_date {
            return false;
        }
        let offset = (epoch_day - self.start_date) as usize;
        let byte_idx = offset / 8;
        let bit_idx = offset % 8;
        if byte_idx >= self.day_bits.len() {
            return false;
        }
        (self.day_bits[byte_idx] >> bit_idx) & 1 == 1
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FareRule {
    pub route_idx: u32,
    pub price: u32,
    pub currency_hash: u32, // simple hash of currency string
}

#[derive(Debug, Default)]
pub struct WalkGraph {
    pub nodes: Vec<WalkNode>,
    // flat array of edges, indexed by node.edges_offset
    pub edges: Vec<WalkEdge>,
    // flat packed geometry: (lat, lon) pairs for compressed edges
    pub geometry: Vec<(f32, f32)>,
    // precomputed spatial grid: (gy, gx) -> list of node indices
    // cell size = 0.001 degrees (~111m), built once at load time
    pub node_grid: std::collections::HashMap<(i32, i32), Vec<u32>>,
}

#[derive(Debug, Clone, Copy)]
pub struct WalkNode {
    pub lat: f32,
    pub lon: f32,
    pub edges_offset: u32,
    pub num_edges: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct WalkEdge {
    pub to_node_idx: u32,
    pub dist_meters: u16,
    // offset into WalkGraph.geometry (in coord pairs)
    pub geometry_offset: u32,
    // number of intermediate coord pairs
    pub geometry_len: u16,
}
