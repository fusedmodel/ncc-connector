//! ncc-core：NCC 平台服务（`ncc-platform`）与内网托管节点（`ncc-registry`）
//! 共用的基础能力。
//!
//! 这一层刻意不做业务：只放两侧都会用到的「环境配置 / 密码学 / ID / 错误 /
//! 作用域 / 加密盒 / 字节存储 / 数据库连接 / Web 小工具」。
//! 两个服务各自保留自己的 model 与 store —— 它们的领域模型并不相同（平台有
//! 计费/网关/社交，节点有集群/执行/轨迹），强行合并只会造出一个谁都不像的中间层。

//! ## 关于这份 `ncc-core`（两侧各存一份，改动要同时落两处）
//!
//! 平台服务与内网节点服务各自需要这一层（环境配置 / 密码学 / ID / 错误 / 作用域 /
//! 加密盒 / 字节存储 / 数据库连接），但**两个仓库必须各自自包含**：
//! `ncc-registry` 的边界约定是「不 import 平台私有代码」，反向同理。
//! 所以它是**有意复制**的一份，而不是跨仓依赖 —— 依赖会让两个仓库互相锁死版本，
//! 而它们的发布节奏本来就不同。
//!
//! 两边内容逐字相同，另一份在：
//! * 节点侧 `ncc-registry/rust/crates/ncc-core`
//! * 平台侧 `ncc-platform/server/crates/ncc-core`
//!
//! 改这里的任何东西，记得把另一侧同步过来（本层只放两个服务都会用到的东西，
//! 别把某个服务独有的逻辑沉下来 —— 那正是「改一处漏一处」的开始）。

pub mod crypto;
pub mod env;
pub mod error;
pub mod ids;
pub mod jwt;
pub mod pool;
pub mod scope;
pub mod secretbox;
pub mod storage;
pub mod timeutil;
pub mod web;

/// 平台服务版本（原 Go: `httpapi.Version`）。
pub const PLATFORM_VERSION: &str = "ncc-server/0.1.0";

/// 内网托管节点版本（原 Go: `httpapi.Version`）。
pub const REGISTRY_VERSION: &str = "ncc-registry/0.2.0";
