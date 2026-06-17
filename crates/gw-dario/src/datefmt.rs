//! Unix 秒 → RFC3339 UTC "Z" 字符串。**必须产 "Z"**(末尾大写 Z,无 +00:00 偏移),
//! 否则 gw-app `parse_rfc3339_unix`(只剥 Z)解析失败 → has_fresh_token 静默禁刷新。
//! 移植 gw-kiro/src/token.rs:237 format_unix_utc 的等价算法,避免跨 crate 依赖。

pub fn format_rfc3339_z(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // civil_from_days(epoch 1970-01-01)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0,146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0,399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0,365]
    let mp = (5 * doy + 2) / 153; // [0,11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1,31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1,12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_unix_to_rfc3339_z() {
        // 与 gw-kiro format_unix_utc 同向量
        assert_eq!(format_rfc3339_z(1_780_531_200), "2026-06-04T00:00:00Z");
        assert_eq!(format_rfc3339_z(0), "1970-01-01T00:00:00Z");
    }
}
