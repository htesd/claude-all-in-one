//! Cursor 官方登录流的端到端验证:开浏览器授权 → 轮询拿凭据 → 立刻用它发一次真请求。
//!
//! 用法:
//!   cargo run -p gw-cursor --example login
//!
//! **不打印凭据明文**,只打印长度与前缀。拿到的 token 用完即弃(不落盘)。
use gw_cursor::login::{self, PollOutcome};

#[tokio::main]
async fn main() {
    let flow = login::start();
    println!("\n① 在浏览器打开下面这个地址，登录并点「授权」：\n");
    println!("   {}\n", flow.login_url);
    println!(
        "② 等你点完，这里会自动拿到凭据（每 {}s 轮询一次，最多等 {} 分钟）\n",
        login::POLL_INTERVAL_SECS,
        login::POLL_INTERVAL_SECS * login::POLL_MAX_ATTEMPTS as u64 / 60
    );

    let client = reqwest::Client::new();
    let mut got = None;
    for i in 1..=login::POLL_MAX_ATTEMPTS {
        match login::poll_once(&client, &flow).await {
            Ok(PollOutcome::Done {
                access_token,
                refresh_token,
                auth_id,
            }) => {
                println!("\n✅ 登录成功（第 {i} 次轮询）");
                println!(
                    "   access_token : {} 字符，前缀 {}…",
                    access_token.len(),
                    &access_token[..access_token.len().min(12)]
                );
                println!(
                    "   refresh_token: {} 字符，前缀 {}…",
                    refresh_token.len(),
                    &refresh_token[..refresh_token.len().min(12)]
                );
                println!("   两者相同     : {}", access_token == refresh_token);
                println!("   auth_id      : {}", auth_id.as_deref().unwrap_or("(无)"));
                if let Some(exp) = gw_cursor::auth::token_expires_at(&access_token) {
                    println!(
                        "   有效期至     : {}",
                        gw_cursor::auth::format_unix_utc(exp)
                    );
                }
                got = Some(access_token);
                break;
            }
            Ok(PollOutcome::Pending) => {
                if i % 5 == 0 {
                    print!(".");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
            Err(e) => {
                eprintln!("\n❌ 轮询失败: {e:?}");
                std::process::exit(1);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(login::POLL_INTERVAL_SECS)).await;
    }

    let Some(_token) = got else {
        eprintln!("\n⏱  超时:没等到授权");
        std::process::exit(1);
    };
    println!("\n③ 这份凭据可直接填进后台的 Cursor 建号表单（access_token / refresh_token）。");
}
