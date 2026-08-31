fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/plugin.proto");
    prost_build::Config::new().compile_protos(&["proto/plugin.proto"], &["proto"])?;
    Ok(())
}
