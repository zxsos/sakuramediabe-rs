fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 用 vendored protoc，免去本地与 CI 各自安装 protoc 的步骤。
    if std::env::var_os("PROTOC").is_none() {
        if let Ok(path) = protoc_bin_vendored::protoc_bin_path() {
            std::env::set_var("PROTOC", path);
        }
    }

    // build script 的工作目录是本 crate 目录，proto 在 workspace 根的 proto/ 下。
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate 位于 <workspace>/crates/<name>")
        .to_path_buf();

    let protos = [
        "common.proto",
        "storage.proto",
        "plugin.proto",
        "host.proto",
    ];
    let inputs: Vec<std::path::PathBuf> = protos
        .iter()
        .map(|name| root.join("proto").join(name))
        .collect();

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&inputs, &[root.join("proto")])?;

    for proto in &inputs {
        println!("cargo:rerun-if-changed={}", proto.display());
    }
    println!("cargo:rerun-if-changed={}", root.join("proto").display());
    Ok(())
}
