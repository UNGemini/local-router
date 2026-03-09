use std::time::Instant;
use wheels_router_nano::Router;

fn main() {
    let bytes = std::fs::read("data/marin.wheelsrouter").unwrap();

    // measure load time
    let start = Instant::now();
    let router = Router::load(&bytes).unwrap();
    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!("load time: {:.2}ms", load_ms);

    let request = r#"{"origin":"37.971195,-122.522396","destination":"37.903697,-122.519149","depart_at":"2026-03-09T08:00:00Z","max_results":5}"#;

    // warmup
    let _ = router.plan(request);

    // benchmark 100 queries
    let start = Instant::now();
    for _ in 0..100 {
        let _ = router.plan(request);
    }
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    let per_query_ms = total_ms / 100.0;

    println!(
        "100 queries: {:.2}ms total, {:.3}ms per query",
        total_ms, per_query_ms
    );
}
