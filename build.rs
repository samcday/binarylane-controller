fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        std::env::set_var("PROTOC", protobuf_src::protoc());
        std::env::set_var("PROTOC_INCLUDE", protobuf_src::include());
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(&["proto/externalgrpc.proto"], &["proto"])?;
    Ok(())
}
