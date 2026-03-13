fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("../schema")
        .file("../schema/transit.capnp")
        .run()
        .expect("capnp compile failed");
}
