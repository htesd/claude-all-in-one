//! 离线复现:从 JSON 文件构建 inference 请求并落盘 protobuf 字节。
//! 用法: cargo run -p gw-cursor --example dump_inference_req -- <payload.json> <out.bin>

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&args[1]).unwrap()).unwrap();
    let model = std::env::var("MODEL").unwrap_or_else(|_| "grok-4.6".into());
    let conv = std::env::var("CONV_ID").unwrap_or_else(|_| "conv-test".into());
    let bytes = gw_cursor::inference::build_request(&body, &model, &conv, false).unwrap();
    std::fs::write(&args[2], &bytes).unwrap();
    println!("{} bytes -> {}", bytes.len(), args[2]);
}
