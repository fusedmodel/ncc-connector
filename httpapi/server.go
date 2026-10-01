package httpapi

import (
	"embed"
	"io/fs"
	"net/http"
	"strings"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/fusedmodel/ncc-registry/config"
	"github.com/fusedmodel/ncc-registry/internal/p2p"
	"github.com/fusedmodel/ncc-registry/internal/secretbox"
	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/storage"
	"github.com/fusedmodel/ncc-registry/store"
)

//go:embed web
var consoleFS embed.FS

// NewRouter 组装全部路由。
//
// 路由分四块，对应产品的四条主线：
//
//	/api/auth       账户与凭据
//	/api/registry   制品托管（发布 / 检索 / 下载 / 分发）
//	/api/nodes      托管节点（Agent 发现与互联）
//	/api/cluster    多节点集群（master / worker）与能力路由
//
// 返回的 *Server 用于退出时收后台资源（见 Close）。只想拿一个 http.Handler
// 的场景用 NewRouter。
func NewServer(cfg *config.Config, st *store.Store, blob storage.Storage) (*Server, *gin.Engine) {
	s := &Server{Cfg: cfg, St: st, Blob: blob}
	s.hub = newClusterHub(cfg, st)
	s.hub.start()
	// P2P 可被打洞入口：默认关（它会在 UDP 上对外应答）；NCCR_P2P_SERVE=1 时随服务启动。
	if cfg.P2PServe {
		if r, err := p2p.StartResponder(cfg.P2PSTUN, 3*time.Second); err == nil {
			s.p2pResponder = r
			logf("p2p.serve 随服务开启 listen=%s mapped=%s", r.Addr(), r.MappedAddr())
		} else {
			logf("p2p.serve 启动失败（打洞入口不可用，其余功能不受影响）: %v", err)
		}
	}
	// 配置内容的静态加密（secret=true 的配置）。密钥由节点密钥派生；
	// 构造失败只影响「存敏感配置」，其余功能照常 —— 不因一个可选能力把服务启动卡死。
	if box, err := secretbox.New(cfg.JWTSecret); err == nil {
		s.Box = box
	} else {
		logf("配置加密不可用（secret 配置将不可写）: %v", err)
	}

	r := gin.New()
	r.Use(gin.Logger(), gin.Recovery())
	if cfg.CORSOrigins != "" {
		r.Use(corsMiddleware(cfg.CORSOrigins))
	}

	// 制品字节：公开可读（私有条目走 /api/registry/:ref/bytes 判断可见性）。
	r.Static("/blobs", cfg.BlobDir)

	r.GET("/api/health", func(c *gin.Context) {
		ok(c, 200, gin.H{"ok": true, "service": "ncc-registry", "role": cfg.Role, "nodeId": cfg.NodeID, "time": timeNow()})
	})

	// 节点自述：CLI / 控制台 / 其它节点都靠它认人。
	r.GET("/api/meta", s.meta)

	api := r.Group("/api")
	api.Use(s.authMiddleware())

	api.GET("/auth/meta", s.authMeta)
	api.POST("/auth/register", s.register)
	api.POST("/auth/login", s.login)
	api.GET("/auth/me", requireAuth(), s.me)
	api.PATCH("/auth/me", requireAuth(), s.patchMe)
	api.GET("/auth/keys", requireAuth(), s.listKeys)
	api.GET("/auth/key-scopes", requireAuth(), s.keyScopes)
	api.POST("/auth/keys", requireScope("keys:write"), s.createKey)
	api.DELETE("/auth/keys/:id", requireScope("keys:write"), s.deleteKey)

	api.GET("/namespaces/mine", requireAuth(), s.myNamespaces)
	api.POST("/namespaces", requireAuth(), s.createNamespace)
	api.POST("/namespaces/", requireAuth(), s.createNamespace)
	// 与平台路径一致：设备/节点上报也在这里，且可读。
	api.GET("/namespaces/living", requireAuth(), s.myNodes)
	api.POST("/namespaces/living", requireScope("nodes:write"), s.nodeHeartbeat)

	reg := api.Group("/registry")
	reg.GET("/kinds", s.kinds)
	reg.POST("/uploads", requireScope("registry:publish"), s.upload)
	reg.GET("", s.listRegistry)
	reg.GET("/", s.listRegistry)
	reg.POST("", requireScope("registry:publish"), s.createItem)
	reg.POST("/", requireScope("registry:publish"), s.createItem)
	// 引用有两种形态：A-…（单段）与 @ns/slug（两段）。gin 的 :param 不跨斜杠，
	// 所以两段形式必须单独注册一套（与平台侧同构）。
	reg.GET("/:id/download", s.download)
	reg.GET("/:id/bytes", s.bytes)
	reg.GET("/:id/:slug/download", s.download)
	reg.GET("/:id/:slug/bytes", s.bytes)
	reg.PATCH("/:id", requireScope("registry:publish"), s.patchItem)
	reg.PATCH("/:id/:slug", requireScope("registry:publish"), s.patchItem)
	// 加签：给已发布的 hur 制品附着/替换签名（签在本地做，这里只接收并无损落盘）。
	// 两种引用形态各注册一次，与上面的 PATCH/download 同构。
	reg.PUT("/:id/signature", requireScope("registry:publish"), s.attachSignature)
	reg.PUT("/:id/:slug/signature", requireScope("registry:publish"), s.attachSignature)
	reg.DELETE("/:id", requireScope("registry:publish"), s.deleteItem)
	reg.DELETE("/:id/:slug", requireScope("registry:publish"), s.deleteItem)
	reg.GET("/:id/:slug", s.getItem)
	reg.GET("/:id", s.getItem)

	nodes := api.Group("/nodes")
	nodes.GET("", s.listNodes)
	nodes.GET("/", s.listNodes)
	nodes.GET("/kinds", s.nodeKinds)
	// 节点「提供能力」词表（run:wasm / egress:llm / serve:mcp …）——检索用 ?can= 的时候取值来源
	nodes.GET("/offers", s.nodeOffers)
	nodes.GET("/discover", s.discoverNodes)
	nodes.GET("/regions", s.nodeRegions)
	nodes.GET("/route", s.clusterRoute)
	nodes.POST("/heartbeat", requireScope("nodes:write"), s.nodeHeartbeat)
	nodes.POST("/links", requireScope("nodes:write"), s.linkNode)
	nodes.PATCH("/links/:id", requireScope("nodes:write"), s.patchNodeLink)
	nodes.DELETE("/links/:id", requireScope("nodes:write"), s.deleteNodeLink)
	nodes.DELETE("/:id", requireScope("nodes:write"), s.deleteNode)

	// 分发授权：连接解决「找得到」，这里解决「拿得到」。
	api.GET("/grants", requireScope("grants:read"), s.listGrants)
	api.POST("/grants", requireScope("grants:write"), s.createGrant)
	api.DELETE("/grants/:id", requireScope("grants:write"), s.deleteGrant)

	// NCC Config：团队的网络 / 基础设施 / Agent 配置托管。
	// 读接口不挂作用域中间件（公开配置匿名可读），非公开配置在 handler 里判
	// config:read —— 因为「该不该给这个人看」与「这份配置是不是公开」必须一起判。
	cfgAPI := api.Group("/configs")
	cfgAPI.GET("/kinds", s.configKindCatalog)
	cfgAPI.GET("/bundle", s.configBundle)
	cfgAPI.GET("", s.listConfigs)
	cfgAPI.GET("/", s.listConfigs)
	cfgAPI.POST("", requireScope("config:write"), s.createConfig)
	cfgAPI.POST("/", requireScope("config:write"), s.createConfig)
	// 引用有两种形态：C-…（单段）与 @ns/slug（两段），与制品同一套写法。
	cfgAPI.GET("/:id/revisions", s.configRevisions)
	cfgAPI.GET("/:id/:slug/revisions", s.configRevisions)
	cfgAPI.POST("/:id/rollback", requireScope("config:write"), s.rollbackConfig)
	cfgAPI.POST("/:id/:slug/rollback", requireScope("config:write"), s.rollbackConfig)
	cfgAPI.PATCH("/:id", requireScope("config:write"), s.updateConfig)
	cfgAPI.PATCH("/:id/:slug", requireScope("config:write"), s.updateConfig)
	cfgAPI.DELETE("/:id", requireScope("config:write"), s.deleteConfig)
	cfgAPI.DELETE("/:id/:slug", requireScope("config:write"), s.deleteConfig)
	cfgAPI.GET("/:id/:slug", s.getConfig)
	cfgAPI.GET("/:id", s.getConfig)

	// 接入票据：把「一个内网 registry」加进 Agent —— key/secret 或接入短链。
	// NCC Trace：运行轨迹（能力评估 + 后训练数据集）。
	// 轨迹**没有「公开」档**：读/写/标注三件事分别要 trace:read / trace:write / trace:label。
	tr := api.Group("/traces")
	tr.GET("/kinds", s.traceKinds)
	tr.GET("/stats", requireScope("trace:read"), s.traceStats)
	tr.GET("/export", requireScope("trace:read"), s.exportTraces)
	tr.GET("", requireScope("trace:read"), s.listTraces)
	tr.GET("/", requireScope("trace:read"), s.listTraces)
	tr.POST("", requireScope("trace:write"), s.ingestTraces)
	tr.POST("/", requireScope("trace:write"), s.ingestTraces)
	tr.GET("/:id/labels", requireScope("trace:read"), s.listTraceLabels)
	tr.POST("/:id/labels", requireScope("trace:label"), s.addTraceLabel)
	tr.DELETE("/:id", requireScope("trace:write"), s.deleteTrace)
	tr.GET("/:id", requireScope("trace:read"), s.getTrace)

	// NCC State：知识库（kb）/ 记忆（mem）/ 检查点（ckpt）。
	//
	// 这三样**是状态，不是制品**：制品是被安装、被运行的代码 + 清单（有签名、可分发），
	// 状态是被改写、会长大的数据（默认私有、生命周期跟着 Agent 走）。
	// 包只能在 hur.json 里**声明**自己要哪些（规则 R11），实际字节住在这台节点的库里。
	//
	// 读接口与配置同一取舍：kb 有公开档（匿名可读公开文档），非公开的与 mem/ckpt
	// 全部在 handler 里判可见范围（默认私有 + state 授权）。
	kbAPI := api.Group("/kb")
	kbAPI.GET("/kinds", s.kbKinds)
	kbAPI.GET("/bundle", s.kbBundle)
	kbAPI.GET("", s.listKb)
	kbAPI.GET("/", s.listKb)
	kbAPI.POST("", requireScope("kb:write"), s.upsertKb)
	kbAPI.POST("/", requireScope("kb:write"), s.upsertKb)
	// 引用两种形态：K-…（单段）与 @ns/slug（两段），与制品/配置同一套写法。
	kbAPI.GET("/:id/revisions", s.kbRevisions)
	kbAPI.GET("/:id/:slug/revisions", s.kbRevisions)
	kbAPI.PATCH("/:id", requireScope("kb:write"), s.patchKb)
	kbAPI.PATCH("/:id/:slug", requireScope("kb:write"), s.patchKb)
	kbAPI.DELETE("/:id", requireScope("kb:write"), s.deleteKb)
	kbAPI.DELETE("/:id/:slug", requireScope("kb:write"), s.deleteKb)
	kbAPI.GET("/:id/:slug", s.getKb)
	kbAPI.GET("/:id", s.getKb)

	// 记忆：**没有公开档**（记忆是私人/团队状态，公开"记忆"这件事本身就不合语义）。
	memAPI := api.Group("/mem")
	memAPI.GET("/kinds", s.memKinds)
	memAPI.GET("/lookup", requireScope("mem:read"), s.lookupMem)
	memAPI.GET("", requireScope("mem:read"), s.listMem)
	memAPI.GET("/", requireScope("mem:read"), s.listMem)
	memAPI.PUT("", requireScope("mem:write"), s.putMem)
	memAPI.PUT("/", requireScope("mem:write"), s.putMem)
	memAPI.POST("/gc", requireScope("mem:write"), s.gcMem)
	memAPI.DELETE("/:id", requireScope("mem:write"), s.deleteMem)
	memAPI.GET("/:id", requireScope("mem:read"), s.getMem)

	// 检查点：不可变快照（元数据先建、字节后传），血缘可回溯。
	// 字节接口不挂作用域中间件：它要同时接受签名地址（服务端签发，对方不用带凭据）
	// 与可读凭据，两者在 handler 里判一次。
	ckAPI := api.Group("/ckpt")
	ckAPI.GET("/kinds", s.ckptKinds)
	ckAPI.GET("", requireScope("ckpt:read"), s.listCkpt)
	ckAPI.GET("/", requireScope("ckpt:read"), s.listCkpt)
	ckAPI.POST("", requireScope("ckpt:write"), s.createCkpt)
	ckAPI.POST("/", requireScope("ckpt:write"), s.createCkpt)
	ckAPI.POST("/prune", requireScope("ckpt:write"), s.pruneCkpt)
	ckAPI.PUT("/:id/blob", requireScope("ckpt:write"), s.putCkptBlob)
	ckAPI.GET("/:id/lineage", requireScope("ckpt:read"), s.ckptLineage)
	ckAPI.GET("/:id/bytes", s.ckptBytes)
	ckAPI.DELETE("/:id", requireScope("ckpt:write"), s.deleteCkpt)
	ckAPI.GET("/:id", requireScope("ckpt:read"), s.getCkpt)

	// NCC Store：通用记录仓 —— **集合是声明，记录是数据，服务端不认识业务字段**。
	//
	// 为什么单独一层：kb/mem/ckpt/trace 是四类内容，机械部分却是同一件事（归属命名空间、
	// 按 key 取、分页、标签、可见性、版本、上限、过期、软删）。各写一遍就是抄四遍。
	// 新增一类内容（issue / log / note）在这里**声明一个集合**即可，不改服务端。
	//
	// 路径约定：集合名走路径，命名空间走 `?namespace=`（集合属于命名空间，所以跨空间读必须显式指明）。
	// 读写作用域是 `store:read|write`（对**所有**集合生效）—— 集合级边界靠命名空间归属与认证，
	// 不靠"一集合一作用域"那种会爆炸的命名法。
	storeAPI := api.Group("/store")
	storeAPI.GET("/kinds", s.storeKinds)
	storeAPI.GET("", s.listStoreCollections)
	storeAPI.GET("/", s.listStoreCollections)
	storeAPI.POST("", requireScope("store:write"), s.declareStoreCollection)
	storeAPI.POST("/", requireScope("store:write"), s.declareStoreCollection)
	storeAPI.POST("/gc", requireScope("store:write"), s.gcStoreRecords)
	storeAPI.DELETE("/:col", requireScope("store:write"), s.archiveStoreCollection)
	storeAPI.GET("/:col", s.listStoreRecords)
	storeAPI.POST("/:col", requireScope("store:write"), s.upsertStoreRecord)
	storeAPI.GET("/:col/:key/history", s.storeRecordHistory)
	storeAPI.PUT("/:col/:key", requireScope("store:write"), s.putStoreRecord)
	storeAPI.DELETE("/:col/:key", requireScope("store:write"), s.deleteStoreRecord)
	storeAPI.GET("/:col/:key", s.getStoreRecord)

	// NCC Index：接收平台推来的索引副本 + 本地匹配。
	//
	// 写口只做「接收推送」（带 sourceId，幂等覆盖）—— 索引的权威在平台，
	// 节点侧开放自建就会立刻分叉出两个真相。读口公开（内网里检索本来就该便宜）。
	api.GET("/index", s.listIndex)
	api.GET("/index/", s.listIndex)
	api.GET("/index/channels", s.indexChannels)
	api.POST("/index", requireScope("index:write"), s.pushIndex)
	api.POST("/index/", requireScope("index:write"), s.pushIndex)
	api.GET("/match", s.matchIndex)
	acc := api.Group("/access")
	acc.POST("/redeem", s.redeem)
	acc.GET("/tickets", requireAuth(), s.listTickets)
	acc.POST("/tickets", requireScope("keys:write"), s.createTicket)
	acc.GET("/tickets/:key", s.ticketInfo)
	acc.DELETE("/tickets/:id", requireScope("keys:write"), s.deleteTicket)

	// 接入短链落地页（secret 在 URL fragment，服务端看不到）。
	r.GET("/j/:key", s.joinPage)

	// P2P：判断本节点打不打得到、可选开一个可被打洞的入口。
	// 与云端 ncc-platform 的 /api/p2p/* 互补：那边是信令/票据，这边是「本机 NAT 状况 + 真实对打」。
	api.GET("/p2p/self", requireScope("p2p:read"), s.p2pSelf)
	api.POST("/p2p/check", requireScope("p2p:write"), s.p2pCheck)
	api.GET("/p2p/serve", requireScope("p2p:read"), s.p2pServeGet)
	api.POST("/p2p/serve", requireScope("p2p:write"), s.p2pServeSet)

	// 制品分享链接：/api/shares 管自己的；/s/:token 是**公开**的领取入口（不用登录）。
	// 与接入短链刻意同形：/j/<key> 换一个节点身份，/s/<token> 换一次读取权。
	sh := api.Group("/shares")
	// 列表与撤销不挂 requireAuth：它们要同时接受「普通用户（自己的分享）」与
	// 「管理员凭据（全部）」，两套身份在 handler 里判一次就好，挂中间件反而会把
	// admin key 挡在 401（它本来就没有 Bearer 令牌）。
	sh.GET("", s.listShares)
	sh.GET("/", s.listShares)
	sh.POST("", requireAuth(), s.createShare)
	sh.POST("/", requireAuth(), s.createShare)
	sh.GET("/info/:token", s.shareInfo)
	sh.DELETE("/:id", s.deleteShare)
	r.GET("/s/:token", s.sharePage)
	r.GET("/s/:token/raw", s.shareRaw)

	// NCC Agent Share：把「我设计好的 Agent」点到点交给指定的人（包 + 可选节点）。
	// **与云端同形**：同一份 `ncc agent` 客户端把目标切到本节点就能用，客户端一行不用改。
	// 只有一处刻意不同：token 按本仓规矩只存 sha256（所以列表里回不出可点的链接）。
	// 读接口不挂作用域：token 本身就是秘密；写接口要登录（与会话同规矩）。
	cards := api.Group("/agent-cards")
	cards.GET("", s.listAgentCards)
	cards.GET("/", s.listAgentCards)
	cards.POST("", requireAuth(), s.createAgentCard)
	cards.POST("/", requireAuth(), s.createAgentCard)
	cards.GET("/:token", s.getAgentCard)
	cards.GET("/:token/blob", s.downloadAgentCardBlob)
	cards.POST("/:token/accept", requireAuth(), s.acceptAgentCard)
	cards.DELETE("/:token", s.deleteAgentCard)
	// 名片的落地页（人可读；设了口令先解锁）：与 /s/<token> 同一脾气 —— 拿到链接的人才看得到。
	r.GET("/a/:token", s.renderCardPage)
	r.POST("/a/:token", s.unlockCardPage)

	// 节点治理面：用户 / 节点 / 服务 的查看与处理（管理员或 admin key/secret）。
	// 单独一道门（requireAdmin），不挂在普通作用域体系上 —— 治理权与资产权是两回事。
	adm := api.Group("/admin")
	adm.Use(s.requireAdmin())
	adm.GET("/overview", s.adminOverview)
	adm.GET("/users", s.adminListUsers)
	adm.PATCH("/users/:id", s.adminPatchUser)
	adm.POST("/users/:id/password", s.adminResetPassword)
	adm.GET("/nodes", s.adminListNodes)
	adm.DELETE("/nodes/:id", s.adminDeleteNode)
	adm.GET("/services", s.adminListServices)
	// 服务引用两种形态：ND-…（单段）与 @命名空间/slug（两段）。
	adm.DELETE("/services/:ref", s.adminArchiveService)
	adm.DELETE("/services/:ref/:slug", s.adminArchiveService)
	adm.GET("/audit", s.adminListAudit)
	adm.GET("/keys", s.adminListKeys)
	adm.POST("/keys/rotate", s.adminRotateKey)

	cl := api.Group("/cluster")
	cl.POST("/join", s.clusterJoin)
	cl.POST("/heartbeat", s.clusterHeartbeat)
	cl.GET("", s.clusterView)
	cl.GET("/", s.clusterView)
	cl.GET("/workers", s.clusterWorkers)
	cl.GET("/directory", s.clusterDirectory)
	// 集群写：master → worker 分发副本 / 回收副本（节点间，集群 token 鉴权）。
	cl.POST("/ingest", s.clusterIngest)
	cl.POST("/revoke", s.clusterRevoke)
	cl.POST("/replicate", requireScope("registry:publish"), s.replicateArtifact)

	// 内置 Web 控制台（单文件，无构建步骤）。
	if cfg.Console {
		sub, err := fs.Sub(consoleFS, "web")
		if err == nil {
			serveIndex := func(c *gin.Context) {
				// 注意：不能用 c.FileFromFS("index.html", …) —— http.FileServer 见到
				// 以 /index.html 结尾的路径会 301 成 "./"，根路径就被重定向掉了。
				b, err := fs.ReadFile(consoleFS, "web/index.html")
				if err != nil {
					fail(c, 500, "internal", "控制台资源缺失")
					return
				}
				c.Data(http.StatusOK, "text/html; charset=utf-8", b)
			}
			r.GET("/console", func(c *gin.Context) { c.Redirect(http.StatusFound, "/") })
			r.NoRoute(func(c *gin.Context) {
				p := c.Request.URL.Path
				if strings.HasPrefix(p, "/api") || strings.HasPrefix(p, "/blobs") {
					fail(c, 404, "not_found", "未知 API 路径: "+p)
					return
				}
				if c.Request.Method != http.MethodGet && c.Request.Method != http.MethodHead {
					fail(c, 405, "method_not_allowed", "只支持 GET")
					return
				}
				// 静态文件按原路径服务，其余路径一律回控制台首页。
				if fp := strings.TrimPrefix(p, "/"); fp != "" {
					if f, err := sub.Open(fp); err == nil {
						_ = f.Close()
						c.FileFromFS(fp, http.FS(sub))
						return
					}
				}
				serveIndex(c)
			})
		}
	}

	return s, r
}

// NewRouter 返回组装好的路由。
//
// 它启动的后台循环（集群心跳 / 过期 worker 清理 / 可被打洞入口）没有句柄可停 ——
// 进程退出时无所谓；但作为库反复创建请改用 NewServer，并在退出时 Close。
func NewRouter(cfg *config.Config, st *store.Store, blob storage.Storage) *gin.Engine {
	_, r := NewServer(cfg, st, blob)
	return r
}

// Close 停掉后台循环与可被打洞入口。可重复调用，失败也不会有错可报，故不返回 error。
//
// 不关数据库：*store.Store 是调用方传进来的，谁开谁关。要落盘就自己关
// （GORM：st.DB.DB().Close()）。也不关 http.Server —— 那是 net/http 的事。
func (s *Server) Close() {
	if s.hub != nil {
		s.hub.Close()
	}
	s.p2pMu.Lock()
	defer s.p2pMu.Unlock()
	if s.p2pResponder != nil {
		s.p2pResponder.Close()
		s.p2pResponder = nil
	}
}

// meta GET /api/meta —— 本节点自述（CLI `ncc registry status` 的第一跳）。
func (s *Server) meta(c *gin.Context) {
	artifacts, nodes, users := s.Counts()
	configs, _ := s.St.CountConfigs()
	publicConfigs, _ := s.St.CountPublicConfigs()
	shares, _ := s.St.CountActiveShares()
	admins, _ := s.St.CountAdmins()
	nodeKinds, _ := s.St.NodeKindCounts()
	services, _ := s.St.CountServiceArtifacts("", "")
	traces, _ := s.St.CountTraces()
	// 三样状态：既是「这台节点托管了多少 Agent 状态」，也是容量规划的依据。
	kbDocs, memEntries, ckpts, _ := s.St.StateCounts()
	// 通用记录仓：集合数 + 记录数（新增一类内容不改服务端，但容量规划得看得见）
	collections, records, _ := s.St.StoreCounts()
	// 索引副本：平台推来的索引条数（内网本地匹配能查到的量）
	indexEntries, _ := s.St.CountIndex()
	hasAdminKey, _ := s.St.HasActiveAdminKey()
	out := gin.H{
		"product": "ncc-registry",
		"kind":    "node",
		"about":   "内网托管节点 · 制品托管 · 配置托管 · 分享 · Agent 发现与互联",
		"node":    s.selfNodeJSON(artifacts, nodes, users),
		// capabilities 是**声明**（命令面按它放行），features 是给人读的一句话。
		// 本地节点将来声明 services / profile 时，CLI 的同名命令会直接生效，不用改客户端。
		"capabilities": []string{
			"registry", "config", "share", "nodes", "grants", "access", "cluster", "admin", "p2p", "trace",
			"kb", "mem", "ckpt",
			// 通用记录仓：集合是声明、记录是数据。**一个能力对全部集合生效** ——
			// 能力回答"这台节点支不支持这类功能"，不是"有哪些集合"（那会爆炸）。
			"store",
			// 索引与匹配：接收平台推来的索引**副本**，在内网本地做匹配
			// （`ncc match --from <节点>`）。节点侧不持有评分，本地排序只有相关度。
			"index",
		},
		"counts": gin.H{
			"artifacts": artifacts, "hostedNodes": nodes, "users": users,
			"configs": configs, "publicConfigs": publicConfigs,
			// 治理面：管理员数、服务数（节点侧 kind=service + 制品侧 kind=api）、有效分享数。
			"admins": admins, "services": services,
			"serviceNodes": nodeKinds[model.NodeService], "shares": shares,
			// 轨迹：既是「这台节点收了多少行为数据」，也是评测数据集的体量。
			"traces": traces,
			// 状态：知识库文档数 / 记忆条目数 / 检查点数（fsck 与容量规划都看这三个）。
			"kbDocs": kbDocs, "memEntries": memEntries, "checkpoints": ckpts,
			// 通用记录仓：集合数与记录数（"又往仓里加了什么"要看得见）
			"collections": collections, "records": records,
			// 索引副本：平台推来多少条（内网本地检索能查到的量）
			"indexEntries": indexEntries,
		},
		// 存储目录：部署时最常被问的就是「字节到底落在哪」，直接报出来。
		"storage": gin.H{
			"driver":    "local",
			"dataDir":   s.Cfg.DataDir,
			"blobDir":   s.Cfg.BlobDir,
			"dbPath":    s.Cfg.DBPath,
			"blobsBase": s.Cfg.PublicURL + "/blobs/",
		},
		"features": []string{
			"registry: artifact hosting & distribution",
			"config: team/infra config hosting (versioned, encrypted secrets, grant-scoped)",
			"share: expiring artifact links (no login for the receiver)",
			"admin: node governance (users / nodes / services) with audit log",
			"nodes: hosted agent/service discovery & linking",
			"grants: explicit access grants (connect != authorize)",
			"access: join by key/secret or one-click intranet link",
			"cluster: master/worker multi-node, routing, replicate & revoke",
			"p2p: NAT profile + real hole-punch check between nodes (no business bytes relayed)",
			"trace: run traces of agents / HUR packages (capability evaluation + post-training datasets; private, opt-in, digest-first)",
			"kb: hosted knowledge bases (namespace-scoped corpora with revision history, pullable by agents)",
			"mem: hosted agent memory (key/value, TTL, source-traceable; no public tier by design)",
			"ckpt: hosted checkpoints (immutable bytes + lineage, signed short-lived download URLs)",
		},
		"console": s.Cfg.PublicURL + "/",
		"auth": gin.H{
			"inviteRequired": s.Cfg.InviteRequired(),
			"clusterToken":   s.Cfg.ClusterToken != "",
			"adminKey":       hasAdminKey,
		},
	}
	if s.Cfg.Role == config.RoleWorker {
		out["masterUrl"] = s.Cfg.MasterURL
	}
	ok(c, 200, out)
}

func corsMiddleware(origins string) gin.HandlerFunc {
	allowed := map[string]bool{}
	star := false
	for _, o := range strings.Split(origins, ",") {
		o = strings.TrimSpace(o)
		if o == "" {
			continue
		}
		if o == "*" {
			star = true
		}
		allowed[o] = true
	}
	return func(c *gin.Context) {
		origin := c.GetHeader("Origin")
		switch {
		case star:
			c.Header("Access-Control-Allow-Origin", "*")
		case origin != "" && allowed[origin]:
			c.Header("Access-Control-Allow-Origin", origin)
			c.Header("Vary", "Origin")
		}
		c.Header("Access-Control-Allow-Headers", "Authorization, Content-Type, X-Filename, X-NCC-Cluster-Token")
		c.Header("Access-Control-Allow-Methods", "GET, POST, PUT, PATCH, DELETE, OPTIONS")
		if c.Request.Method == http.MethodOptions {
			c.AbortWithStatus(http.StatusNoContent)
			return
		}
		c.Next()
	}
}
