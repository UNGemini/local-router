fn main() {
    capnpc::CompilerCommand::new()
        .file("schema/transit.capnp")
        .run()
        .expect("capnp compile failed");
}
