// types matching the wheels router web api response format
// all fields use snake_case in json

use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct PlanResponse {
    pub plans: Vec<Plan>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Plan {
    pub duration_seconds: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds_min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds_max: Option<u32>,
    pub start_time: String,
    pub legs: Vec<Leg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fares_min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fares_max: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum Leg {
    #[serde(rename = "walk")]
    Walk(WalkLeg),
    #[serde(rename = "transit")]
    Transit(TransitLeg),
    #[serde(rename = "wait")]
    Wait(WaitLeg),
}

#[derive(Serialize, Clone, Debug)]
pub struct WalkLeg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Location>,
    pub duration_seconds: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_meters: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polyline: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct TransitLeg {
    pub route_options: Vec<RouteOption>,
}

#[derive(Serialize, Clone, Debug)]
pub struct WaitLeg {
    pub duration_seconds: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct RouteOption {
    pub route_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trip_id: Option<String>,
    pub route_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_long_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_short_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headsign: Option<String>,
    pub agency: AgencyInfo,
    pub mode: String,
    pub duration_seconds: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fare: Option<FareInfo>,
    pub stops: Vec<StopInfo>,
    pub from: StopInfo,
    pub to: StopInfo,
}

#[derive(Serialize, Clone, Debug)]
pub struct Location {
    pub location: LatLon,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct AgencyInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct FareInfo {
    pub base_fare: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_fare: Option<u32>,
    pub currency: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct StopInfo {
    pub location: LatLon,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrival_offset_minutes: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub departure_offset_minutes: Option<i32>,
}
