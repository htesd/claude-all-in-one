//! RunInference(InferenceService 的 BiDi 方法)实弹探针。
//! 桌面 agent-host 的 runInference 走的就是它;Stream 面 tools 必 422/400,
//! 这条面可能不同。发送序列:run_request → invoke_model(内嵌完整
//! InferenceStreamRequest,含 tools)→ half-close;回包 invocation_response
//! 内嵌 InferenceStreamResponse(与 Stream 面同型)。
//! 用法: TOKEN=<jwt> [MODEL=claude-opus-5] [PAYLOAD=请求.json]
//!   cargo run -p gw-cursor --example e2e_runinference

use futures::StreamExt;
use gw_cursor::protobuf::{Reader, Value as PVal, Writer};

fn requested_model(model: &str) -> Writer {
    let mut rm = Writer::new();
    rm.string(1, model);
    rm.uint(2, 1); // max_mode
    rm
}

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(run());
}

async fn run() {
    let model = std::env::var("MODEL").unwrap_or_else(|_| "claude-opus-5".into());
    let conv = uuid::Uuid::new_v4().to_string();
    let inv = uuid::Uuid::new_v4().to_string();

    let body = match std::env::var("PAYLOAD") {
        Ok(path) => serde_json::from_str(&std::fs::read_to_string(&path).expect("读 PAYLOAD"))
            .expect("PAYLOAD 必须是合法 JSON"),
        Err(_) => serde_json::json!({
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "What is the weather in Tokyo? Use the tool."}],
            "tools": [{"name": "get_weather", "description": "Get weather for a city",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}],
        }),
    };

    // 内层 InferenceStreamRequest(复用生产构造器,tools/parameters 原样带上)。
    // 实测:invoke_model.request **不得**带 run 级字段(requested_model/
    // conversation_id/invocation_id 由 run_request 固定),只留 messages+tools。
    let full = gw_cursor::inference::build_request(&body, &model, &conv, false).unwrap();
    let mut inner_w = Writer::new();
    for (f, v) in Reader::new(&full) {
        if let (1 | 2, PVal::Len(b)) = (f, v) {
            inner_w.bytes(f, b);
        }
    }
    let inner = inner_w.into_bytes();

    // run_request{conversation_id=1, requested_model=3}
    let mut rr = Writer::new();
    rr.string(1, &conv);
    let rm = requested_model(&model);
    rr.message(3, &rm);
    // client msg: run_request=1
    let mut c1 = Writer::new();
    c1.message(1, &rr);

    // invoke_model{invocation_id=1, request=2(InferenceStreamRequest)}
    let mut im = Writer::new();
    im.string(1, &inv);
    im.bytes(2, &inner);
    let mut c2 = Writer::new();
    c2.message(2, &im);

    let token = std::env::var("TOKEN").expect("需要 TOKEN env");
    let client = reqwest::Client::new(); // h2(wire 面强制;Stream 面才钉 h1)
    let mut payload = gw_cursor::wire::frame(&c1.into_bytes());
    payload.extend_from_slice(&gw_cursor::wire::frame(&c2.into_bytes()));
    println!("request {} bytes (run_request + invoke_model)", payload.len());

    // 身份头消融:CLIENT_TYPE / NO_NS=1(不带 sand 命名空间与 ghost)可调。
    let client_type = std::env::var("CLIENT_TYPE").unwrap_or_else(|_| "sand".into());
    let no_ns = std::env::var("NO_NS").is_ok();
    let mut rb = client
        .post("https://api2.cursor.sh/aiserver.v1.InferenceService/RunInference")
        .header("content-type", "application/connect+proto")
        .header("connect-protocol-version", "1")
        .header("authorization", format!("Bearer {token}"))
        .header(
            "x-cursor-checksum",
            gw_cursor::wire::checksum(&gw_cursor::wire::default_machine_id(&token), None),
        )
        .header("x-cursor-client-type", &client_type)
        .header(
            "x-cursor-client-version",
            std::env::var("VER").unwrap_or_else(|_| "0.18.0".into()),
        )
        .header("x-request-id", uuid::Uuid::new_v4().to_string())
        .header("te", "trailers");
    if !no_ns {
        rb = rb
            .header("x-sand-box-namespace", "prod")
            .header("x-ghost-mode", "true");
    }
    let resp = rb.body(payload).send().await.unwrap();
    println!("HTTP {}", resp.status());
    let mut stream = resp.bytes_stream();
    let mut dec = gw_cursor::wire::FrameDecoder::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        dec.feed(&chunk);
        while let Ok(Some((flag, payload))) = dec.try_next_frame() {
            if flag & 0x02 != 0 {
                println!("END {}", String::from_utf8_lossy(&payload));
                continue;
            }
            let data = gw_cursor::wire::frame_payload(flag, &payload).unwrap();
            // RunInferenceServerMessage: heartbeat=1 run_ready=2 invocation_response=3 invocation_end=4
            for (f, v) in Reader::new(&data) {
                match (f, v) {
                    (1, _) => println!("heartbeat"),
                    (2, PVal::Len(b)) => println!("run_ready {} bytes", b.len()),
                    (4, PVal::Len(b)) => {
                        // invocation_end{invocation_id=1, error=2}
                        println!("invocation_end {} bytes", b.len());
                        for (ef, ev) in Reader::new(b) {
                            if let (2, PVal::Len(eb)) = (ef, ev) {
                                for (ef2, ev2) in Reader::new(eb) {
                                    if let PVal::Len(s) = ev2 {
                                        println!("  end.err f{ef2}: {}", String::from_utf8_lossy(s));
                                    } else if let PVal::Varint(n) = ev2 {
                                        println!("  end.err f{ef2}: {n}");
                                    }
                                }
                            }
                        }
                    }
                    (3, PVal::Len(b)) => {
                        // invocation_response{invocation_id=1, response=2: InferenceStreamResponse}
                        for (sf, sv) in Reader::new(b) {
                            if let (2, PVal::Len(resp)) = (sf, sv) {
                                for (rf, rv) in Reader::new(resp) {
                                    if let PVal::Len(sub) = rv {
                                        match rf {
                                            1 | 9 => {
                                                for (tf, tv) in Reader::new(sub) {
                                                    if let (1, PVal::Len(text)) = (tf, tv) {
                                                        println!("  text: {}", String::from_utf8_lossy(text));
                                                    }
                                                }
                                            }
                                            2 => {
                                                print!("  tool_call_part:");
                                                for (tf, tv) in Reader::new(sub) {
                                                    match (tf, tv) {
                                                        (2, PVal::Len(n)) => print!(" name={}", String::from_utf8_lossy(n)),
                                                        (3, PVal::Len(a)) => print!(" args={}", String::from_utf8_lossy(a)),
                                                        (4, PVal::Varint(c)) => print!(" complete={c}"),
                                                        _ => {}
                                                    }
                                                }
                                                println!();
                                            }
                                            3 | 5 => {
                                                let mut vals = Vec::new();
                                                for (uf, uv) in Reader::new(sub) {
                                                    if let PVal::Varint(n) = uv {
                                                        vals.push(format!("f{uf}={n}"));
                                                    }
                                                }
                                                println!("  usage: {}", vals.join(" "));
                                            }
                                            8 => {
                                                for (ef, ev) in Reader::new(sub) {
                                                    if let (1, PVal::Len(m)) = (ef, ev) {
                                                        println!("  ERROR: {}", String::from_utf8_lossy(m));
                                                    }
                                                }
                                            }
                                            _ => println!("  case {rf}: {} bytes", sub.len()),
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    println!("stream closed");
}
