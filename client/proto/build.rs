fn main() -> std::io::Result<()> {
    let files = ["../../proto/nexodesk/v1/session.proto", "../../proto/nexodesk/v1/rendezvous.proto"];

    // SAFETY: build scripts are single-threaded, nobody reads the environment concurrently.
    unsafe {
        std::env::set_var(
            "PROTOC",
            protoc_bin_vendored::protoc_bin_path().expect("bundled protoc not available"),
        );
    }
    for file in files {
        println!("cargo:rerun-if-changed={file}");
    }
    prost_build::compile_protos(&files, &["../../proto"])
}
