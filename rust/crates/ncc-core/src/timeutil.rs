//! 时间与时区。
//!
//! 关键约束是**与既有 SQLite 数据互通**：GORM（glebarez/sqlite 驱动）把
//! `time.Time` 落成 TEXT，形如 `2026-09-07 01:16:25.887763+08:00`。
//! 所以本模块所有解析/格式化都围绕这个格式，而不是天真的 RFC3339 ——
//! 用 RFC3339 解析会得到「时间格式不对」的 500，而且是读写都能跑、只有某几条
//! 老数据炸的那种最难查的错。

use chrono::{DateTime, FixedOffset, Local, SecondsFormat, TimeZone, Utc};

/// Go 侧 GORM 落库的时间格式（小数位按需、带时区偏移）。
const GO_DB_FMT: &str = "%Y-%m-%d %H:%M:%S%.f%:z";

/// 当前时间（本地时区），按 GORM 格式字符串化，可直接写进 datetime 列。
pub fn now_go() -> String {
    format_go(Local::now().fixed_offset())
}

/// 按 GORM 格式格式化时间点。
pub fn format_go(t: DateTime<FixedOffset>) -> String {
    t.format(GO_DB_FMT).to_string()
}

/// 解析库里 / 请求里的时间串。认这些形态：
///
/// * `%Y-%m-%d %H:%M:%S%.f%:z`（本仓 GORM 落库格式）
/// * RFC3339 / RFC3339 带 Z（客户端与 HTTP 头常用）
/// * `%Y-%m-%d %H:%M:%S%.f`（无时区，按本机时区解释）
/// * `%Y-%m-%d`（纯日期）
pub fn parse_time(s: &str) -> Option<DateTime<FixedOffset>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_str(s, GO_DB_FMT) {
        return Some(t);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t);
    }
    if let Ok(t) = DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%z") {
        return Some(t);
    }
    let offset = *Local::now().offset();
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return offset.from_local_datetime(&ndt).single();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return offset.from_local_datetime(&ndt).single();
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = d.and_hms_opt(0, 0, 0)?;
        return offset.from_local_datetime(&ndt).single();
    }
    None
}

/// 解析失败时回落成「很久以前」。
///
/// 用途：在线/离线判断。老数据或脏数据不该让一个节点看起来是「刚刚在线」——
/// 那会把流量导到一个根本没心跳的节点上。
pub fn parse_time_or_epoch(s: &str) -> DateTime<FixedOffset> {
    parse_time(s).unwrap_or_else(|| {
        Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0)
            .single()
            .expect("epoch 合法")
            .fixed_offset()
    })
}

/// HTTP 响应里的时间戳：UTC、RFC3339 秒精度（`2026-10-08T02:24:06Z`）。
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// 当前 Unix 秒。
pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析_gorm_格式() {
        let t = parse_time("2026-09-07 01:16:25.887763+08:00").unwrap();
        assert_eq!(t.timestamp(), 1788714985);
        assert_eq!(format_go(t), "2026-09-07 01:16:25.887763+08:00");
    }

    #[test]
    fn 解析_无小数与_rfc3339() {
        assert!(parse_time("2026-09-07 01:16:25+08:00").is_some());
        assert!(parse_time("2026-09-07T01:16:25Z").is_some());
        assert!(parse_time("2026-09-07").is_some());
        assert!(parse_time("").is_none());
        assert!(parse_time("不是时间").is_none());
    }
}
