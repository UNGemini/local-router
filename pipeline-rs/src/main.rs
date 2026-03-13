mod gtfs;
mod osm;
mod output;
mod transfers;
mod walk;
pub mod transit_capnp {
    include!(concat!(env!("OUT_DIR"), "/transit_capnp.rs"));
}

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "build", about = "Convert GTFS + OSM to .wheelsrouter")]
struct Args {
    #[arg(long, help = "Path to GTFS zip file")]
    gtfs: PathBuf,

    #[arg(long, help = "Path to OSM PBF file (optional)")]
    osm: Option<PathBuf>,

    #[arg(short, long, help = "Output .wheelsrouter file path")]
    output: PathBuf,
}

fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn bar(len: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.cyan} {msg} [{bar:35.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn main() -> Result<()> {
    let args = Args::parse();

    // ---- GTFS ----
    let pb = spinner("reading gtfs");
    let gtfs = gtfs::GtfsData::load(&args.gtfs, &pb).context("failed to load GTFS")?;
    let n_platforms = gtfs.stops.iter().filter(|s| s.location_type == 0).count();
    let n_stations = gtfs.stops.iter().filter(|s| s.location_type == 1).count();
    let n_entrances = gtfs.stops.iter().filter(|s| s.location_type == 2).count();
    pb.finish_with_message(format!(
        "gtfs loaded  {} stops  {} routes  {} trips  {} services",
        gtfs.stops.len(),
        gtfs.routes.len(),
        gtfs.trips.len(),
        gtfs.services.len(),
    ));
    if n_stations > 0 || n_entrances > 0 {
        eprintln!(
            "  ({} platforms, {} stations, {} entrances)",
            n_platforms, n_stations, n_entrances
        );
    }

    // ---- Route patterns ----
    let pb = spinner("building route patterns");
    let patterns = gtfs::build_route_patterns(&gtfs);
    let total_trips: usize = patterns.iter().map(|p| p.trip_order.len()).sum();
    pb.finish_with_message(format!(
        "route patterns  {} patterns  {} trips",
        patterns.len(),
        total_trips
    ));

    // ---- Service bitfields ----
    let pb = spinner("building service bitfields");
    let (services, svc_id_map) = gtfs::build_service_bitfields(&gtfs);
    pb.finish_with_message(format!("service bitfields  {} services", services.len()));

    // ---- Transfers ----
    let max_transfer_dist = if args.osm.is_some() {
        transfers::MAX_TRANSFER_DIST_DEFAULT
    } else {
        transfers::MAX_TRANSFER_DIST_NO_OSM
    };
    let pb = bar(gtfs.stops.len() as u64, "computing transfers");
    let xfers = transfers::compute_transfers(&gtfs, max_transfer_dist, &pb);
    pb.finish_with_message(format!("transfers  {} entries", xfers.len()));

    // ---- OSM walk graph ----
    let (walk_nodes, walk_edges, walk_ped_set) = if let Some(osm_path) = &args.osm {
        let pb = spinner("reading osm pedestrian graph");
        let stop_coords: Vec<(f64, f64)> = gtfs
            .stops
            .iter()
            .filter(|s| s.location_type <= 2)
            .map(|s| (s.stop_lat as f64, s.stop_lon as f64))
            .collect();
        let (nodes, edges, ped_set) = osm::build_walk_graph(osm_path, &stop_coords, 2000.0)
            .context("failed to build walk graph")?;
        let total_edges: usize = edges.values().map(|v| v.len()).sum();
        pb.finish_with_message(format!(
            "osm graph  {} nodes  {} edges",
            nodes.len(),
            total_edges
        ));
        (nodes, edges, ped_set)
    } else {
        eprintln!("  no osm file — skipping walk graph (transfer radius: 3 km)");
        (
            vec![],
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        )
    };

    // ---- Link stops ----
    let pb = bar(gtfs.stops.len() as u64, "linking stops to walk graph");
    let mut stop_walk_links = walk::link_stops_to_walk_graph(
        &gtfs.stops,
        &walk_nodes,
        if walk_nodes.is_empty() {
            None
        } else {
            Some(&walk_ped_set)
        },
        &pb,
    );
    let linked = stop_walk_links.iter().filter(|&&x| x != u32::MAX).count();
    pb.finish_with_message(format!(
        "stop links  {}/{} linked",
        linked,
        gtfs.stops.len()
    ));

    // ---- Compress walk graph ----
    let (walk_nodes, walk_edges) = if !walk_nodes.is_empty() {
        let raw_nodes = walk_nodes.len();
        let raw_edges: usize = walk_edges.values().map(|v| v.len()).sum();
        let protected: std::collections::HashSet<u32> = stop_walk_links
            .iter()
            .filter(|&&x| x != u32::MAX)
            .copied()
            .collect();
        let pb = spinner("compressing walk graph");
        let (new_nodes, new_edges) =
            walk::compress_walk_graph(&walk_nodes, &walk_edges, &protected);
        let new_edges_count: usize = new_edges.values().map(|v| v.len()).sum();
        pb.finish_with_message(format!(
            "walk graph compressed  {} → {} nodes  {} → {} edges",
            raw_nodes,
            new_nodes.len(),
            raw_edges,
            new_edges_count
        ));
        // re-link stops to compressed graph
        let pb2 = bar(gtfs.stops.len() as u64, "re-linking stops");
        stop_walk_links = walk::link_stops_to_walk_graph(&gtfs.stops, &new_nodes, None, &pb2);
        let linked2 = stop_walk_links.iter().filter(|&&x| x != u32::MAX).count();
        pb2.finish_with_message(format!(
            "stop links  {}/{} linked after reindex",
            linked2,
            gtfs.stops.len()
        ));
        (new_nodes, new_edges)
    } else {
        (walk_nodes, walk_edges)
    };

    // ---- Assemble + write ----
    let pb = spinner("assembling capnp message");
    let msg_bytes = output::build_capnp(
        &gtfs,
        &patterns,
        &services,
        &svc_id_map,
        &xfers,
        &walk_nodes,
        &walk_edges,
        &stop_walk_links,
    )
    .context("failed to build capnp message")?;
    pb.finish_with_message(format!(
        "capnp assembled  {:.1} mb",
        msg_bytes.len() as f64 / 1024.0 / 1024.0
    ));

    let pb = spinner(format!("writing {}", args.output.display()).as_str());
    std::fs::write(&args.output, &msg_bytes)
        .with_context(|| format!("failed to write {}", args.output.display()))?;
    pb.finish_with_message(format!(
        "written {}  ({:.1} mb)",
        args.output.display(),
        msg_bytes.len() as f64 / 1024.0 / 1024.0
    ));

    Ok(())
}
