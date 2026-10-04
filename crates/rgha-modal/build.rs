//! Compiles Modal's vendored public API proto with protox (no system protoc needed).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/api.proto");
    let fds = protox::compile(["api.proto"], ["proto"])?;
    tonic_prost_build::configure().build_server(false).compile_fds(fds)?;
    Ok(())
}
