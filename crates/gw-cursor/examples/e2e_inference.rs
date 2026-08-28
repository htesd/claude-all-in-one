//! inference 驱动实弹验证:用 Rust 构建的报文打真实上游。
//! 用法: TOKEN=<jwt> [MODEL=grok-4.6] [PROXY=http://...] [PAYLOAD=请求.json] [IMAGE=图片路径]
//!   [RAW_OUT=请求.bin]
//!   cargo run -p gw-cursor --example e2e_inference
//! 默认只发一个最小文本请求；PAYLOAD 可传任意请求，IMAGE 可快速构造最小图片请求。
//! RAW_OUT 会只落盘 protobuf 而不联网，方便交给生产同出口的探针验证。

use base64::Engine as _;
use futures::StreamExt;
use gw_core::provider::StreamItem;

#[tokio::main]
async fn main() {
    let model = std::env::var("MODEL").unwrap_or_else(|_| "grok-4.6".into());
    let conv = std::env::var("CONV_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

    let body = match (std::env::var("PAYLOAD"), std::env::var("IMAGE")) {
        (Ok(_), Ok(_)) => panic!("PAYLOAD 与 IMAGE 只能设置一个"),
        (Ok(path), Err(_)) => {
            serde_json::from_str(&std::fs::read_to_string(&path).expect("读取 PAYLOAD 指向的 JSON"))
                .expect("PAYLOAD 必须是合法 JSON")
        }
        (Err(_), Ok(path)) => {
            let mime = if path.to_ascii_lowercase().ends_with(".jpg")
                || path.to_ascii_lowercase().ends_with(".jpeg")
            {
                "image/jpeg"
            } else {
                "image/png"
            };
            let data = base64::engine::general_purpose::STANDARD
                .encode(std::fs::read(&path).expect("读取 IMAGE 指向的图片"));
            serde_json::json!({
                "max_tokens": 128,
                "messages": [{"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}},
                    {"type": "text", "text": "Describe the main visible content of this image in one short sentence."}
                ]}],
            })
        }
        (Err(_), Err(_)) => serde_json::json!({
            "max_tokens": 512,
            "messages": [{"role": "user", "content": "Reply with exactly one word: pong"}],
        }),
    };
    let bytes = gw_cursor::inference::build_request(&body, &model, &conv, false).unwrap();
    println!("request {} bytes", bytes.len());
    if let Ok(path) = std::env::var("RAW_OUT") {
        std::fs::write(&path, &bytes).expect("写入 RAW_OUT");
        println!("raw request -> {path}");
        return;
    }

    let token = std::env::var("TOKEN").expect("联网模式需要 TOKEN env");
    let mut client = reqwest::Client::builder().http1_only();
    if let Ok(proxy) = std::env::var("PROXY") {
        client = client.proxy(reqwest::Proxy::all(proxy).expect("PROXY 必须是合法代理 URL"));
    }
    let client = client.build().expect("构造 HTTP client");
    let resp = client
        .post("https://api2.cursor.sh/aiserver.v1.InferenceService/Stream")
        .header("content-type", "application/connect+proto")
        .header("connect-protocol-version", "1")
        .header("authorization", format!("Bearer {token}"))
        .header(
            "x-cursor-checksum",
            gw_cursor::wire::checksum(&gw_cursor::wire::default_machine_id(&token), None),
        )
        .header("x-cursor-client-type", "sand")
        .header("x-cursor-client-version", "0.18.0")
        .header("x-sand-box-namespace", "prod")
        .header("x-ghost-mode", "true")
        .header("x-request-id", uuid::Uuid::new_v4().to_string())
        .header("te", "trailers")
        .body(gw_cursor::wire::frame(&bytes))
        .send()
        .await
        .unwrap();
    println!("HTTP {}", resp.status());
    let mut stream = resp.bytes_stream();
    let mut dec = gw_cursor::wire::FrameDecoder::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        dec.feed(&chunk);
        while let Ok(Some((flag, payload))) = dec.try_next_frame() {
            if flag & 0x02 != 0 {
                println!("END {}", String::from_utf8_lossy(&payload));
            } else {
                let data = gw_cursor::wire::frame_payload(flag, &payload).unwrap();
                println!("frame flag={flag} len={}", data.len());
                for (f, v) in gw_cursor::protobuf::Reader::new(&data) {
                    if let gw_cursor::protobuf::Value::Len(sub) = v {
                        println!("  case {f}: {} bytes", sub.len());
                        if f == 1 || f == 9 {
                            for (sf, sv) in gw_cursor::protobuf::Reader::new(sub) {
                                if sf == 1 {
                                    if let gw_cursor::protobuf::Value::Len(text) = sv {
                                        println!("    {}", String::from_utf8_lossy(text));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let _ = StreamItem::UpstreamCut; // 类型占位,保持 import 有意义
}
