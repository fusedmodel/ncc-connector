//! 认证上下文与作用域模型。
//!
//! 作用域列表与蕴含关系是从 Go 侧原样搬过来的 —— 它们是**对外契约**：
//! CLI 里 `ncc key create --scopes ...` 写的就是这些字符串，改动会让既有
//! 凭据突然失效。

/// 认证上下文（JWT 会话 / 节点令牌 / API-Key）。
#[derive(Debug, Clone, Default)]
pub struct AuthInfo {
    pub user_id: String,
    pub email: String,
    /// user | node | key
    pub kind: String,
    pub key_id: String,
    /// kind=node：票据兑换出来的节点令牌绑定的节点
    pub node_id: String,
    pub scopes: Vec<String>,
    /// true = JWT 用户会话（代表用户本人，不受作用域限制）
    pub session: bool,
}

impl AuthInfo {
    pub fn is_user_session(&self) -> bool {
        self.session
    }
}

/// 新建 API-Key 的默认作用域（不含 `keys:write`，避免「一把 key 再生出更多 key」）。
pub const DEFAULT_SCOPES: &[&str] = &[
    "registry:read",
    "registry:download",
    "registry:publish",
    "nodes:read",
    "nodes:write",
    "grants:read",
    "grants:write",
    "config:read",
    "config:write",
    "trace:read",
    "trace:write",
    // 三样状态：默认给读写（它们是「我的 Agent 的状态」，新建 key 就是我的）
    "kb:read",
    "kb:write",
    "mem:read",
    "mem:write",
    "ckpt:read",
    "ckpt:write",
    // 通用记录仓：一个作用域对所有集合生效
    "store:read",
    "store:write",
    // 反馈：说自己那句（写）与看别人的话（读）分开
    "feedback:read",
    "feedback:write",
    "p2p:read",
    "p2p:write",
];

/// 索引相关作用域。刻意**不进默认作用域**：索引的权威在平台，节点侧只是副本。
pub const INDEX_SCOPES: &[&str] = &["index:read", "index:write"];

/// 接入票据兑换出的节点令牌默认作用域：能上报心跳、能看/拉公开制品，但不能发布。
pub const NODE_TICKET_SCOPES: &[&str] = &["nodes:write", "registry:read", "registry:download"];

/// 可用作用域目录。
pub const ALL_SCOPES: &[&str] = &[
    "registry:read",
    "registry:download",
    "registry:publish",
    "nodes:read",
    "nodes:write",
    "grants:read",
    "grants:write",
    "config:read",
    "config:write",
    "trace:read",
    "trace:write",
    "trace:label",
    "kb:read",
    "kb:write",
    "mem:read",
    "mem:write",
    "ckpt:read",
    "ckpt:write",
    "store:read",
    "store:write",
    "feedback:read",
    "feedback:write",
    "p2p:read",
    "p2p:write",
    "index:read",
    "index:write",
    "exec:read",
    "exec:write",
    "conn:read",
    "conn:write",
    "keys:write",
];

/// 作用域中文说明 `/key-scopes` 用。
pub fn scope_desc(sc: &str) -> &'static str {
    match sc {
        "registry:read" => "查看制品目录与详情",
        "registry:download" => "下载制品字节",
        "registry:publish" => "发布、覆盖、下架制品",
        "nodes:read" => "查看托管节点与连接",
        "nodes:write" => "注册节点、上报心跳、维护连接",
        "grants:read" => "查看授权",
        "grants:write" => "授予与撤销授权",
        "config:read" => "读取基础设施配置",
        "config:write" => "写入基础设施配置",
        "trace:read" => "查看运行轨迹",
        "trace:write" => "采集运行轨迹",
        "trace:label" => "给轨迹下判断（评测）",
        "kb:read" => "读取知识库条目",
        "kb:write" => "写入知识库条目",
        "mem:read" => "读取记忆条目",
        "mem:write" => "写入记忆条目",
        "ckpt:read" => "读取检查点",
        "ckpt:write" => "写入检查点",
        "store:read" => "读取通用记录仓",
        "store:write" => "写入通用记录仓",
        "feedback:read" => "查看反馈",
        "feedback:write" => "提交反馈",
        "p2p:read" => "查看 P2P 连接信息",
        "p2p:write" => "参与 P2P 打洞与信令",
        "index:read" => "本地检索与匹配索引",
        "index:write" => "接收平台推来的索引",
        "exec:read" => "查看远程执行任务",
        "exec:write" => "提交远程执行任务",
        "conn:read" => "查看连接通道",
        "conn:write" => "在连接通道上执行命令与传文件",
        "keys:write" => "签发与吊销 API-Key",
        _ => "",
    }
}

/// 校验作用域名是否合法。
pub fn valid_scope(sc: &str) -> bool {
    ALL_SCOPES.contains(&sc)
}

/// 作用域蕴含：写包含读。
pub fn scope_implies(have: &str, want: &str) -> bool {
    if have == want {
        return true;
    }
    match have {
        "registry:publish" => want == "registry:download" || want == "registry:read",
        "registry:download" => want == "registry:read",
        "nodes:write" => want == "nodes:read",
        "grants:write" => want == "grants:read",
        "config:write" => want == "config:read",
        "trace:write" | "trace:label" => want == "trace:read",
        "kb:write" => want == "kb:read",
        "mem:write" => want == "mem:read",
        "ckpt:write" => want == "ckpt:read",
        "store:write" => want == "store:read",
        "index:write" => want == "index:read",
        _ => false,
    }
}

/// 判断认证上下文是否满足某个作用域。
///
/// 会话（代表用户本人）直接放行；节点令牌与 API-Key 走作用域蕴含。
pub fn allow(a: Option<&AuthInfo>, scope: &str) -> bool {
    let Some(a) = a else { return false };
    if a.session {
        return true;
    }
    a.scopes.iter().any(|sc| scope_implies(sc, scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 作用域蕴含_写包含读() {
        assert!(scope_implies("registry:publish", "registry:read"));
        assert!(scope_implies("registry:publish", "registry:download"));
        assert!(scope_implies("nodes:write", "nodes:read"));
        assert!(!scope_implies("registry:read", "registry:publish"));
        // 反馈读写不互相蕴含：说得出口不等于能替别人处置
        assert!(!scope_implies("feedback:write", "feedback:read"));
    }

    #[test]
    fn 会话不受作用域限制() {
        let a = AuthInfo {
            session: true,
            ..Default::default()
        };
        assert!(allow(Some(&a), "keys:write"));
        assert!(!allow(None, "keys:write"));
    }
}
