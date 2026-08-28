//! 把我方构造的 CLI 形态请求帧原样落盘,供与抓包实物逐字节 diff(**不发任何请求**)。
//!
//! ```bash
//! cargo run -p gw-cursor --example dump_frames
//! ```
//!
//! 参数与 `probe_wire_v2` 完全一致(model=default、空系统提示、tz=Asia/Shanghai、cwd=/),
//! 否则 diff 出来的差异分不清是"构造不同"还是"入参不同"。

use gw_cursor::{cli, run};

const TIMEZONE: &str = "Asia/Shanghai";
const CWD: &str = "/";
const MODEL: &str = "default";
const SYSTEM: &str = "";

fn main() {
    // 固定 uuid / 时间戳:diff 时好把它们标准化掉。
    let conv = "00000000-0000-4000-8000-000000000001";
    let turn = "00000000-0000-4000-8000-000000000002";
    // 运行时拼接固定假 JWT，避免 Secret Scanner 把测试夹具误报为真实凭据。
    let token = concat!("dummy.", "eyJzdWIiOiJhdXRoMHx1c2VyX1RFU1QifQ", ".sig");

    let model = run::Model::new(MODEL);
    let catalog = cli::cli_catalog_lan();

    let frame0 = cli::build_frame0_cli(
        "记住暗号:蓝莓42。只回复「收到」两个字",
        &model,
        &catalog,
        conv,
        turn,
        cli::CliTurn::Opening,
    );
    let env = cli::build_context_frame_cli(SYSTEM, token, conv, TIMEZONE, CWD);

    std::fs::write("/tmp/ours-frame0.bin", &frame0).unwrap();
    std::fs::write("/tmp/ours-env.bin", &env).unwrap();
    println!("frame0 = {} B  →  /tmp/ours-frame0.bin", frame0.len());
    println!("env    = {} B  →  /tmp/ours-env.bin", env.len());

    // 控制帧:cli_request_frames 的第 3 帧起(前两个是 frame0 与 env)。
    let payloads = cli::cli_request_frames(&frame0, &env);
    println!("请求帧总数 = {}", payloads.len());
    for (i, (p, gz)) in payloads.iter().enumerate().skip(2) {
        let path = format!("/tmp/ours-ctrl-{:03}.bin", i);
        std::fs::write(&path, p).unwrap();
        println!("  ctrl[{i}] = {} B gzip={} → {path}", p.len(), gz);
    }
    // 模型目录单独落一份,便于与实物 .14×9 对比。
    println!(
        "\n目录条目数(menu_visible) = {}",
        catalog.iter().filter(|m| m.menu_visible).count()
    );
}
