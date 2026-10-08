//! 环境变量读取。与 Go 侧 `envOr` / `envInt` / `envDur` / `envBool` / `envList`
//! 一一对应，语义保持完全一致（空字符串一律视为「没设置」，回落到默认值）。

use std::path::{Path, PathBuf};

/// 读取字符串；未设置或只有空白时返回 `def`。
pub fn env_or(key: &str, def: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => def.to_string(),
    }
}

/// 读取整数；解析失败回落到 `def`（与 Go 一致：配置写错不炸，用默认值兜）。
pub fn env_int(key: &str, def: i64) -> i64 {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => v.trim().parse::<i64>().unwrap_or(def),
        _ => def,
    }
}

/// 读取时长；支持 Go 风格 `10m` / `1h30m` 与纯数字（秒）。
pub fn env_duration(key: &str, def: std::time::Duration) -> std::time::Duration {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => {
            let raw = v.trim();
            if let Some(d) = parse_go_duration(raw) {
                return d;
            }
            if let Ok(n) = raw.parse::<i64>() {
                return std::time::Duration::from_secs(n as u64);
            }
            def
        }
        _ => def,
    }
}

/// 解析 Go 风格时长字符串（`300ms` / `10s` / `1h30m` / `-5m`）。
///
/// 为什么不用 humantime：Go 的 `time.ParseDuration` 默认只认 `ns/us/µs/ms/s/m/h`
/// 与组合写法，配置表里写的就是这些，自己解析能把两边行为对齐到字符级。
pub fn parse_go_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if body == "0" {
        return Some(std::time::Duration::ZERO);
    }
    // 单位按「最长匹配优先」尝试：`300ms` 必须先认出 `ms` 再考虑 `m`+`s`，
    // 否则会被读成「300 分钟后跟一个裸 s」而整体解析失败。
    const UNITS: &[(&str, f64)] = &[
        ("ns", 1e-9),
        ("us", 1e-6),
        ("µs", 1e-6),
        ("μs", 1e-6),
        ("ms", 1e-3),
        ("s", 1.0),
        ("m", 60.0),
        ("h", 3600.0),
    ];

    let mut total_secs: f64 = 0.0;
    let mut rest = body;
    let mut seen = false;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_end == 0 {
            return None;
        }
        let (num_str, tail) = rest.split_at(num_end);
        let v: f64 = num_str.parse().ok()?;
        let (mult, used) = UNITS
            .iter()
            .find(|(u, _)| tail.starts_with(u))
            .map(|(u, m)| (*m, u.len()))?;
        total_secs += v * mult;
        seen = true;
        rest = &tail[used..];
    }
    if !seen {
        return None;
    }
    let d = std::time::Duration::from_secs_f64(total_secs);
    Some(if neg {
        std::time::Duration::ZERO // 负时长在本项目里没有语义，统一折成 0（避免 u64 下溢）
    } else {
        d
    })
}

/// 读取布尔；`1/true/yes/on` 为真，`0/false/no/off` 为假，其余回落默认值。
pub fn env_bool(key: &str, def: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => match v.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => def,
        },
        Err(_) => def,
    }
}

/// 读取逗号分隔列表（去空白、去空项）；未设置返回空。
pub fn env_list(key: &str) -> Vec<String> {
    match std::env::var(key) {
        Ok(raw) => raw
            .split(',')
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 解析目录：绝对路径直接用，相对路径按 `base` 解析（不是按当前工作目录 ——
/// 换个目录启动也不该忽地换地方），并确保目录存在。
pub fn resolve_dir(base: &Path, v: &str) -> std::io::Result<PathBuf> {
    let p = resolve_path(base, v);
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

/// 解析路径：绝对路径直接用，相对路径按 `base` 解析。不建目录。
pub fn resolve_path(base: &Path, v: &str) -> PathBuf {
    let p = Path::new(v);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// 保证路径的父目录存在。
pub fn ensure_parent(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// 读取或首次生成并落盘的密钥/ID 文件（原 Go 里 jwt-secret / node-id 的做法）。
///
/// 权限固定 0600：这些文件里是节点身份与登录态密钥，同机其他用户不该读得到。
pub fn load_or_create_secret(path: &Path, bytes: usize) -> std::io::Result<String> {
    if let Ok(v) = std::fs::read_to_string(path) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let v = crate::crypto::rand_hex(bytes);
    ensure_parent(path)?;
    std::fs::write(path, &v)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn 时长解析对齐_go() {
        assert_eq!(parse_go_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_go_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_go_duration("300ms"), Some(Duration::from_millis(300)));
        assert_eq!(parse_go_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_go_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_go_duration("-5m"), Some(Duration::from_secs(0)));
        assert_eq!(parse_go_duration("abc"), None);
        assert_eq!(parse_go_duration(""), None);
    }

    #[test]
    fn 目录解析_相对路径按基准目录() {
        let base = Path::new("/tmp/base");
        assert_eq!(resolve_path(base, "data"), PathBuf::from("/tmp/base/data"));
        assert_eq!(resolve_path(base, "/abs/data"), PathBuf::from("/abs/data"));
    }
}
