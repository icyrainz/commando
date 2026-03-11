fn main() {
    capnpc::CompilerCommand::new()
        .src_prefix("../../schema")
        .file("../../schema/commando.capnp")
        .run()
        .expect("failed to compile capnp schema");
}
