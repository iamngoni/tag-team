fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/tagteam/v1/tagteam.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../../proto/tagteam/v1/tagteam.proto"], &["../../proto"])?;
    Ok(())
}
