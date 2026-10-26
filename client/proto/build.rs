fn main() -> std::io::Result<()> {
    let files = ["../../proto/nexo/v1/session.proto"];

    // SAFETY: lo script di build è a thread singolo, nessuno legge l'ambiente in parallelo.
    unsafe {
        std::env::set_var(
            "PROTOC",
            protoc_bin_vendored::protoc_bin_path().expect("protoc incluso non disponibile"),
        );
    }
    for file in files {
        println!("cargo:rerun-if-changed={file}");
    }
    prost_build::compile_protos(&files, &["../../proto"])
}
