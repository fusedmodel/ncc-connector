package httpapi

// NCC Agent Share（内网节点侧）—— `ncc agent share` / `ncc agent add` 在本节点上跑。
//
// 与云端 ncc-platform 的 `/api/agent-cards` **同形**：同一份 `ncc agent` 客户端
// 只要把目标切到本节点（`ncc target add office --base http://<内网 IP>:8282`）就能用，
// 客户端一行都不用改。刻意不同的只有一处：token 按本仓规矩**只存 sha256**。
//
// 红线（与 `ncc-platform/prd/ncc-agent-share.md` §6 逐条对齐）：
//  1. 名片 ≠ 分发渠道：不展示、不检索、没有全站列表；token 是秘密，可撤销 / 可过期 / 可限次。
//     要长期分发请走本节点既有的制品托管（`ncc publish` / `ncc registry …`），名片是点到点投递。
//  2. 名片 ≠ 授权：收下只代表**找得到**；私有制品 / 私有节点仍要 `grant`（本仓是 Grant 表）。
//  3. 收下 ≠ 执行：服务端只投递字节。
//  4. 字节即事实：sha256 与 hur.json 都由服务端从**收到的字节**算/读（不信客户端报的）。

import (
	"archive/zip"
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"html"
	"io"
	"strings"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

const (
	maxCardSize     = 8 << 20  // 名片里的包字节上限（8MB）
	maxCardUnzip    = 64 << 20 // 解压后上限：压缩炸弹在这里被挡住
	maxCardManifest = 64 << 10 // 只读 hur.json，且不给它撑爆内存的机会
	cardDefaultTTL  = 7 * 24 * time.Hour
	cardMaxTTL      = 365 * 24 * time.Hour
	cardMaxUses     = 10000
	cardBlobPrefix  = "agent-cards/" // 对象名前缀（s.Blob 里）
)

func cardSHA256(b []byte) string {
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}

func clampRunesCard(s string, max int) string {
	r := []rune(strings.TrimSpace(s))
	if len(r) > max {
		return string(r[:max])
	}
	return string(r)
}

// hurManifestBytes 从 `.hur` 字节里读出 `hur.json` 原文。
//
// 容器是 gzip 包住的 zip（老包是裸 zip，两种都要认）。**不落盘解包**：只开这一个条目、
// 解压后大小设上限 —— 这是别人上传的字节，这里不该有「解压到磁盘」这种动作。
func hurManifestBytes(raw []byte) ([]byte, error) {
	zipBytes := raw
	if len(raw) >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
		zr, err := gzip.NewReader(bytes.NewReader(raw))
		if err != nil {
			return nil, errWith(400, "bad_request", "gzip 层打不开（不是有效的 .hur）")
		}
		defer zr.Close()
		b, err := io.ReadAll(io.LimitReader(zr, maxCardUnzip))
		if err != nil {
			return nil, errWith(400, "bad_request", "gzip 层解压失败")
		}
		zipBytes = b
	}
	zr, err := zip.NewReader(bytes.NewReader(zipBytes), int64(len(zipBytes)))
	if err != nil {
		return nil, errWith(400, "bad_request", "既不是 gzip 也不是 zip —— 这不是一个 hur 包")
	}
	for _, f := range zr.File {
		if f.Name != "hur.json" {
			continue
		}
		if f.UncompressedSize64 > maxCardManifest {
			return nil, errWith(400, "bad_request", "包里的 hur.json 过大（>64KB）")
		}
		rc, err := f.Open()
		if err != nil {
			return nil, errWith(400, "bad_request", "读 hur.json 失败")
		}
		defer rc.Close()
		return io.ReadAll(io.LimitReader(rc, maxCardManifest+1))
	}
	return nil, errWith(400, "bad_request", "包里没有 hur.json")
}

type cardManifest struct {
	Spec    string `json:"spec"`
	Kind    string `json:"kind"`
	Profile string `json:"profile"`
	ID      string `json:"id"`
	Name    string `json:"name"`
	Version string `json:"version"`
}

/* ---------------- 状态与视图 ---------------- */

// cardState 名片状态：`""` 可用，否则是**为什么不能收**（三种分开说：
// 对接受方来说都是「用不了」，但原因决定了他下一步做什么）。
func cardState(c *model.AgentCard) string {
	now := time.Now()
	if c.Revoked() {
		return "revoked"
	}
	if c.Expired(now) {
		return "expired"
	}
	if c.Exhausted() {
		return "exhausted"
	}
	return ""
}

func cardStateMsg(reason string) string {
	switch reason {
	case "revoked":
		return "这张名片已被作者撤销"
	case "expired":
		return "这张名片已过有效期，请让作者重新生成一张"
	case "exhausted":
		return "这张名片的收下次数已用完，请让作者重新生成一张"
	}
	return "这张名片不可用"
}

// cardReadable 读（读名片 / 取字节）要不要拦。
//
// ⚠️ **名额用完不算拦**：名额是在 accept 那一刻扣的，扣完就不让取字节，
// 等于把已经拿到名额的人关在门外（平台侧实测踩过这个坑）。用完只对 accept 生效。
func cardReadable(c *model.AgentCard) bool {
	return !c.Revoked() && !c.Expired(time.Now())
}

func (s *Server) cardLink(token string) string {
	return strings.TrimRight(s.Cfg.PublicURL, "/") + "/a/" + token
}

// cardJSON 名片视图：**字段名与云端逐字对齐**（同一份客户端两处都能解析）。
//
// `token` 只能来自**调用方这次请求的那条路径** —— 库里只存 token 的 sha256，
// 谁也拿不回明文（这是本仓的规矩，比云端更保守）。所以：
//   · 单条接口（取名片 / 页面）能回出真实可用的 url 与 blob；
//   · 列表接口回不出来（作者手上也没明文），只给 hint —— 客户端会把链接那行省掉。
func (s *Server) cardJSON(c *model.AgentCard, token string) gin.H {
	var manifest any
	if strings.TrimSpace(c.Manifest) != "" {
		if err := json.Unmarshal([]byte(c.Manifest), &manifest); err != nil {
			manifest = nil
		}
	}
	node := gin.H(nil)
	if c.NodeRef != "" || c.NodeID != "" {
		node = gin.H{"ref": c.NodeRef, "id": c.NodeID, "kind": c.NodeKind, "label": c.NodeLabel}
	}
	expires := any(nil)
	if c.ExpiresAt != nil {
		expires = c.ExpiresAt.UTC().Format(time.RFC3339)
	}
	blob, url := "", ""
	if token != "" {
		blob = "/api/agent-cards/" + token + "/blob"
		url = s.cardLink(token)
	}
	return gin.H{
		"id": c.ID, "name": c.Name, "note": c.Note,
		"author": s.cardAuthor(c.OwnerID),
		"agent": gin.H{
			"id": c.AgentID, "version": c.AgentVersion,
			"kind": c.AgentKind, "profile": c.AgentProfile,
			"sha256": c.SHA256, "bytes": c.Size,
			"blob": blob,
		},
		"manifest":  manifest,
		"node":      node,
		"expiresAt": expires, "uses": c.Uses, "maxUses": c.MaxUses,
		"hasPassword": c.PassHash != "", "revoked": c.Revoked(),
		"state": cardState(c), "hint": c.TokenHint,
		"url": url,
	}
}

// cardAuthor 作者公开信息。本节点只有 users 表（用户名 + 邮箱 + 所在地），
// 与平台侧的字段名保持一致（handle 取不到就给空串，客户端会跳过）。
func (s *Server) cardAuthor(ownerID string) gin.H {
	out := gin.H{"userId": ownerID, "handle": "", "displayName": ""}
	if u, err := s.St.FindUserByID(ownerID); err == nil {
		out["displayName"] = u.Name
	}
	return out
}

/* ---------------- 创建 ---------------- */

// createAgentCard POST /api/agent-cards —— body = `.hur` 字节（或 multipart `file`）。
//
// 与平台侧同一套参数（查询串或表单）：name / note / node / key / uses / expiresAt / expires / profile。
func (s *Server) createAgentCard(c *gin.Context) {
	a := authOf(c)

	var raw []byte
	name, note, key, nodeRef, expiresRaw, profile := "", "", "", "", "", ""

	if strings.HasPrefix(c.GetHeader("Content-Type"), "multipart/form-data") {
		fh, err := c.FormFile("file")
		if err != nil {
			fail(c, 400, "bad_request", "缺少 file 字段")
			return
		}
		if fh.Size > maxCardSize {
			fail(c, 413, "payload_too_large", "包超过 8MB")
			return
		}
		f, err := fh.Open()
		if err != nil {
			fail(c, 500, "internal", "读取上传失败")
			return
		}
		defer f.Close()
		if raw, err = io.ReadAll(io.LimitReader(f, maxCardSize+1)); err != nil {
			fail(c, 500, "internal", "读取上传失败")
			return
		}
		name, note = c.PostForm("name"), c.PostForm("note")
		key, nodeRef, profile = c.PostForm("key"), c.PostForm("node"), c.PostForm("profile")
		expiresRaw = firstNonEmpty(c.PostForm("expires"), c.PostForm("expiresAt"))
	} else {
		b, err := io.ReadAll(io.LimitReader(c.Request.Body, maxCardSize+1))
		if err != nil {
			fail(c, 500, "internal", "读取上传失败")
			return
		}
		raw = b
		name, note = c.Query("name"), c.Query("note")
		key, nodeRef, profile = c.Query("key"), c.Query("node"), c.Query("profile")
		expiresRaw = firstNonEmpty(c.Query("expires"), c.Query("expiresAt"))
	}
	uses := parseInt64Default(firstNonEmpty(c.Query("uses"), c.PostForm("uses")))

	if len(raw) == 0 {
		fail(c, 400, "bad_request", "上传内容为空")
		return
	}
	if len(raw) > maxCardSize {
		fail(c, 413, "payload_too_large", "包超过 8MB")
		return
	}

	// 清单从**字节**里读：读不出来就不是包，直接拒。
	mb, err := hurManifestBytes(raw)
	if err != nil {
		fail(c, errStatus(err), errCode(err), "这不是一个 hur 包："+err.Error())
		return
	}
	var mf cardManifest
	if err := json.Unmarshal(mb, &mf); err != nil {
		fail(c, 400, "bad_request", "包里的 hur.json 不是合法 JSON")
		return
	}
	if mf.Spec == "" || mf.ID == "" || mf.Version == "" {
		fail(c, 400, "bad_request", "hur.json 缺 spec / id / version —— 不是一个完整的包")
		return
	}
	// profile：老包清单里可能没写（「没写就按 kind 推导」这条规则住在 hur-core），
	// 那就用作者侧 CLI 解析出来的那个填上。只用于展示 —— 权威的 kind/id/version 来自字节。
	if mf.Profile == "" {
		mf.Profile = strings.TrimSpace(profile)
	}
	if name == "" {
		name = mf.Name
	}
	if name == "" {
		name = mf.ID
	}
	if uses < 0 || uses > cardMaxUses {
		fail(c, 400, "bad_request", "收下次数需在 0-10000 之间（0 = 不限）")
		return
	}

	passHash, passSalt := "", ""
	if key != "" {
		if len(key) < 3 || len(key) > 16 {
			fail(c, 400, "bad_request", "访问 key 长度需 3-16 位")
			return
		}
		passSalt = store.RandHex(8)
		passHash = store.HashCardPass(passSalt, key)
	}

	// 有效期：`expires`（Go duration，客户端把 7d 换成 168h）或 `expiresAt`（RFC3339）；
	// `none` = 不过期。缺省 7 天 —— 名片是「给指定的人」的东西，默认不该永不过期。
	var expiresAt *time.Time
	if v := strings.TrimSpace(expiresRaw); v != "" {
		if !strings.EqualFold(v, "none") {
			t, err := time.Parse(time.RFC3339, v)
			if err != nil {
				d, derr := time.ParseDuration(v)
				if derr != nil || d <= 0 {
					fail(c, 400, "bad_request", "expires 用 Go duration（168h）或 none；绝对时间用 expiresAt（RFC3339）")
					return
				}
				t = time.Now().Add(d)
			}
			if t.After(time.Now().Add(cardMaxTTL)) {
				fail(c, 400, "bad_request", "有效期最长 365 天")
				return
			}
			expiresAt = &t
		}
	} else {
		def := time.Now().Add(cardDefaultTTL)
		expiresAt = &def
	}

	// 节点那一侧：现在解析一次，别把「对方点开才发现连不上」留给接受方。
	nodeID, nodeKind, nodeLabel := "", "", ""
	if r := strings.TrimSpace(nodeRef); r != "" && r != "none" {
		row, err := s.resolveNode(a.UserID, r)
		if err != nil {
			fail(c, errStatus(err), errCode(err), "找不到这个节点（用 ND-… 或 @命名空间/节点slug）")
			return
		}
		if !s.canSeeNode(row, a.UserID) {
			// 不是「别人的私有节点」那种边界（作者就是自己），而是：他要转手一台
			// 别人连不到的节点 —— 接受方永远连不上，当场说清比事后猜好。
			fail(c, 403, "grant_required", "这个节点不是公开节点，别人连不上：让它的主人授权或设为公开，或 --node none 只分享包")
			return
		}
		nodeID, nodeKind, nodeLabel = row.ID, row.Kind, row.Name
	}

	cardID := store.NewID("AC")
	blobName := cardBlobPrefix + cardID + ".hur"
	if _, err := s.Blob.Put(blobName, raw); err != nil {
		fail(c, 500, "internal", "写入名片字节失败")
		return
	}
	card, token, err := s.St.CreateAgentCard(store.AgentCardInput{
		ID: cardID, OwnerID: a.UserID,
		Name: clampRunesCard(name, 60), Note: clampRunesCard(note, 400),
		BlobName: blobName, Size: int64(len(raw)), SHA256: cardSHA256(raw),
		Manifest: string(mb),
		AgentID:  mf.ID, AgentVersion: mf.Version, AgentKind: mf.Kind, AgentProfile: mf.Profile,
		NodeRef: strings.TrimSpace(nodeRef), NodeID: nodeID, NodeKind: nodeKind, NodeLabel: nodeLabel,
		PassHash: passHash, PassSalt: passSalt, MaxUses: uses, ExpiresAt: expiresAt,
	})
	if err != nil {
		_ = s.Blob.Delete(blobName)
		fail(c, 500, "internal", "创建名片失败")
		return
	}
	link := s.cardLink(token)
	s.ensureAdmin(c)
	s.audit(c, model.ActShareCreate, card.ID, card.Name, "创建 Agent 名片",
		gin.H{"agent": card.AgentID, "node": card.NodeRef, "uses": uses, "expiresAt": expiresAt})
	ok(c, 201, gin.H{
		"card": s.cardJSON(card, token),
		// token 与 key 都只在创建这一刻回显（库里只存哈希，之后谁也取不回来）。
		"url": link,
		"key": key,
		"howto": gin.H{
			"human": "把 url 发给对方：浏览器打开是名片页，上面写着怎么收下",
			"agent": "对方一条命令收下：" + "ncc agent add '" + link + "'",
			"note":  "收下只代表**找得到**：私有制品 / 私有节点仍要显式 grant",
		},
	})
}

/* ---------------- 读 ---------------- */

// findCard 取名片：token（32 hex，现算哈希）或 id（AC-…）都认。
func (s *Server) findCard(ref string) (*model.AgentCard, error) {
	ref = strings.TrimSpace(ref)
	if strings.HasPrefix(ref, "AC-") {
		return s.St.FindAgentCardByID(ref)
	}
	return s.St.FindAgentCardByToken(ref)
}

// cardKeyOK 访问口令校验。作者本人不需要口令。
func (s *Server) cardKeyOK(c *gin.Context, card *model.AgentCard) bool {
	if card.PassHash == "" {
		return true
	}
	if a := authOf(c); a != nil && a.UserID == card.OwnerID {
		return true
	}
	key := firstNonEmpty(c.Query("key"), c.PostForm("key"))
	if key != "" {
		return store.HashCardPass(card.PassSalt, key) == card.PassHash
	}
	ck, err := c.Cookie(cardCookieName(card.ID))
	return err == nil && ck == card.PassHash
}

// getAgentCard GET /api/agent-cards/:token —— token 即秘密（匿名可读）。
func (s *Server) getAgentCard(c *gin.Context) {
	card, err := s.findCard(c.Param("token"))
	if err != nil {
		fail(c, 404, "not_found", "名片不存在")
		return
	}
	if !s.cardKeyOK(c, card) {
		fail(c, 403, "forbidden", "需要访问 key")
		return
	}
	if !cardReadable(card) {
		st := cardState(card)
		fail(c, 410, st, cardStateMsg(st))
		return
	}
	_ = s.St.BumpAgentCardViews(card.ID)
	ok(c, 200, gin.H{"card": s.cardJSON(card, c.Param("token"))})
}

// listAgentCards GET /api/agent-cards —— **只有自己的**（含已撤销 / 过期 / 用尽）。
func (s *Server) listAgentCards(c *gin.Context) {
	a := authOf(c)
	if a == nil {
		fail(c, 401, "unauthorized", "未认证或凭据无效（先 ncc login 或带 API-KEY）")
		return
	}
	limit, offset := pageParams(c)
	rows, err := s.St.ListAgentCards(a.UserID, limit, offset)
	if err != nil {
		fail(c, 500, "internal", "服务内部错误")
		return
	}
	list := make([]gin.H, 0, len(rows))
	for i := range rows {
		// token 明文在库里不存在（只存哈希）→ 列表里没有可点的链接，只给 hint。
		list = append(list, s.cardJSON(&rows[i], ""))
	}
	ok(c, 200, gin.H{"cards": list, "total": len(list), "limit": limit, "offset": offset,
		"note": "本节点只存 token 的 sha256：链接只在作者当初发出去的那一份里，列表回不出来"})
}

// acceptAgentCard POST /api/agent-cards/:token/accept —— 收下一次（扣名额）。
//
// 名额是「投递次数」不是「安装次数」：客户端把它放在**下载之前**，
// 扣不动就不再下载字节 —— 否则限次形同虚设。
func (s *Server) acceptAgentCard(c *gin.Context) {
	card, err := s.findCard(c.Param("token"))
	if err != nil {
		fail(c, 404, "not_found", "名片不存在")
		return
	}
	if !s.cardKeyOK(c, card) {
		fail(c, 403, "forbidden", "需要访问 key")
		return
	}
	if st := cardState(card); st != "" {
		fail(c, 410, st, cardStateMsg(st))
		return
	}
	uses, err := s.St.ConsumeAgentCard(card.ID)
	if err != nil {
		// 并发下刚好被抢光：还是 exhausted，不是 500。
		fail(c, 410, "exhausted", cardStateMsg("exhausted"))
		return
	}
	remaining := int64(-1)
	if card.MaxUses > 0 {
		remaining = card.MaxUses - uses
	}
	ok(c, 200, gin.H{"uses": uses, "maxUses": card.MaxUses, "remaining": remaining})
}

// downloadAgentCardBlob GET /api/agent-cards/:token/blob —— `.hur` 字节。
func (s *Server) downloadAgentCardBlob(c *gin.Context) {
	card, err := s.findCard(c.Param("token"))
	if err != nil {
		fail(c, 404, "not_found", "名片不存在")
		return
	}
	if !s.cardKeyOK(c, card) {
		fail(c, 403, "forbidden", "需要访问 key")
		return
	}
	if !cardReadable(card) {
		st := cardState(card)
		fail(c, 410, st, cardStateMsg(st))
		return
	}
	rc, _, err := s.Blob.Open(card.BlobName)
	if err != nil {
		fail(c, 404, "not_found", "名片里的包已不在本节点（可能已被清理）")
		return
	}
	defer rc.Close()
	body, err := io.ReadAll(io.LimitReader(rc, maxCardSize+1))
	if err != nil {
		fail(c, 500, "internal", "读取字节失败")
		return
	}
	// 指纹随字节一起给：接受方拿它比对（不一致就**不许**装）。
	c.Header("X-NCC-Sha256", card.SHA256)
	c.Header("X-NCC-Agent", card.AgentID+"@"+card.AgentVersion)
	c.Header("X-Content-Type-Options", "nosniff")
	c.Header("X-Robots-Tag", "noindex, nofollow")
	c.Header("Content-Disposition",
		"attachment; filename=\""+card.AgentID+"-"+card.AgentVersion+".hur\"")
	c.Data(200, "application/octet-stream", body)
}

// deleteAgentCard DELETE /api/agent-cards/:token —— 撤销。
//
// 撤销 = **标记 revoked + 删字节**：字节真的没了（不是「标记一下但还能下」），
// 记录留着 —— 作者在 `ncc agent ls` 里要看得到自己发过什么、哪张已作废，
// 对方点开也能得到一句「作者撤销了」而不是含糊的 404。
func (s *Server) deleteAgentCard(c *gin.Context) {
	a := authOf(c)
	card, err := s.findCard(c.Param("token"))
	if err != nil {
		fail(c, 404, "not_found", "名片不存在")
		return
	}
	if a == nil {
		fail(c, 401, "unauthorized", "未认证或凭据无效（先 ncc login 或带 API-KEY）")
		return
	}
	if card.OwnerID != a.UserID {
		fail(c, 403, "forbidden", "只能撤销自己的名片")
		return
	}
	if err := s.St.RevokeAgentCard(card.ID, a.UserID); err != nil {
		fail(c, 500, "internal", "撤销失败")
		return
	}
	_ = s.Blob.Delete(card.BlobName)
	s.audit(c, model.ActShareRevoke, card.ID, card.Name, "撤销 Agent 名片",
		gin.H{"agent": card.AgentID})
	ok(c, 200, gin.H{"ok": true, "id": card.ID, "revoked": true})
}

func parseInt64Default(s string) int64 {
	n := int64(0)
	for _, ch := range strings.TrimSpace(s) {
		if ch < '0' || ch > '9' {
			return 0
		}
		n = n*10 + int64(ch-'0')
		if n > cardMaxUses {
			return cardMaxUses + 1
		}
	}
	return n
}

/* ---------------- 人也能看：/a/:token ---------------- */

func cardCookieName(id string) string { return "ncc_card_" + id }

// renderCardPage GET /a/:token —— 这一条链接的落地页（noindex）。
//
// 拿到链接的人（也许是没装 ncc 的同事）至少该看见：这是什么、谁给的、怎么收下。
// 页面**不列任何人的其他名片** —— 它不是目录，只是一张名片的脸。
func (s *Server) renderCardPage(c *gin.Context) {
	card, err := s.findCard(c.Param("token"))
	if err != nil {
		c.String(404, "名片不存在")
		return
	}
	if !s.cardKeyOK(c, card) {
		s.renderCardUnlock(c, card, "")
		return
	}
	if !cardReadable(card) {
		st := cardState(card)
		c.Header("Content-Type", "text/html; charset=utf-8")
		c.Header("X-Robots-Tag", "noindex, nofollow")
		c.String(410, cardPageShell("名片不可用", html.EscapeString(cardStateMsg(st))))
		return
	}
	_ = s.St.BumpAgentCardViews(card.ID)

	author := s.cardAuthor(card.OwnerID)
	who := html.EscapeString(author["displayName"].(string))
	nodeLine := "（这张名片只有包，不附带节点）"
	if card.NodeRef != "" {
		nodeLine = "接受方会把该节点收进自己的连接表：<code>" + html.EscapeString(card.NodeRef) + "</code>"
	}
	expire, uses := "长期有效", "不限次"
	if card.ExpiresAt != nil {
		expire = card.ExpiresAt.Local().Format("2006-01-02 15:04")
	}
	if card.MaxUses > 0 {
		uses = "限 " + itoa(card.MaxUses) + " 人（已收 " + itoa(card.Uses) + "）"
	}
	warn := ""
	if cardState(card) == "exhausted" {
		warn = `<p class="warn">这张名片的收下名额已用完 —— 已经收下过的人不受影响。</p>`
	}
	body := warn + `
<p class="by">` + who + ` 通过本内网节点分享了一个 Agent 给你</p>
<h1>` + html.EscapeString(card.Name) + `</h1>
` + noteHTMLCard(card.Note) + `
<table>
<tr><td>包</td><td><code>` + html.EscapeString(card.AgentID) + `@` + html.EscapeString(card.AgentVersion) +
		`</code> · ` + html.EscapeString(card.AgentProfile) + `</td></tr>
<tr><td>指纹</td><td><code class="sha">` + html.EscapeString(card.SHA256) + `</code></td></tr>
<tr><td>节点</td><td>` + nodeLine + `</td></tr>
<tr><td>有效期</td><td>` + expire + ` · ` + uses + `</td></tr>
</table>
<p class="how">收下它（装到本机 + 连上节点）：</p>
<pre>` + html.EscapeString("ncc agent add '"+s.cardLink(c.Param("token"))+"'") + `</pre>
<p class="sub">这一步需要 ncc 客户端，并且要指向本节点（<code>` + html.EscapeString(s.Cfg.PublicURL) + `</code>）。</p>
<p class="sub">收下只是「找得到」；要取对方的私有制品仍需单独授权（grant）。</p>`
	s.sendCardHTML(c, 200, "NCC Agent 名片", body)
}

// unlockCardPage POST /a/:token —— 提交访问 key 后写 7 天 Cookie 并跳回页面。
func (s *Server) unlockCardPage(c *gin.Context) {
	card, err := s.findCard(c.Param("token"))
	if err != nil {
		c.String(404, "名片不存在")
		return
	}
	if card.PassHash != "" && store.HashCardPass(card.PassSalt, c.PostForm("key")) == card.PassHash {
		c.SetCookie(cardCookieName(card.ID), card.PassHash, 7*24*3600, "/a/"+c.Param("token"), "", false, true)
		c.Redirect(303, "/a/"+c.Param("token"))
		return
	}
	s.renderCardUnlock(c, card, "key 不正确，请重试")
}

func (s *Server) renderCardUnlock(c *gin.Context, card *model.AgentCard, errMsg string) {
	errHTML := ""
	if errMsg != "" {
		errHTML = `<p class="err">` + html.EscapeString(errMsg) + `</p>`
	}
	body := `<p class="by">这是一张点到点的 Agent 名片</p>
<h1>需要访问 key</h1>
<p class="sub">「` + html.EscapeString(card.Name) + `」需要作者给你的访问 key</p>
<form method="post" action="/a/` + html.EscapeString(c.Param("token")) + `">
<input name="key" type="password" placeholder="输入访问 key" maxlength="16" autocomplete="off" autofocus>
<button type="submit">查看</button>
</form>` + errHTML
	s.sendCardHTML(c, 200, "需要访问 key", body)
}

func (s *Server) sendCardHTML(c *gin.Context, status int, title, body string) {
	c.Header("Content-Type", "text/html; charset=utf-8")
	c.Header("X-Content-Type-Options", "nosniff")
	c.Header("Referrer-Policy", "no-referrer")
	// 点到点链接不进搜索引擎：这不是节点内容，只该被「拿到链接的人」看到。
	c.Header("X-Robots-Tag", "noindex, nofollow")
	c.Status(status)
	_, _ = c.Writer.WriteString(cardPageShell(title, body))
}

func noteHTMLCard(note string) string {
	if strings.TrimSpace(note) == "" {
		return ""
	}
	return `<p class="note">` + html.EscapeString(note) + `</p>`
}

// cardPageShell 极简卡片页外壳（自带样式，不引任何外部资源）。
func cardPageShell(title, body string) string {
	return `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>` + html.EscapeString(title) + `</title>
<style>
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;padding:24px;
background:linear-gradient(135deg,#0f172a,#1e293b 60%,#155e75);font-family:system-ui,-apple-system,"PingFang SC",sans-serif;color:#e2e8f0}
.card{background:rgba(255,255,255,.06);border:1px solid rgba(255,255,255,.16);border-radius:16px;
padding:28px 26px;width:min(94vw,560px)}
h1{font-size:22px;margin:6px 0 14px}.by{color:#67e8f9;font-size:13.5px;margin:0}
.sub,.note{color:#94a3b8;font-size:13.5px}
table{width:100%;border-collapse:collapse;margin:14px 0}
td{padding:6px 0;font-size:13.5px;vertical-align:top;border-top:1px solid rgba(255,255,255,.09)}
td:first-child{color:#94a3b8;width:64px}
code{background:rgba(255,255,255,.08);padding:1px 5px;border-radius:5px;font-size:12.5px}
code.sha{word-break:break-all}
pre{background:rgba(0,0,0,.35);padding:12px;border-radius:10px;overflow-x:auto;font-size:13px}
input{width:100%;padding:11px 14px;border-radius:10px;border:1px solid rgba(255,255,255,.25);
background:rgba(255,255,255,.08);color:#fff;font-size:16px;letter-spacing:.2em;text-align:center}
input:focus{outline:none;border-color:#22d3ee}
button{margin-top:14px;width:100%;padding:11px;border:none;border-radius:10px;background:#0891b2;color:#fff;font-size:15px;font-weight:600;cursor:pointer}
button:hover{background:#0e7490}
.err{color:#fca5a5;font-size:13px;margin:12px 0 0}
.warn{color:#fcd34d;font-size:13.5px;margin:0 0 12px}
.how{margin:18px 0 6px;font-size:13.5px}
</style></head><body><div class="card">` + body + `</div></body></html>`
}
