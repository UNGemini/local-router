// integration test: loads a real .wheelsrouter file and runs queries against it

use wheels_router_nano::Router;

fn load_test_router() -> Router {
    let path = "data/marin.wheelsrouter";
    let bytes = std::fs::read(path).expect("failed to read test data; run pipeline first");
    Router::load(&bytes).expect("failed to load transit data")
}

fn load_sf_router() -> Option<Router> {
    let path = "data/sf.wheelsrouter";
    let bytes = std::fs::read(path).ok()?;
    Some(Router::load(&bytes).expect("failed to load sf data"))
}

#[test]
fn test_sf_embarcadero_to_civic() {
    let router = match load_sf_router() {
        Some(r) => r,
        None => {
            eprintln!("skipping: sf data not found");
            return;
        }
    };
    // embarcadero area -> civic center area
    let request = serde_json::json!({
        "origin": "37.793557,-122.397823",
        "destination": "37.788942,-122.417393",
        "depart_at": "2026-03-09T10:00:00Z",
        "walking_speed": "normal",
        "max_results": 5
    });
    let result = router.plan(&request.to_string()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let plans = parsed["plans"].as_array().unwrap();
    println!("sf embarcadero->civic: {} plans", plans.len());
    for (i, plan) in plans.iter().enumerate() {
        let dur = plan["duration_seconds"].as_u64().unwrap();
        let legs = plan["legs"].as_array().unwrap();
        let modes: Vec<&str> = legs
            .iter()
            .map(|l| {
                if l.get("route_options").is_some() {
                    "transit"
                } else {
                    "walk"
                }
            })
            .collect();
        println!(
            "  plan {}: {}s ({} min), legs: {:?}",
            i,
            dur,
            dur / 60,
            modes
        );
    }
    assert!(
        !plans.is_empty(),
        "expected at least a walk-only plan for 1.7km trip"
    );
    // check path geometry on walk legs
    for (i, plan) in plans.iter().enumerate() {
        let legs = plan["legs"].as_array().unwrap();
        for (j, leg) in legs.iter().enumerate() {
            if let Some(wt) = leg.get("walk_type") {
                let path = leg.get("path");
                let path_len = path
                    .and_then(|p| p.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let dist = leg
                    .get("distance_meters")
                    .and_then(|d| d.as_u64())
                    .unwrap_or(0);
                let from = leg
                    .get("from")
                    .and_then(|f| f.get("address"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("?");
                let to = leg
                    .get("to")
                    .and_then(|t| t.get("address"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("?");
                println!(
                    "  plan {} leg {} ({}): path_points={}, dist={}m, {} -> {}",
                    i,
                    j,
                    wt.as_str().unwrap_or("?"),
                    path_len,
                    dist,
                    from,
                    to
                );
            }
        }
    }
}

#[test]
fn test_sf_walk_paths_complete() {
    let router = match load_sf_router() {
        Some(r) => r,
        None => {
            eprintln!("skipping: sf data not found");
            return;
        }
    };
    // test multiple queries to find path failures
    let queries = vec![
        // mission -> wharf
        ("37.758917,-122.414580", "37.808332,-122.417743"),
        // sunset -> financial district
        ("37.753611,-122.485000", "37.790000,-122.400000"),
        // richmond -> soma
        ("37.779500,-122.467000", "37.783000,-122.399000"),
        // castro -> north beach
        ("37.762200,-122.435000", "37.800500,-122.409000"),
    ];
    let mut egress_total = 0u32;
    let mut egress_missing_path = 0u32;
    let mut access_total = 0u32;
    let mut access_missing_path = 0u32;
    for (oi, (origin, dest)) in queries.iter().enumerate() {
        let request = serde_json::json!({
            "origin": origin,
            "destination": dest,
            "depart_at": "2026-03-09T09:00:00Z",
            "max_results": 5
        });
        let result = router.plan(&request.to_string()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let plans = parsed["plans"].as_array().unwrap();
        println!("query {}: {} plans", oi, plans.len());
        for (i, plan) in plans.iter().enumerate() {
            let dur = plan["duration_seconds"].as_u64().unwrap();
            let legs = plan["legs"].as_array().unwrap();
            println!("  plan {} ({}s = {} min):", i, dur, dur / 60);
            for (j, leg) in legs.iter().enumerate() {
                if let Some(wt) = leg.get("walk_type") {
                    let wt_str = wt.as_str().unwrap_or("?");
                    let path_len = leg
                        .get("path")
                        .and_then(|p| p.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let dist = leg
                        .get("distance_meters")
                        .and_then(|d| d.as_u64())
                        .unwrap_or(0);
                    let from = leg
                        .get("from")
                        .and_then(|f| f.get("address"))
                        .and_then(|a| a.as_str())
                        .unwrap_or("?");
                    let to = leg
                        .get("to")
                        .and_then(|t| t.get("address"))
                        .and_then(|a| a.as_str())
                        .unwrap_or("?");
                    println!(
                        "    leg {} ({}): {}pts, {}m, {} -> {}",
                        j, wt_str, path_len, dist, from, to
                    );
                    if path_len <= 2 && dist > 20 {
                        println!(
                            "      ^ STRAIGHT LINE (only {} points for {}m)",
                            path_len, dist
                        );
                    }
                    if wt_str == "station_egress" {
                        egress_total += 1;
                        if path_len <= 2 && dist > 20 {
                            egress_missing_path += 1;
                        }
                    }
                    if wt_str == "station_access" {
                        access_total += 1;
                        if path_len <= 2 && dist > 20 {
                            access_missing_path += 1;
                        }
                    }
                } else if let Some(ro) = leg.get("route_options") {
                    let name = ro[0]["route_name"].as_str().unwrap_or("?");
                    let mode = ro[0]["mode"].as_str().unwrap_or("?");
                    println!("    leg {} (transit): {} {}", j, mode, name);
                }
            }
        }
    }
    println!(
        "access paths: {}/{} have real geometry",
        access_total - access_missing_path,
        access_total
    );
    println!(
        "egress paths: {}/{} have real geometry",
        egress_total - egress_missing_path,
        egress_total
    );
    assert!(
        egress_missing_path == 0,
        "{} egress walks missing real path geometry",
        egress_missing_path
    );
}

#[test]
fn test_load_data() {
    let router = load_test_router();
    let stats = router.stats();
    println!("stats: {}", stats);
    // should have stops, routes, trips
    assert!(stats.contains("\"stops\""));
    assert!(stats.contains("\"routes\""));
    let parsed: serde_json::Value = serde_json::from_str(&stats).unwrap();
    let n_stops = parsed["stops"].as_u64().unwrap();
    let n_routes = parsed["routes"].as_u64().unwrap();
    assert!(n_stops > 100, "expected >100 stops, got {}", n_stops);
    assert!(n_routes > 10, "expected >10 routes, got {}", n_routes);
}

#[test]
fn test_plan_weekday_morning() {
    let router = load_test_router();
    // san rafael transit center (37.971195, -122.522396) -> e blithedale & tower dr (37.903697, -122.519149)
    // monday 2026-03-09 at 08:00
    let request = serde_json::json!({
        "origin": "37.971195,-122.522396",
        "destination": "37.903697,-122.519149",
        "depart_at": "2026-03-09T08:00:00Z",
        "max_results": 5
    });
    let result = router.plan(&request.to_string());
    match result {
        Ok(json) => {
            println!("plan result: {}", &json[..json.len().min(2000)]);
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let plans = parsed["plans"].as_array().unwrap();
            println!("found {} plans", plans.len());
            assert!(!plans.is_empty(), "expected at least one plan");

            // check first plan structure
            let plan = &plans[0];
            assert!(plan["duration_seconds"].as_u64().unwrap() > 0);
            assert!(plan["start_time"].as_str().is_some());
            let legs = plan["legs"].as_array().unwrap();
            assert!(!legs.is_empty(), "plan should have legs");

            // check that at least one leg is a transit leg
            let has_transit = legs.iter().any(|leg| leg.get("route_options").is_some());
            assert!(has_transit, "plan should contain at least one transit leg");

            // verify transit leg structure
            for leg in legs {
                if let Some(route_options) = leg.get("route_options") {
                    let opts = route_options.as_array().unwrap();
                    assert!(!opts.is_empty());
                    let opt = &opts[0];
                    assert!(opt["route_id"].as_str().is_some());
                    assert!(opt["mode"].as_str().is_some());
                    assert!(opt["duration_seconds"].as_u64().unwrap() > 0);
                    assert!(opt["stops"].as_array().unwrap().len() >= 2);
                    println!(
                        "  transit: {} ({}) - {} stops, {}s",
                        opt["route_name"].as_str().unwrap_or("?"),
                        opt["mode"].as_str().unwrap_or("?"),
                        opt["stops"].as_array().unwrap().len(),
                        opt["duration_seconds"].as_u64().unwrap()
                    );
                }
                if let Some(wt) = leg.get("walk_type") {
                    println!(
                        "  walk: {} - {}s",
                        wt.as_str().unwrap_or("?"),
                        leg["duration_seconds"].as_u64().unwrap_or(0)
                    );
                }
            }
        }
        Err(e) => {
            panic!("plan failed: {}", e);
        }
    }
}

#[test]
fn test_plan_no_results_far_away() {
    let router = load_test_router();
    // origin in marin, destination very far away (tokyo)
    let request = serde_json::json!({
        "origin": "37.971195,-122.522396",
        "destination": "35.681236,139.767125",
        "depart_at": "2026-03-09T08:00:00Z"
    });
    let result = router.plan(&request.to_string()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let plans = parsed["plans"].as_array().unwrap();
    assert!(
        plans.is_empty(),
        "should have no plans for unreachable destination"
    );
}

#[test]
fn test_plan_nearby_stops() {
    let router = load_test_router();
    // two nearby stops in san rafael area
    let request = serde_json::json!({
        "origin": "37.971195,-122.522396",
        "destination": "37.926504,-122.515248",
        "depart_at": "2026-03-09T09:00:00Z",
        "max_results": 3
    });
    let result = router.plan(&request.to_string()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let plans = parsed["plans"].as_array().unwrap();
    println!("nearby stops: {} plans found", plans.len());
    // these stops are along hwy 101, should have direct bus service
    for (i, plan) in plans.iter().enumerate() {
        let dur = plan["duration_seconds"].as_u64().unwrap();
        let legs = plan["legs"].as_array().unwrap();
        println!("  plan {}: {}s, {} legs", i, dur, legs.len());
    }
}
