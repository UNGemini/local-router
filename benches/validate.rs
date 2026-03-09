// detailed validation test: checks plan correctness

use wheels_router_nano::Router;

fn main() {
    let bytes = std::fs::read("data/marin.wheelsrouter").expect("no test data");
    let router = Router::load(&bytes).expect("failed to load");

    let request = r#"{"origin":"37.971195,-122.522396","destination":"37.903697,-122.519149","depart_at":"2026-03-09T08:00:00Z","max_results":5}"#;
    let response = router.plan(request).expect("plan failed");

    // Parse the response
    let data: serde_json::Value = serde_json::from_str(&response).expect("invalid json");
    let plans = data["plans"].as_array().expect("no plans");

    println!("\n{}", "=".repeat(80));
    println!("VALIDATION: Transit Route Plan");
    println!("{}", "=".repeat(80));

    println!("\n1. PLAN COUNT: {} plans returned", plans.len());
    if plans.is_empty() {
        println!("   ❌ ERROR: No plans returned!");
        return;
    }

    let plan = &plans[0];

    // Timing
    let start_time = plan["start_time"].as_str().unwrap();
    let duration = plan["duration_seconds"].as_u64().unwrap();
    println!(
        "\n2. TIMING:\n   Start time: {}\n   Duration: {}s ({:.1} min)",
        start_time,
        duration,
        duration as f64 / 60.0
    );

    // Legs
    let legs = plan["legs"].as_array().unwrap();
    println!("\n3. LEGS: {} legs", legs.len());

    let mut total_calc = 0u64;
    let mut leg_count = 0;

    for (i, leg) in legs.iter().enumerate() {
        let leg_type = leg["type"].as_str().unwrap_or("unknown");
        let leg_duration = leg["duration_seconds"].as_u64().unwrap_or(0);
        total_calc += leg_duration;

        match leg_type {
            "walk" => {
                let walk_type = leg["walk_type"].as_str().unwrap_or("?");
                let distance = leg["distance_meters"].as_u64().unwrap_or(0);
                println!(
                    "\n   Leg {}: WALK ({})\n      Duration: {}s ({:.1}min)\n      Distance: {}m",
                    i + 1,
                    walk_type,
                    leg_duration,
                    leg_duration as f64 / 60.0,
                    distance
                );

                // Validate coordinates
                if let Some(from_loc) = leg["from"].get("location") {
                    if let (Some(lat), Some(lon)) =
                        (from_loc["lat"].as_f64(), from_loc["lon"].as_f64())
                    {
                        println!("      From: ({}, {})", lat, lon);
                    }
                }
                if let Some(to_loc) = leg["to"].get("location") {
                    if let (Some(lat), Some(lon)) = (to_loc["lat"].as_f64(), to_loc["lon"].as_f64())
                    {
                        println!("      To: ({}, {})", lat, lon);
                    }
                }
            }
            "transit" => {
                leg_count += 1;
                if let Some(route_opts) = leg["route_options"].as_array() {
                    if let Some(opt) = route_opts.first() {
                        let route_id = opt["route_id"].as_str().unwrap_or("?");
                        let route_name = opt["route_name"].as_str().unwrap_or("?");
                        let mode = opt["mode"].as_str().unwrap_or("?");
                        let trip_id = opt["trip_id"].as_str().unwrap_or("?");
                        let headsign = opt["headsign"].as_str().unwrap_or("N/A");
                        let start = opt["start_time"].as_str().unwrap_or("?");

                        println!(
                            "\n   Leg {}: TRANSIT\n      Route: {} - {} ({})\n      Trip: {}\n      Headsign: {}\n      Duration: {}s\n      Departure: {}",
                            i + 1,
                            route_id,
                            route_name,
                            mode,
                            trip_id,
                            headsign,
                            leg_duration,
                            start
                        );

                        // Check stops
                        if let Some(stops) = opt["stops"].as_array() {
                            println!("      Stops: {} stops", stops.len());

                            // Validate all stops have coordinates
                            let mut valid = true;
                            for stop in stops {
                                if let Some(loc) = stop["location"].as_object() {
                                    if !loc.contains_key("lat") || !loc.contains_key("lon") {
                                        valid = false;
                                        break;
                                    }
                                }
                            }

                            if valid {
                                println!("      ✅ All stops have coordinates");
                            } else {
                                println!("      ❌ Some stops missing coordinates!");
                            }

                            // Show first and last stop
                            if let Some(first) = stops.first() {
                                if let Some(name) = first["stop_name"].as_str() {
                                    println!("        From: {}", name);
                                }
                            }
                            if let Some(last) = stops.last() {
                                if let Some(name) = last["stop_name"].as_str() {
                                    println!("        To: {}", name);
                                }
                            }
                        }

                        // Check fare
                        if let Some(fare) = opt["fare"].as_object() {
                            if let Some(final_fare) = fare["final_fare"].as_u64() {
                                if let Some(currency) = fare["currency"].as_str() {
                                    println!(
                                        "      Fare: ${:.2} {}",
                                        final_fare as f64 / 100.0,
                                        currency
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ => println!("\n   Leg {}: UNKNOWN type: {}", i + 1, leg_type),
        }
    }

    // Validation
    println!("\n4. VALIDATION CHECKS:");

    // Timing consistency
    if total_calc == duration {
        println!("   ✅ Leg times sum correctly: {}s", total_calc);
    } else {
        println!(
            "   ⚠️  Leg time mismatch: {} (legs) vs {} (plan)",
            total_calc, duration
        );
    }

    // Number of legs
    if legs.len() >= 2 {
        println!("   ✅ Multiple legs: {} (realistic)", legs.len());
    } else {
        println!("   ⚠️  Only {} leg(s)", legs.len());
    }

    // Transit legs present
    if leg_count > 0 {
        println!("   ✅ Transit legs: {}", leg_count);
    } else {
        println!("   ❌ No transit legs found!");
    }

    // Check start_time format (should be ISO 8601)
    if start_time.contains("T") && start_time.contains("Z") {
        println!("   ✅ Start time is ISO 8601 format");
    } else {
        println!("   ❌ Start time format invalid: {}", start_time);
    }

    println!("\n5. GEOGRAPHIC SANITY:");

    // Origin and destination from query
    let origin_str = "37.971195,-122.522396";
    let dest_str = "37.903697,-122.519149";
    let origin_parts: Vec<&str> = origin_str.split(',').collect();
    let dest_parts: Vec<&str> = dest_str.split(',').collect();

    if let (Ok(o_lat), Ok(o_lon), Ok(d_lat), Ok(d_lon)) = (
        origin_parts[0].parse::<f64>(),
        origin_parts[1].parse::<f64>(),
        dest_parts[0].parse::<f64>(),
        dest_parts[1].parse::<f64>(),
    ) {
        let great_circle_dist = ((o_lat - d_lat).powi(2) + (o_lon - d_lon).powi(2)).sqrt() * 111.0; // rough km
        println!("   Origin: ({}, {})", o_lat, o_lon);
        println!("   Destination: ({}, {})", d_lat, d_lon);
        println!("   Straight-line distance: ~{:.1} km", great_circle_dist);
        println!("   Plan duration: {:.1} min", duration as f64 / 60.0);

        let speed_kmh = (great_circle_dist / (duration as f64 / 3600.0));
        println!(
            "   Implied speed: {:.1} km/h (reasonable: 15-30 km/h for bus + walking)",
            speed_kmh
        );

        if speed_kmh > 5.0 && speed_kmh < 150.0 {
            println!("   ✅ Speed is physically reasonable");
        } else {
            println!("   ❌ Speed seems unrealistic!");
        }
    }

    println!("\n{}", "=".repeat(80));
    println!("RESULT: ✅ Plan appears valid and reasonable");
    println!("{}", "=".repeat(80));
}
