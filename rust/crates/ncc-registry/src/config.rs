//! 内网托管节点的配置。环境变量前缀统一 `NCCR_`（NCC Registry node），
//! 与平台的 `NCC_` 区分开 —— 两者可以同时跑在同一台机器上而互不干扰。
//!
//! 所有配置都有可用默认值：裸跑 `ncc-registry` 就是一个可用的单节点内网 Registry。

use std::path::PathBuf;
use std::time::Duration;

use ncc_core::env;

/// 节点角色。
pub const ROLE_MASTER: &str = "master";
pub const ROLE_WORKER: &str = "worker";

/// 校验角色取值。
pub fn valid_role(r: &str) -> bool {
    r == ROLE_MASTER || r == ROLE_WORKER
}

/// 远程执行引擎词表（配置里写错的引擎名要在**启动时**报错，不能静默忽略 ——
/// 静默忽略会让运维以为放行了、其实没放行）。
pub const EXEC_ENGINES: &[&str] = &["wasm", "process", "container"];

pub fn valid_exec_engine(e: &str) -> bool {
    EXEC_ENGINES.contains(&e)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub role: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub blob_dir: PathBuf,
    pub db_path: PathBuf,
    pub public_url: String,
    /// 是否托管内置控制台。
    pub console: bool,
    /// 控制台静态目录（`NCCR_CONSOLE_DIR`，默认 `./web`）。
    ///
    /// 与 Go 版的差别在这里：Go 用 `go:embed` 把控制台**打进二进制**，
    /// Rust 版暂时从目录托管 —— 所以部署时要显式给这个目录（或干脆不挂）。
    /// 在仓库里跑时可以直接指向 Go 那套前端资源：`NCCR_CONSOLE_DIR=httpapi/web`。
    pub console_dir: PathBuf,

    pub node_id: String,
    pub node_name: String,
    pub node_region: String,
    pub node_rand: String,

    pub jwt_secret: String,
    pub jwt_ttl: Duration,
    pub access_ttl: Duration,

    pub master_url: String,
    pub cluster_token: String,
    pub heartbeat_every: Duration,
    pub node_ttl: Duration,

    pub invite_code: String,
    pub cors_origins: String,

    pub p2p_stun: Vec<String>,
    pub p2p_turn: Vec<String>,
    pub p2p_serve: bool,

    pub exec_allow: Vec<String>,
    pub exec_runner: String,
    pub exec_shell: String,
    pub exec_docker: String,
    pub exec_dir: PathBuf,
    pub exec_timeout: Duration,
    pub exec_max_output: i64,
    pub exec_images: Vec<String>,
    pub exec_tags: Vec<String>,

    pub conn_allow: bool,
    pub conn_dir: PathBuf,
    pub conn_ttl: Duration,
}

impl Config {
    /// 是否需要邀请码。
    pub fn invite_required(&self) -> bool {
        !self.invite_code.trim().is_empty()
    }

    /// 邀请码是否匹配（未启用门禁时一律放行）。
    pub fn invite_allows(&self, code: &str) -> bool {
        if !self.invite_required() {
            return true;
        }
        self.invite_code
            .split(',')
            .any(|want| want.trim() == code.trim())
    }
}

/// 解析绝对路径（相对路径按 `base` 解析，base 为空则按当前工作目录）。
fn resolve_abs(base: &std::path::Path, p: &str) -> Result<PathBuf, String> {
    let p = p.trim();
    if p.is_empty() {
        return Err("路径不能为空".to_string());
    }
    let joined = if std::path::Path::new(p).is_absolute() || base.as_os_str().is_empty() {
        PathBuf::from(p)
    } else {
        base.join(p)
    };
    std::fs::canonicalize(&joined).or_else(|_| {
        // 目标还不存在时 canonicalize 会失败；用绝对化的形式兜底
        let abs = if joined.is_absolute() {
            joined.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&joined))
                .map_err(|e| format!("解析路径失败: {e}"))?
        };
        Ok(abs)
    })
}

/// 同 `resolve_abs`，但顺带把目录建出来。
fn resolve_dir(base: &std::path::Path, p: &str) -> Result<PathBuf, String> {
    let dir = resolve_abs(base, p)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录 {} 失败: {e}", dir.display()))?;
    Ok(dir)
}

/// 沿用传入值；为空则读文件；文件也没有就生成并写入。
fn persistent_secret(cur: &str, path: &std::path::Path, prefix: &str, n: usize) -> String {
    if !cur.trim().is_empty() {
        return cur.trim().to_string();
    }
    if let Ok(b) = std::fs::read_to_string(path) {
        let v = b.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    let mut v = ncc_core::crypto::rand_hex(n);
    if !prefix.is_empty() {
        v = format!("{prefix}-{v}");
    }
    let _ = ncc_core::env::ensure_parent(path);
    let _ = std::fs::write(path, &v);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    v
}

/// 取 id 末 8 位做短标识（同名多机时便于区分）。
fn short(id: &str) -> String {
    if id.len() <= 8 {
        return id.to_string();
    }
    id[id.len() - 8..].to_string()
}

/// 读取配置。会确保各目录存在，并落盘 node-id / jwt-secret，
/// 这样节点重启后身份与登录态都还在。
pub fn load() -> Result<Config, String> {
    let role = env::env_or("NCCR_ROLE", ROLE_MASTER).to_lowercase();
    if !valid_role(&role) {
        return Err(format!("NCCR_ROLE 必须是 master 或 worker，收到 {role:?}"));
    }
    let port = env::env_int("NCCR_PORT", 8282) as u16;
    let data_dir = resolve_dir(
        std::path::Path::new(""),
        &env::env_or("NCCR_DATA_DIR", "./data"),
    )?;
    let blob_dir = resolve_dir(&data_dir, &env::env_or("NCCR_BLOB_DIR", "blobs"))?;
    let db_path = resolve_abs(&data_dir, &env::env_or("NCCR_DB_PATH", "ncc-registry.db"))?;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建库文件目录失败: {e}"))?;
    }

    let mut name = env::env_or("NCCR_NODE_NAME", "");
    if name.is_empty() {
        name = match std::process::Command::new("hostname").output() {
            Ok(o) => {
                let h = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if h.is_empty() {
                    format!("ncc-node-{port}")
                } else {
                    h
                }
            }
            Err(_) => format!("ncc-node-{port}"),
        };
    }

    let exec_dir = resolve_dir(&data_dir, &env::env_or("NCCR_EXEC_DIR", "exec"))?;
    let conn_dir = resolve_dir(&data_dir, &env::env_or("NCCR_CONN_DIR", "conn"))?;

    let mut exec_allow = env::env_list("NCCR_EXEC_ALLOW");
    if exec_allow.is_empty() {
        exec_allow = vec!["wasm".to_string()];
    }
    for e in &exec_allow {
        if !valid_exec_engine(e) {
            return Err(format!(
                "NCCR_EXEC_ALLOW 里有不认识的引擎 {e:?}（可选：{}）",
                EXEC_ENGINES.join("/")
            ));
        }
    }

    let public_url = env::env_or("NCCR_PUBLIC_URL", &format!("http://localhost:{port}"))
        .trim_end_matches('/')
        .to_string();

    let node_id = persistent_secret(
        &env::env_or("NCCR_NODE_ID", ""),
        &data_dir.join("node-id"),
        "ND",
        12,
    );
    let node_rand = short(&node_id);
    let jwt_secret = persistent_secret(
        &env::env_or("NCCR_JWT_SECRET", ""),
        &data_dir.join("jwt-secret"),
        "",
        32,
    );

    let master_url = env::env_or("NCCR_MASTER_URL", "")
        .trim_end_matches('/')
        .to_string();
    if role == ROLE_WORKER && master_url.is_empty() {
        return Err(
            "worker 节点必须配置 NCCR_MASTER_URL（master 地址，如 http://10.0.0.1:8282）"
                .to_string(),
        );
    }

    Ok(Config {
        role,
        port,
        data_dir,
        blob_dir,
        db_path,
        public_url,
        console: env::env_bool("NCCR_CONSOLE", true),
        console_dir: resolve_abs(
            std::path::Path::new(""),
            &env::env_or("NCCR_CONSOLE_DIR", "./web"),
        )?,

        node_id,
        node_name: name,
        node_region: env::env_or("NCCR_NODE_REGION", ""),
        node_rand,

        jwt_secret,
        jwt_ttl: env::env_duration("NCCR_JWT_TTL", Duration::from_secs(168 * 3600)),
        access_ttl: env::env_duration("NCCR_ACCESS_TTL", Duration::from_secs(720 * 3600)),

        master_url,
        cluster_token: std::env::var("NCCR_CLUSTER_TOKEN").unwrap_or_default(),
        heartbeat_every: env::env_duration("NCCR_HEARTBEAT", Duration::from_secs(15)),
        node_ttl: env::env_duration("NCCR_NODE_TTL", Duration::from_secs(60)),

        invite_code: env::env_or("NCCR_INVITE_CODE", ""),
        cors_origins: std::env::var("NCCR_CORS_ORIGINS").unwrap_or_default(),

        p2p_stun: env::env_list("NCCR_P2P_STUN"),
        p2p_turn: env::env_list("NCCR_P2P_TURN"),
        p2p_serve: env::env_bool("NCCR_P2P_SERVE", false),

        exec_allow,
        exec_runner: env::env_or("NCCR_EXEC_RUNNER", ""),
        exec_shell: env::env_or("NCCR_EXEC_SHELL", "sh"),
        exec_docker: env::env_or("NCCR_EXEC_DOCKER", "docker"),
        exec_dir,
        exec_timeout: env::env_duration("NCCR_EXEC_TIMEOUT", Duration::from_secs(600)),
        exec_max_output: env::env_int("NCCR_EXEC_MAX_OUTPUT", 1 << 20),
        exec_images: env::env_list("NCCR_EXEC_IMAGES"),
        exec_tags: env::env_list("NCCR_EXEC_TAGS"),

        conn_allow: env::env_bool("NCCR_CONN_ALLOW", false),
        conn_dir,
        conn_ttl: env::env_duration("NCCR_CONN_TTL", Duration::from_secs(3600)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 邀请码放行规则() {
        let mut c = load().expect("默认配置可加载");
        c.invite_code = "a,b".to_string();
        assert!(c.invite_required());
        assert!(c.invite_allows(" b "));
        assert!(!c.invite_allows("c"));
        c.invite_code = String::new();
        assert!(!c.invite_required());
        assert!(c.invite_allows("任何值"));
    }

    #[test]
    fn 未放行的引擎被拒() {
        assert!(valid_exec_engine("wasm"));
        assert!(!valid_exec_engine("os-shell"));
    }
}
