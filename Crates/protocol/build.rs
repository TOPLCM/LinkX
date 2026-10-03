//! Proto/ 是协议单一源，编译期生成 Rust 代码（prost）
fn main() {
    let proto_dir = "../../Proto/linkx/v1";
    println!("cargo:rerun-if-changed={proto_dir}");
    prost_build::Config::new()
        .bytes(["."])
        .compile_protos(
            &[
                "common.proto",
                "notify.proto",
                "clipboard.proto",
                "file.proto",
                "heartbeat.proto",
                "config.proto",
                "media.proto",
                "device.proto",
                "album.proto",
            ]
            .iter()
            .map(|f| format!("{proto_dir}/{f}"))
            .collect::<Vec<_>>(),
            &["../../Proto"],
        )
        .expect("prost build 失败：protoc 是否在 PATH？");
}
