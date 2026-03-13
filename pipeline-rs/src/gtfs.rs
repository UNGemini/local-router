/// GTFS loading, route-pattern extraction, service bitfield encoding,
/// and frequency expansion.
///
/// Key differences from the Python version:
///  - stop_times are streamed straight into per-trip Vecs; no intermediate
///    list-of-dicts is ever kept alive.
///  - All string IDs are interned to u32 indices immediately on first sight,
///    so the hot data path works with plain integer arithmetic.
///  - flat_geometry accumulation uses Vec<u8> (O(1) amortised), not `bytes +=`.
use anyhow::{Context, Result};
use indicatif::ProgressBar;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read};
use std::path::Path;

pub const NOT_SET: u32 = u32::MAX;
const EARTH_RADIUS_M: f64 = 6_371_000.0;

// ---------------------------------------------------------------------------
// Haversine
// ---------------------------------------------------------------------------

#[inline]
pub fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let rlat1 = lat1.to_radians();
    let rlat2 = lat2.to_radians();
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + rlat1.cos() * rlat2.cos() * (dlon / 2.0).sin().powi(2);
    EARTH_RADIUS_M * 2.0 * a.sqrt().asin()
}

// ---------------------------------------------------------------------------
// Time/date helpers
// ---------------------------------------------------------------------------

/// Parse "HH:MM:SS" or "H:MM:SS" to seconds since midnight.
/// Returns NOT_SET on empty/invalid input.
pub fn parse_time(s: &str) -> u32 {
    let s = s.trim();
    if s.is_empty() {
        return NOT_SET;
    }
    let mut parts = s.splitn(3, ':');
    let h: u32 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(NOT_SET);
    let m: u32 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(NOT_SET);
    let sec: u32 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(NOT_SET);
    if h == NOT_SET || m == NOT_SET || sec == NOT_SET {
        return NOT_SET;
    }
    h * 3600 + m * 60 + sec
}

/// Parse "YYYYMMDD" to days since Unix epoch.
pub fn parse_date(s: &str) -> Option<u32> {
    if s.len() < 8 {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let mo: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    // days since 1970-01-01, using the proleptic Gregorian calendar
    Some(gregorian_to_epoch_days(y, mo, d))
}

/// Days from epoch (1970-01-01) using the proleptic Gregorian calendar.
fn gregorian_to_epoch_days(y: i32, m: u32, d: u32) -> u32 {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let z: i32 = if m <= 2 { y - 1 } else { y };
    let era: i32 = if z >= 0 { z } else { z - 399 } / 400;
    let yoe: u32 = (z - era * 400) as u32;
    let doy: u32 = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe: u32 = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days: i32 = era * 146097 + doe as i32 - 719468;
    days as u32
}

/// Return weekday index (0=Mon … 6=Sun) for epoch day.
fn weekday_of(epoch_day: u32) -> usize {
    // 1970-01-01 was a Thursday (index 3)
    ((epoch_day + 3) % 7) as usize
}

fn _secs_to_timestr(secs: u32) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn parse_color(s: &str) -> u32 {
    let s = s.trim().trim_start_matches('#');
    if s.is_empty() {
        return 0;
    }
    u32::from_str_radix(s, 16).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Raw GTFS row types (just the fields we need)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RawAgency {
    pub agency_id: String,
    pub agency_name: String,
    pub agency_url: String,
    pub agency_timezone: String,
}

#[derive(Debug, Clone)]
pub struct RawStop {
    pub stop_id: String,
    pub stop_name: String,
    pub stop_lat: f32,
    pub stop_lon: f32,
    pub location_type: u8,
    pub parent_station: String,
    pub platform_code: String,
    pub zone_id: String,
}

#[derive(Debug, Clone)]
pub struct RawRoute {
    pub route_id: String,
    pub agency_id: String,
    pub short_name: String,
    pub long_name: String,
    pub route_type: u8,
    pub color: u32,
    pub text_color: u32,
}

#[derive(Debug, Clone)]
pub struct RawService {
    pub service_id: String,
    pub start_date: u32, // epoch days
    pub end_date: u32,
    pub days: [bool; 7],     // Mon=0 … Sun=6
    pub additions: Vec<u32>, // epoch days
    pub removals: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct RawPathway {
    pub from_stop_id: String,
    pub to_stop_id: String,
    pub traversal_time: u32, // seconds
    pub is_bidirectional: bool,
}

#[derive(Debug, Clone)]
pub struct RawTransfer {
    pub from_stop_id: String,
    pub to_stop_id: String,
    pub transfer_type: u8,
    pub min_transfer_time: u32, // seconds, 0 if not set
}

#[derive(Debug, Clone)]
pub struct FrequencyEntry {
    pub trip_id: String,
    pub start_time: u32,
    pub end_time: u32,
    pub headway_secs: u32,
    pub exact_times: bool,
}

#[derive(Debug, Clone)]
pub struct RawFareAttr {
    pub fare_id: String,
    pub price: f64,
    pub currency_type: String,
}

#[derive(Debug, Clone)]
pub struct RawFareRule {
    pub fare_id: String,
    pub route_id: String, // empty = all routes
    pub origin_id: String,
    pub destination_id: String,
}

// ---------------------------------------------------------------------------
// Processed GTFS data
// ---------------------------------------------------------------------------

/// A concrete trip (after optional frequency expansion).
#[derive(Debug, Clone)]
pub struct Trip {
    pub trip_id: String,
    pub route_id: String,
    pub service_id: String,
    pub headsign: String,
    pub direction_id: u8,
    /// Non-empty only for frequency-template trips that are kept as templates.
    pub frequency_rules: Vec<FrequencyEntry>,
    pub template_first_departure: u32,
}

/// Flattened stop times for one trip, sorted by stop_sequence.
#[derive(Debug, Clone)]
pub struct TripStopTimes {
    pub stop_idxs: Vec<u32>,
    pub arrivals: Vec<u32>,
    pub departures: Vec<u32>,
}

pub struct GtfsData {
    pub timezone: String,
    pub feed_id: String,

    pub agencies: Vec<RawAgency>,
    pub agency_id_map: HashMap<String, usize>,

    pub stops: Vec<RawStop>,
    pub stop_id_map: HashMap<String, u32>,

    pub routes: Vec<RawRoute>,
    pub route_id_map: HashMap<String, u32>,

    pub trips: Vec<Trip>,
    pub trip_id_map: HashMap<String, u32>,

    /// For each trip (by trip_id), the sorted stop-time rows.
    pub stop_times: HashMap<String, TripStopTimes>,

    pub services: HashMap<String, RawService>,
    pub pathways: Vec<RawPathway>,
    pub raw_transfers: Vec<RawTransfer>,
    pub fare_attrs: HashMap<String, RawFareAttr>,
    pub fare_rules: Vec<RawFareRule>,
}

// ---------------------------------------------------------------------------
// CSV reading helpers
// ---------------------------------------------------------------------------

/// Helper struct for header-indexed CSV access.
struct CsvRows {
    headers: csv::StringRecord,
    rows: Vec<csv::StringRecord>,
}

impl CsvRows {
    fn get<'a>(&'a self, row: &'a csv::StringRecord, field: &str) -> &'a str {
        if let Some(pos) = self.headers.iter().position(|h| h == field) {
            row.get(pos).unwrap_or("").trim()
        } else {
            ""
        }
    }
}

fn open_zip_file(path: &Path) -> Result<zip::ZipArchive<BufReader<std::fs::File>>> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    zip::ZipArchive::new(BufReader::new(f))
        .with_context(|| format!("reading zip {}", path.display()))
}

/// Read a CSV file from a ZIP, returning headers + all rows buffered in memory.
/// Safe for small files (agency, stops, routes, trips, calendar, etc.).
fn read_csv(zip: &mut zip::ZipArchive<BufReader<std::fs::File>>, name: &str) -> Result<CsvRows> {
    let mut file = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => {
            return Ok(CsvRows {
                headers: csv::StringRecord::new(),
                rows: vec![],
            });
        }
        Err(e) => return Err(anyhow::anyhow!("{e}")).context(format!("opening {name} in zip")),
    };
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .context(format!("reading {name}"))?;
    let start = if contents.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(&contents[start..]);
    let headers = rdr
        .headers()
        .context(format!("reading headers of {name}"))?
        .clone();
    let mut rows = Vec::new();
    for (i, result) in rdr.records().enumerate() {
        rows.push(result.with_context(|| format!("row {} in {}", i + 2, name))?);
    }
    Ok(CsvRows { headers, rows })
}

/// Stream stop_times.txt from the zip row by row, directly populating
/// per-trip buffers. Never holds the full file in memory.
fn stream_stop_times(
    zip: &mut zip::ZipArchive<BufReader<std::fs::File>>,
    trip_id_map: &HashMap<String, u32>,
    stop_id_map: &HashMap<String, u32>,
    pb: &ProgressBar,
) -> Result<HashMap<u32, Vec<(u32, u32, u32, u32)>>> {
    let file = match zip.by_name("stop_times.txt") {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(HashMap::new()),
        Err(e) => return Err(anyhow::anyhow!("{e}")).context("opening stop_times.txt in zip"),
    };

    // Wrap in a reader that strips a leading UTF-8 BOM if present.
    struct BomStripReader<R: Read> {
        inner: R,
        checked: bool,
    }
    impl<R: Read> Read for BomStripReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.checked {
                self.checked = true;
                let mut bom = [0u8; 3];
                let n = self.inner.read(&mut bom)?;
                if n >= 3 && bom[..3] == [0xef, 0xbb, 0xbf] {
                    // BOM consumed — start fresh
                } else {
                    // Copy what we read into the output buffer
                    let copy = n.min(buf.len());
                    buf[..copy].copy_from_slice(&bom[..copy]);
                    if copy < n {
                        // unlikely: buf smaller than 3 bytes, put back the rest
                        // (this path is practically unreachable with csv's buffering)
                    }
                    return Ok(copy);
                }
            }
            self.inner.read(buf)
        }
    }

    let reader = BomStripReader {
        inner: BufReader::new(file),
        checked: false,
    };

    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);

    let headers = rdr
        .headers()
        .context("reading headers of stop_times.txt")?
        .clone();

    let col = |name: &str| -> Option<usize> { headers.iter().position(|h| h == name) };
    let c_trip = col("trip_id").context("stop_times.txt missing trip_id column")?;
    let c_stop = col("stop_id").context("stop_times.txt missing stop_id column")?;
    let c_seq = col("stop_sequence").unwrap_or(usize::MAX);
    let c_arr = col("arrival_time").unwrap_or(usize::MAX);
    let c_dep = col("departure_time").unwrap_or(usize::MAX);

    // trip_idx -> Vec<(seq, stop_idx, arr, dep)>
    let mut raw: HashMap<u32, Vec<(u32, u32, u32, u32)>> =
        HashMap::with_capacity(trip_id_map.len());

    let mut record = csv::StringRecord::new();
    let mut row_num = 1usize;
    while rdr
        .read_record(&mut record)
        .with_context(|| format!("stop_times.txt row {}", row_num + 1))?
    {
        row_num += 1;
        if row_num % 100_000 == 0 {
            pb.set_message(format!(
                "reading stop_times ({:.1}M rows)",
                row_num as f64 / 1_000_000.0
            ));
        }
        let tid = record.get(c_trip).unwrap_or("").trim();
        let sid = record.get(c_stop).unwrap_or("").trim();
        let tidx = match trip_id_map.get(tid) {
            Some(&i) => i,
            None => continue,
        };
        let sidx = match stop_id_map.get(sid) {
            Some(&i) => i,
            None => continue,
        };
        let seq: u32 = if c_seq == usize::MAX {
            0
        } else {
            record.get(c_seq).unwrap_or("0").trim().parse().unwrap_or(0)
        };
        let arr = if c_arr == usize::MAX {
            NOT_SET
        } else {
            parse_time(record.get(c_arr).unwrap_or("").trim())
        };
        let dep = if c_dep == usize::MAX {
            NOT_SET
        } else {
            parse_time(record.get(c_dep).unwrap_or("").trim())
        };
        raw.entry(tidx).or_default().push((seq, sidx, arr, dep));
    }
    Ok(raw)
}

// ---------------------------------------------------------------------------
// GtfsData::load
// ---------------------------------------------------------------------------

impl GtfsData {
    pub fn load(path: &Path, pb: &ProgressBar) -> Result<Self> {
        let mut zip = open_zip_file(path)?;

        // ---- agencies ----
        pb.set_message("reading agency.txt");
        let ag_csv = read_csv(&mut zip, "agency.txt")?;
        let mut agencies: Vec<RawAgency> = Vec::new();
        let mut agency_id_map: HashMap<String, usize> = HashMap::new();
        for row in &ag_csv.rows {
            let id = ag_csv.get(row, "agency_id").to_string();
            let name = ag_csv.get(row, "agency_name").to_string();
            let url = ag_csv.get(row, "agency_url").to_string();
            let tz = ag_csv.get(row, "agency_timezone").to_string();
            agency_id_map.insert(id.clone(), agencies.len());
            agencies.push(RawAgency {
                agency_id: id,
                agency_name: name,
                agency_url: url,
                agency_timezone: tz,
            });
        }
        // single-agency feed with no agency_id: map "" -> 0
        if agencies.len() == 1 && !agency_id_map.contains_key("") {
            agency_id_map.insert(String::new(), 0);
        }
        let timezone = agencies
            .first()
            .map(|a| a.agency_timezone.clone())
            .unwrap_or_else(|| "UTC".to_string());
        let feed_id = agencies
            .first()
            .map(|a| a.agency_id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());

        // ---- stops ----
        pb.set_message("reading stops.txt");
        let st_csv = read_csv(&mut zip, "stops.txt")?;
        let mut stops: Vec<RawStop> = Vec::new();
        let mut stop_id_map: HashMap<String, u32> = HashMap::new();
        for row in &st_csv.rows {
            let id = st_csv.get(row, "stop_id");
            if id.is_empty() {
                continue;
            }
            let lat: f32 = st_csv.get(row, "stop_lat").parse().unwrap_or(0.0);
            let lon: f32 = st_csv.get(row, "stop_lon").parse().unwrap_or(0.0);
            let lt: u8 = st_csv.get(row, "location_type").parse().unwrap_or(0);
            stop_id_map.insert(id.to_string(), stops.len() as u32);
            stops.push(RawStop {
                stop_id: id.to_string(),
                stop_name: st_csv.get(row, "stop_name").to_string(),
                stop_lat: lat,
                stop_lon: lon,
                location_type: lt,
                parent_station: st_csv.get(row, "parent_station").to_string(),
                platform_code: st_csv.get(row, "platform_code").to_string(),
                zone_id: st_csv.get(row, "zone_id").to_string(),
            });
        }

        // ---- routes ----
        pb.set_message("reading routes.txt");
        let rt_csv = read_csv(&mut zip, "routes.txt")?;
        let mut routes: Vec<RawRoute> = Vec::new();
        let mut route_id_map: HashMap<String, u32> = HashMap::new();
        for row in &rt_csv.rows {
            let id = rt_csv.get(row, "route_id");
            if id.is_empty() {
                continue;
            }
            let rt: u8 = rt_csv.get(row, "route_type").parse().unwrap_or(3);
            route_id_map.insert(id.to_string(), routes.len() as u32);
            routes.push(RawRoute {
                route_id: id.to_string(),
                agency_id: rt_csv.get(row, "agency_id").to_string(),
                short_name: rt_csv.get(row, "route_short_name").to_string(),
                long_name: rt_csv.get(row, "route_long_name").to_string(),
                route_type: rt,
                color: parse_color(rt_csv.get(row, "route_color")),
                text_color: parse_color(rt_csv.get(row, "route_text_color")),
            });
        }

        // ---- trips ----
        pb.set_message("reading trips.txt");
        let tr_csv = read_csv(&mut zip, "trips.txt")?;
        let mut trips: Vec<Trip> = Vec::new();
        let mut trip_id_map: HashMap<String, u32> = HashMap::new();
        for row in &tr_csv.rows {
            let tid = tr_csv.get(row, "trip_id");
            let rid = tr_csv.get(row, "route_id");
            if tid.is_empty() || !route_id_map.contains_key(rid) {
                continue;
            }
            let dir: u8 = tr_csv.get(row, "direction_id").parse().unwrap_or(0);
            trip_id_map.insert(tid.to_string(), trips.len() as u32);
            trips.push(Trip {
                trip_id: tid.to_string(),
                route_id: rid.to_string(),
                service_id: tr_csv.get(row, "service_id").to_string(),
                headsign: tr_csv.get(row, "trip_headsign").to_string(),
                direction_id: dir,
                frequency_rules: vec![],
                template_first_departure: 0,
            });
        }

        // ---- stop_times (streamed row-by-row to avoid loading 5+ GB into RAM) ----
        pb.set_message("reading stop_times.txt");
        let raw_stop_times = stream_stop_times(&mut zip, &trip_id_map, &stop_id_map, pb)
            .context("failed to stream stop_times.txt")?;

        // Sort each trip's stop times and build TripStopTimes
        let mut stop_times: HashMap<String, TripStopTimes> =
            HashMap::with_capacity(raw_stop_times.len());
        for (tidx, mut rows) in raw_stop_times {
            rows.sort_unstable_by_key(|r| r.0);
            let trip_id = trips[tidx as usize].trip_id.clone();
            let n = rows.len();
            let mut st = TripStopTimes {
                stop_idxs: Vec::with_capacity(n),
                arrivals: Vec::with_capacity(n),
                departures: Vec::with_capacity(n),
            };
            for (_seq, sidx, arr, dep) in rows {
                st.stop_idxs.push(sidx);
                st.arrivals.push(arr);
                st.departures.push(dep);
            }
            stop_times.insert(trip_id, st);
        }

        // ---- calendar ----
        pb.set_message("reading calendar.txt");
        let cal_csv = read_csv(&mut zip, "calendar.txt")?;
        let mut services: HashMap<String, RawService> = HashMap::new();
        for row in &cal_csv.rows {
            let sid = cal_csv.get(row, "service_id");
            if sid.is_empty() {
                continue;
            }
            let start = parse_date(cal_csv.get(row, "start_date")).unwrap_or(0);
            let end = parse_date(cal_csv.get(row, "end_date")).unwrap_or(0);
            let days_fields = [
                "monday",
                "tuesday",
                "wednesday",
                "thursday",
                "friday",
                "saturday",
                "sunday",
            ];
            let mut days = [false; 7];
            for (i, f) in days_fields.iter().enumerate() {
                days[i] = cal_csv.get(row, f) == "1";
            }
            services.insert(
                sid.to_string(),
                RawService {
                    service_id: sid.to_string(),
                    start_date: start,
                    end_date: end,
                    days,
                    additions: vec![],
                    removals: vec![],
                },
            );
        }

        // ---- calendar_dates ----
        pb.set_message("reading calendar_dates.txt");
        let cd_csv = read_csv(&mut zip, "calendar_dates.txt")?;
        for row in &cd_csv.rows {
            let sid = cd_csv.get(row, "service_id");
            if sid.is_empty() {
                continue;
            }
            let d = match parse_date(cd_csv.get(row, "date")) {
                Some(d) => d,
                None => continue,
            };
            let etype: u8 = cd_csv.get(row, "exception_type").parse().unwrap_or(0);
            let svc = services
                .entry(sid.to_string())
                .or_insert_with(|| RawService {
                    service_id: sid.to_string(),
                    start_date: d,
                    end_date: d,
                    days: [false; 7],
                    additions: vec![],
                    removals: vec![],
                });
            if etype == 1 {
                svc.additions.push(d);
                if d > svc.end_date {
                    svc.end_date = d;
                }
                if d < svc.start_date {
                    svc.start_date = d;
                }
            } else if etype == 2 {
                svc.removals.push(d);
            }
        }

        // ---- pathways ----
        pb.set_message("reading pathways.txt");
        let pw_csv = read_csv(&mut zip, "pathways.txt")?;
        let mut pathways: Vec<RawPathway> = Vec::new();
        for row in &pw_csv.rows {
            let from = pw_csv.get(row, "from_stop_id");
            let to = pw_csv.get(row, "to_stop_id");
            if from.is_empty() || to.is_empty() {
                continue;
            }
            let mut tt: u32 = pw_csv.get(row, "traversal_time").parse().unwrap_or(0);
            if tt == 0 {
                let length: f32 = pw_csv.get(row, "length").parse().unwrap_or(0.0);
                if length > 0.0 {
                    tt = (length / 1.2) as u32;
                }
            }
            if tt == 0 {
                tt = 120;
            }
            let bidir = pw_csv.get(row, "is_bidirectional") == "1";
            pathways.push(RawPathway {
                from_stop_id: from.to_string(),
                to_stop_id: to.to_string(),
                traversal_time: tt,
                is_bidirectional: bidir,
            });
        }

        // ---- transfers ----
        pb.set_message("reading transfers.txt");
        let tf_csv = read_csv(&mut zip, "transfers.txt")?;
        let mut raw_transfers: Vec<RawTransfer> = Vec::new();
        for row in &tf_csv.rows {
            let from = tf_csv.get(row, "from_stop_id");
            let to = tf_csv.get(row, "to_stop_id");
            if from.is_empty() || to.is_empty() {
                continue;
            }
            let ttype: u8 = tf_csv.get(row, "transfer_type").parse().unwrap_or(0);
            let mtt: u32 = tf_csv.get(row, "min_transfer_time").parse().unwrap_or(0);
            raw_transfers.push(RawTransfer {
                from_stop_id: from.to_string(),
                to_stop_id: to.to_string(),
                transfer_type: ttype,
                min_transfer_time: mtt,
            });
        }

        // ---- frequencies ----
        pb.set_message("reading frequencies.txt");
        let fr_csv = read_csv(&mut zip, "frequencies.txt")?;
        let mut freq_by_trip: HashMap<String, Vec<FrequencyEntry>> = HashMap::new();
        for row in &fr_csv.rows {
            let tid = fr_csv.get(row, "trip_id");
            if !trip_id_map.contains_key(tid) {
                continue;
            }
            let start = parse_time(fr_csv.get(row, "start_time"));
            let end = parse_time(fr_csv.get(row, "end_time"));
            let hw: u32 = fr_csv.get(row, "headway_secs").parse().unwrap_or(0);
            let exact = fr_csv.get(row, "exact_times") == "1";
            freq_by_trip
                .entry(tid.to_string())
                .or_default()
                .push(FrequencyEntry {
                    trip_id: tid.to_string(),
                    start_time: start,
                    end_time: end,
                    headway_secs: hw,
                    exact_times: exact,
                });
        }

        // ---- fare attributes ----
        pb.set_message("reading fare_attributes.txt");
        let fa_csv = read_csv(&mut zip, "fare_attributes.txt")?;
        let mut fare_attrs: HashMap<String, RawFareAttr> = HashMap::new();
        for row in &fa_csv.rows {
            let fid = fa_csv.get(row, "fare_id");
            if fid.is_empty() {
                continue;
            }
            let price: f64 = fa_csv.get(row, "price").parse().unwrap_or(0.0);
            let cur = fa_csv.get(row, "currency_type").to_string();
            fare_attrs.insert(
                fid.to_string(),
                RawFareAttr {
                    fare_id: fid.to_string(),
                    price,
                    currency_type: cur,
                },
            );
        }

        // ---- fare rules ----
        pb.set_message("reading fare_rules.txt");
        let frul_csv = read_csv(&mut zip, "fare_rules.txt")?;
        let mut fare_rules: Vec<RawFareRule> = Vec::new();
        for row in &frul_csv.rows {
            let fid = frul_csv.get(row, "fare_id");
            if fid.is_empty() {
                continue;
            }
            fare_rules.push(RawFareRule {
                fare_id: fid.to_string(),
                route_id: frul_csv.get(row, "route_id").to_string(),
                origin_id: frul_csv.get(row, "origin_id").to_string(),
                destination_id: frul_csv.get(row, "destination_id").to_string(),
            });
        }

        // ---- frequency expansion ----
        pb.set_message("expanding frequencies");
        // For frequency-based trips, we either expand into concrete trips
        // (exact_times=1) OR keep them as templates with FrequencyRules stored.
        // We match the Python behaviour: expand all into concrete trips,
        // remove the template's stop_times.
        let mut data = GtfsData {
            timezone,
            feed_id,
            agencies,
            agency_id_map,
            stops,
            stop_id_map,
            routes,
            route_id_map,
            trips,
            trip_id_map,
            stop_times,
            services,
            pathways,
            raw_transfers,
            fare_attrs,
            fare_rules,
        };
        data.expand_frequencies(freq_by_trip);
        Ok(data)
    }

    // ---- frequency expansion ----
    fn expand_frequencies(&mut self, freq_by_trip: HashMap<String, Vec<FrequencyEntry>>) {
        let mut expanded = 0u32;
        let mut templates_to_remove: Vec<String> = Vec::new();

        for (tid, freq_entries) in &freq_by_trip {
            let template_st = match self.stop_times.get(tid) {
                Some(st) if st.stop_idxs.len() >= 2 => st.clone(),
                _ => continue,
            };
            let first_dep = template_st.departures[0];
            if first_dep == NOT_SET {
                continue;
            }

            // Keep template trips with their frequency rules (match Python logic:
            // store frequencyRules on the template trip)
            let tidx = match self.trip_id_map.get(tid) {
                Some(&i) => i,
                None => continue,
            };
            let template_trip = self.trips[tidx as usize].clone();
            templates_to_remove.push(tid.clone());

            for fentry in freq_entries {
                if fentry.start_time == NOT_SET
                    || fentry.end_time == NOT_SET
                    || fentry.headway_secs == 0
                {
                    continue;
                }
                let mut t = fentry.start_time;
                let mut seq = 0u32;
                while t < fentry.end_time {
                    let new_tid = format!("{tid}_freq_{seq}");
                    let new_idx = self.trips.len() as u32;
                    self.trip_id_map.insert(new_tid.clone(), new_idx);
                    let offset = t.wrapping_sub(first_dep) as i64;
                    // create new stop times shifted by offset
                    let new_st = TripStopTimes {
                        stop_idxs: template_st.stop_idxs.clone(),
                        arrivals: template_st
                            .arrivals
                            .iter()
                            .map(|&a| {
                                if a == NOT_SET {
                                    NOT_SET
                                } else {
                                    (a as i64 + offset) as u32
                                }
                            })
                            .collect(),
                        departures: template_st
                            .departures
                            .iter()
                            .map(|&d| {
                                if d == NOT_SET {
                                    NOT_SET
                                } else {
                                    (d as i64 + offset) as u32
                                }
                            })
                            .collect(),
                    };
                    self.stop_times.insert(new_tid.clone(), new_st);
                    let mut new_trip = template_trip.clone();
                    new_trip.trip_id = new_tid.clone();
                    self.trips.push(new_trip);
                    expanded += 1;
                    t += fentry.headway_secs;
                    seq += 1;
                }
            }
        }

        // Remove template stop_times (they are not real scheduled trips)
        for tid in &templates_to_remove {
            self.stop_times.remove(tid);
        }

        if expanded > 0 {
            eprintln!(
                "  expanded {} frequency templates into {} concrete trips",
                templates_to_remove.len(),
                expanded
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Route pattern extraction
// ---------------------------------------------------------------------------

pub struct RoutePattern {
    pub gtfs_route_idx: u32,
    /// Ordered stop indices for this pattern.
    pub stop_idxs: Vec<u32>,
    /// (trip_idx_in_gtfs.trips, first_departure_secs), sorted by departure.
    pub trip_order: Vec<(u32, u32)>,
}

pub fn build_route_patterns(gtfs: &GtfsData) -> Vec<RoutePattern> {
    // group trips by route
    let mut trips_by_route: HashMap<u32, Vec<u32>> = HashMap::new();
    for (tidx, trip) in gtfs.trips.iter().enumerate() {
        if let Some(&ridx) = gtfs.route_id_map.get(&trip.route_id) {
            trips_by_route.entry(ridx).or_default().push(tidx as u32);
        }
    }

    let mut patterns: Vec<RoutePattern> = Vec::new();
    let mut skipped = 0u32;

    for (ridx, trip_idxs) in &trips_by_route {
        // group by stop pattern tuple
        let mut pattern_trips: HashMap<Vec<u32>, Vec<(u32, u32)>> = HashMap::new();
        for &tidx in trip_idxs {
            let trip = &gtfs.trips[tidx as usize];
            let st = match gtfs.stop_times.get(&trip.trip_id) {
                Some(st) if st.stop_idxs.len() >= 2 => st,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            let dep = {
                let d = st.departures[0];
                if d != NOT_SET {
                    d
                } else {
                    st.arrivals[0]
                }
            };
            pattern_trips
                .entry(st.stop_idxs.clone())
                .or_default()
                .push((tidx, dep));
        }

        for (stop_pattern, mut trip_deps) in pattern_trips {
            trip_deps.sort_unstable_by_key(|&(_, dep)| dep);
            patterns.push(RoutePattern {
                gtfs_route_idx: *ridx,
                stop_idxs: stop_pattern,
                trip_order: trip_deps,
            });
        }
    }

    if skipped > 0 {
        eprintln!("  (skipped {skipped} trips with no valid stop sequence)");
    }
    patterns
}

// ---------------------------------------------------------------------------
// Service bitfield encoding
// ---------------------------------------------------------------------------

pub struct EncodedService {
    pub id: String,
    pub start_epoch_days: u32,
    pub day_bits: Vec<u8>,
}

pub fn build_service_bitfields(gtfs: &GtfsData) -> (Vec<EncodedService>, HashMap<String, u32>) {
    let mut result: Vec<EncodedService> = Vec::new();
    let mut svc_id_map: HashMap<String, u32> = HashMap::new();

    for (sid, svc) in &gtfs.services {
        let num_days = if svc.end_date >= svc.start_date {
            (svc.end_date - svc.start_date + 1) as usize
        } else {
            1
        };
        let mut bits = vec![0u8; (num_days + 7) / 8];

        let add_set: HashSet<u32> = svc.additions.iter().copied().collect();
        let rem_set: HashSet<u32> = svc.removals.iter().copied().collect();

        for i in 0..num_days {
            let day = svc.start_date + i as u32;
            let wd = weekday_of(day);
            let mut active = svc.days[wd];
            if add_set.contains(&day) {
                active = true;
            }
            if rem_set.contains(&day) {
                active = false;
            }
            if active {
                bits[i / 8] |= 1 << (i % 8);
            }
        }

        svc_id_map.insert(sid.clone(), result.len() as u32);
        result.push(EncodedService {
            id: sid.clone(),
            start_epoch_days: svc.start_date,
            day_bits: bits,
        });
    }

    (result, svc_id_map)
}
