fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 用 vendored protoc，免去本地与 CI 各自安装 protoc 的步骤。
    if std::env::var_os("PROTOC").is_none() {
        if let Ok(path) = protoc_bin_vendored::protoc_bin_path() {
            std::env::set_var("PROTOC", path);
        }
    }

    // 本仓库里 crate 根就是仓库根，proto 就在同级目录下。
    //
    // 原写法是 `CARGO_MANIFEST_DIR.ancestors().nth(2)`（crate 位于
    // `<workspace>/crates/<name>` 时指向 workspace 根）。独立成仓后那层层级
    // 没有了，**漏改这里报的是「proto 找不到」**，看不出是路径问题。
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("proto");

    let protos = [
        "common.proto",
        "storage.proto",
        "plugin.proto",
        "host.proto",
    ];
    let inputs: Vec<std::path::PathBuf> = protos.iter().map(|name| root.join(name)).collect();

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&inputs, &[&root])?;

    for proto in &inputs {
        println!("cargo:rerun-if-changed={}", proto.display());
    }
    println!("cargo:rerun-if-changed={}", root.display());
    Ok(())
}
