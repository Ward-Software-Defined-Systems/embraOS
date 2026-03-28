fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_files = &[
        "../../proto/common.proto",
        "../../proto/trust.proto",
        "../../proto/brain.proto",
        "../../proto/apid.proto",
    ];

    let includes = &["../../proto"];

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(proto_files, includes)?;

    // Re-run if any proto file changes
    for proto in proto_files {
        println!("cargo:rerun-if-changed={}", proto);
    }

    Ok(())
}
