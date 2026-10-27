fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/tagteam/v1/tagteam.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let vendored_include = protoc_bin_vendored::include_path()?;
    let proto = std::path::PathBuf::from("../../proto/tagteam/v1/tagteam.proto");
    let includes = [std::path::PathBuf::from("../../proto"), vendored_include];
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc);

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_with_config(config, &[proto], &includes)?;
    Ok(())
}
