// NCC Index（节点侧）：接收平台推来的索引，并在**内网本地**做匹配。
//
// 为什么节点也要有这一套：内网里的 Agent 不该为了「找个能办事的人」出网 ——
// 平台是权威（跨网可查），节点是副本（本地可查）。两条边界：
//
//  1. **只收平台推来的**：不提供「在节点上凭空造一条索引」的接口。索引的权威在
//     平台，节点侧开放写入就会立刻分叉出两个真相（两边内容不一样，谁也不知道以
//     谁为准）。因此 `POST /api/index` 的语义是**接收推送**（带 sourceId），
//     幂等：同一条重推是覆盖。
//  2. **本地匹配不算信誉权重**：评分（star）只存在平台，是平台的内部权重。副本不
//     假装自己知道全网信誉 —— 本地排序只有相关度，且**不显示任何评分**。
package httpapi

import (
	"sort"
	"strings"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

const (
	maxIndexTitle = 120
	maxIndexList  = 100
)

// indexPushReq 平台推来的一条索引。
//
// 字段名与平台 `GET /api/index` 的条目一致（`ncc index publish` 直接把平台返回的
// 那条送过来），所以这里不需要一套转换表。
type indexPushReq struct {
	ID          string   `json:"id"` // 平台那条索引的 id（IX-…）：幂等键
	OwnerKey    string   `json:"ownerKey"`
	Owner       string   `json:"owner"`
	DisplayName string   `json:"displayName"`
	Kind        string   `json:"kind"`
	Channel     string   `json:"channel"`
	Slug        string   `json:"slug"`
	Ref         string   `json:"ref"`
	Title       string   `json:"title"`
	Summary     string   `json:"summary"`
	Description string   `json:"description"`
	Category    string   `json:"category"`
	Region      string   `json:"region"`
	Tags        []string `json:"tags"`
	Intents     []string `json:"intents"`
	Languages   []string `json:"languages"`
	Protocol    string   `json:"protocol"`
	Endpoint    string   `json:"endpoint"`
	Visibility  string   `json:"visibility"`
	Status      string   `json:"status"`
	PushedAt    string   `json:"pushedAt"`
	// Provider 是平台条目里的嵌套来源信息（CLI 原样送过来）。
	Provider struct {
		Handle       string `json:"handle"`
		DisplayName  string `json:"displayName"`
		ProviderKind string `json:"providerKind"`
		Region       string `json:"region"`
	} `json:"provider"`
	// Source 是引用摘要（service / capability）。
	Source struct {
		Ref string `json:"ref"`
	} `json:"source"`
}

// pushIndex POST /api/index —— 接收一条推来的索引（需凭据；幂等覆盖）。
func (s *Server) pushIndex(c *gin.Context) {
	a := authOf(c)
	if a == nil {
		fail(c, 401, "unauthorized", "未认证或凭据无效")
		return
	}
	var req indexPushReq
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_request", "请求体格式错误")
		return
	}
	if strings.TrimSpace(req.ID) == "" {
		fail(c, 400, "bad_request", "缺少 id：索引的权威在平台，节点只收平台推来的条目（重推按 id 覆盖）")
		return
	}
	channel := normalizeChannel(req.Channel)
	if channel == "" {
		fail(c, 400, "bad_request", "缺少 channel（频道，如 booking/hotel）")
		return
	}
	title := strings.TrimSpace(req.Title)
	if title == "" {
		fail(c, 400, "bad_request", "缺少 title")
		return
	}
	kind := strings.TrimSpace(req.Kind)
	if kind == "" {
		kind = "service"
	}
	// 登记人身份：优先用平台给的名片信息，没有就用 owner 兜底。
	owner := strings.TrimSpace(req.Provider.Handle)
	display := strings.TrimSpace(req.Provider.DisplayName)
	if owner == "" {
		owner = strings.TrimSpace(req.Owner)
	}
	if display == "" {
		display = strings.TrimSpace(req.DisplayName)
	}
	ref := strings.TrimSpace(req.Ref)
	if ref == "" {
		ref = strings.TrimSpace(req.Source.Ref)
	}
	var pushedAt time.Time
	if req.PushedAt != "" {
		if t, err := time.Parse(time.RFC3339, req.PushedAt); err == nil {
			pushedAt = t
		}
	}
	v, err := s.St.UpsertIndex(store.IndexInput{
		SourceID:     strings.TrimSpace(req.ID),
		OwnerKey:     strings.TrimSpace(req.OwnerKey),
		Owner:        truncateRunes(owner, 80),
		DisplayName:  truncateRunes(display, 80),
		ProviderKind: strings.TrimSpace(req.Provider.ProviderKind),
		Region:       truncateRunes(firstNonEmpty(req.Region, req.Provider.Region), 60),
		Kind:         kind,
		Channel:      channel,
		Slug:         strings.TrimSpace(req.Slug),
		Ref:          truncateRunes(ref, 120),
		Title:        truncateRunes(title, maxIndexTitle),
		Summary:      truncateRunes(req.Summary, 300),
		Description:  truncateRunes(req.Description, 4000),
		Category:     strings.TrimSpace(req.Category),
		Tags:         cleanList(req.Tags, 12, 24),
		Intents:      cleanList(req.Intents, 20, 40),
		Languages:    cleanList(req.Languages, 6, 12),
		Protocol:     truncateRunes(req.Protocol, 20),
		Endpoint:     truncateRunes(req.Endpoint, 400),
		Visibility:   orDefault(req.Visibility, "public"),
		Status:       orDefault(req.Status, "active"),
		PushedAt:     pushedAt,
	})
	if err != nil {
		fail(c, 500, "internal", "接收索引失败")
		return
	}
	ok(c, 200, gin.H{
		"ok": true, "id": v.ID, "sourceId": v.SourceID, "channel": v.Channel,
		"updated": !v.UpdatedAt.IsZero(),
	})
}

// listIndex GET /api/index?channel=&kind=&side=&q=&region=&limit=
func (s *Server) listIndex(c *gin.Context) {
	rows, err := s.St.ListIndex(store.IndexListOpts{
		Channel:    normalizeChannel(c.Query("channel")),
		Kind:       strings.TrimSpace(c.Query("kind")),
		Side:       strings.TrimSpace(c.Query("side")),
		Q:          strings.TrimSpace(c.Query("q")),
		Region:     strings.TrimSpace(c.Query("region")),
		PublicOnly: true,
		Limit:      intParam(c, "limit", 40),
	})
	if err != nil {
		fail(c, 500, "internal", "读取索引失败")
		return
	}
	list := make([]gin.H, 0, len(rows))
	for i := range rows {
		list = append(list, indexJSON(&rows[i]))
	}
	ok(c, 200, gin.H{"index": list, "total": len(list), "source": "node", "kind": "node"})
}

// indexChannels GET /api/index/channels
func (s *Server) indexChannels(c *gin.Context) {
	rows, err := s.St.IndexChannels()
	if err != nil {
		fail(c, 500, "internal", "读取频道失败")
		return
	}
	list := make([]gin.H, 0, len(rows))
	for _, r := range rows {
		list = append(list, gin.H{"channel": r.Channel, "entries": r.Entries})
	}
	ok(c, 200, gin.H{"channels": list, "total": len(list), "source": "node", "kind": "node"})
}

// matchIndex GET /api/match?intent=&channel=&region=&side=&limit=
//
// 本地匹配：只在本节点收到的索引里找。排序 = 相关度（**没有信誉权重** ——
// 评分是平台的内部权重，副本不该假装知道）。
func (s *Server) matchIndex(c *gin.Context) {
	intent := truncateRunes(c.Query("intent"), 200)
	channel := normalizeChannel(c.Query("channel"))
	region := truncateRunes(c.Query("region"), 60)
	side := orDefault(c.Query("side"), "supply")
	if side != "supply" && side != "need" {
		fail(c, 400, "bad_request", "side 只能是 supply 或 need")
		return
	}
	if strings.TrimSpace(intent) == "" && channel == "" && region == "" {
		fail(c, 400, "bad_request", "请给出 intent（一句需求）或 channel / region 之一")
		return
	}
	limit := intParam(c, "limit", 10)
	if limit < 1 {
		limit = 10
	}
	if limit > 50 {
		limit = 50
	}
	rows, err := s.St.ListIndex(store.IndexListOpts{
		Channel: channel, Side: side, Region: region,
		PublicOnly: true, Limit: 500,
	})
	if err != nil {
		fail(c, 500, "internal", "读取索引失败")
		return
	}
	tokens := tokenizeIntent(intent)
	type scored struct {
		row     *model.IndexEntry
		score   int
		reasons []string
	}
	hits := make([]scored, 0, len(rows))
	for i := range rows {
		sc, reasons := indexLocalScore(&rows[i], tokens, channel, region)
		if sc == 0 {
			continue
		}
		hits = append(hits, scored{row: &rows[i], score: sc, reasons: reasons})
	}
	sort.SliceStable(hits, func(i, j int) bool {
		if hits[i].score != hits[j].score {
			return hits[i].score > hits[j].score
		}
		return hits[i].row.UpdatedAt.After(hits[j].row.UpdatedAt)
	})
	if len(hits) > limit {
		hits = hits[:limit]
	}
	ids := make([]string, 0, len(hits))
	results := make([]gin.H, 0, len(hits))
	for _, h := range hits {
		ids = append(ids, h.row.ID)
		results = append(results, gin.H{
			"score": h.score, "reasons": h.reasons, "index": indexJSON(h.row),
		})
	}
	s.St.BumpIndexHits(ids)
	ok(c, 200, gin.H{
		"query":   gin.H{"intent": intent, "channel": channel, "side": side, "region": region},
		"results": results, "count": len(results), "candidates": len(rows),
		"source": "node", "kind": "node",
		// 说清这里少了什么：本地副本没有平台的信誉权重
		"note": "本地索引：排序只有相关度（信誉权重在平台侧，节点不持有评分）",
	})
}

/* ---------------- 打分（节点侧：只有相关度） ---------------- */

func indexLocalScore(e *model.IndexEntry, tokens []string, channel, region string) (int, []string) {
	relevance := 0
	// textSignal：「真的读懂了你在说什么」的那部分。频道 / 区域只用来缩小范围，
	// 不能单独把人召回（与平台同一套口径）。
	textSignal := 0
	reasons := []string{}

	if channel != "" {
		if e.Channel == channel {
			relevance += 30
			reasons = append(reasons, "频道命中："+e.Channel)
		} else if strings.HasPrefix(e.Channel, channel+"/") {
			relevance += 18
			reasons = append(reasons, "频道命中："+e.Channel)
		}
	}
	if len(tokens) > 0 {
		tags := store.ParseList(e.Tags)
		intents := store.ParseList(e.Intents)
		hitTags := []string{}
		for _, tk := range tokens {
			for _, h := range append(append([]string{}, tags...), intents...) {
				if strings.EqualFold(h, tk) {
					hitTags = append(hitTags, h)
					break
				}
			}
		}
		if n := len(hitTags) * 8; n > 32 {
			relevance += 32
			textSignal += 32
		} else {
			relevance += n
			textSignal += n
		}
		if len(hitTags) > 0 {
			if len(hitTags) > 5 {
				hitTags = hitTags[:5]
			}
			reasons = append(reasons, "标签 / 关键词命中："+strings.Join(hitTags, "、"))
		}

		title := strings.ToLower(e.Title)
		ch := strings.ToLower(e.Channel)
		body := strings.ToLower(strings.Join([]string{e.Summary, e.Description, strings.Join(intents, " ")}, " "))
		weight := 0
		hits := []string{}
		for _, tk := range tokens {
			w := 0
			if strings.Contains(title, tk) {
				w = 3
			} else if strings.Contains(ch, tk) {
				w = 2
			} else if strings.Contains(body, tk) {
				w = 1
			}
			if w > 0 {
				weight += w
				hits = append(hits, tk)
			}
		}
		if weight > 40 {
			weight = 40
		}
		relevance += weight
		textSignal += weight
		if len(hits) > 0 {
			// 长的优先、短的若被长的包含就不重复列（与平台同一套可读性处理）
			sort.SliceStable(hits, func(i, j int) bool {
				return len([]rune(hits[i])) > len([]rune(hits[j]))
			})
			keep := make([]string, 0, len(hits))
			for _, tk := range hits {
				covered := false
				for _, k := range keep {
					if strings.Contains(k, tk) {
						covered = true
						break
					}
				}
				if !covered {
					keep = append(keep, tk)
				}
			}
			if len(keep) > 4 {
				keep = keep[:4]
			}
			reasons = append(reasons, "关键词命中："+strings.Join(keep, "、"))
		}
	}
	if region != "" && e.RegionMatch(region) {
		relevance += 10
		reasons = append(reasons, "区域覆盖："+e.Region)
	}
	// 说了话却一个字都没命中：不匹配（频道 / 区域不能单独召回）
	if len(tokens) > 0 && textSignal == 0 {
		return 0, nil
	}
	if relevance == 0 {
		return 0, nil
	}
	if e.Endpoint != "" {
		relevance += 4
		reasons = append(reasons, "公开可直接接取")
	}
	if time.Since(e.UpdatedAt) < 30*24*time.Hour {
		relevance += 4
	}
	return relevance, reasons
}

/* ---------------- 视图与小工具 ---------------- */

// indexJSON 节点侧的索引视图。**没有评分字段**（节点不持有评分）。
func indexJSON(e *model.IndexEntry) gin.H {
	out := gin.H{
		"id": e.ID, "sourceId": e.SourceID, "kind": e.Kind,
		"channel": e.Channel, "slug": e.Slug, "ref": e.Ref,
		"title": e.Title, "summary": e.Summary, "description": e.Description,
		"category": e.Category, "region": e.Region,
		"tags": store.ParseList(e.Tags), "intents": store.ParseList(e.Intents),
		"languages": store.ParseList(e.Languages),
		"protocol":  e.Protocol, "endpoint": e.Endpoint,
		"visibility": e.Visibility, "status": e.Status, "hits": e.Hits,
		"updatedAt": e.UpdatedAt, "receivedAt": e.ReceivedAt, "pushedAt": e.PushedAt,
		"provider": gin.H{
			"handle": e.Owner, "displayName": e.DisplayName,
			"providerKind": e.ProviderKind, "region": e.Region,
		},
	}
	if e.Endpoint != "" {
		out["howTo"] = gin.H{"endpoint": e.Endpoint, "protocol": e.Protocol, "open": true}
	} else if e.Ref != "" {
		out["howTo"] = gin.H{"step": "引用原件：" + e.Ref}
	}
	return out
}

// normalizeChannel 频道规范化（与平台同一套规则：小写、去空段、下划线/点/空格统一成段分隔符，
// 段内连字符折叠 —— `Food____RES` 要与 `food-res` 是同一个频道）。
func normalizeChannel(s string) string {
	s = strings.ToLower(strings.TrimSpace(s))
	s = strings.NewReplacer("_", "-", " ", "-", ".", "-").Replace(s)
	for strings.Contains(s, "--") {
		s = strings.ReplaceAll(s, "--", "-")
	}
	parts := strings.FieldsFunc(s, func(r rune) bool { return r == '/' })
	out := make([]string, 0, len(parts))
	for _, p := range parts {
		p = strings.Trim(p, "-")
		if p != "" {
			out = append(out, p)
		}
	}
	return strings.Join(out, "/")
}

func orDefault(v, def string) string {
	if strings.TrimSpace(v) == "" {
		return def
	}
	return strings.TrimSpace(v)
}

func cleanList(in []string, maxItems, maxLen int) []string {
	out := make([]string, 0, len(in))
	seen := map[string]bool{}
	for _, v := range in {
		v = truncateRunes(strings.TrimSpace(v), maxLen)
		if v == "" || seen[v] {
			continue
		}
		seen[v] = true
		out = append(out, v)
		if len(out) >= maxItems {
			break
		}
	}
	return out
}

func truncateRunes(s string, n int) string {
	r := []rune(strings.TrimSpace(s))
	if len(r) <= n {
		return string(r)
	}
	return string(r[:n])
}

func intParam(c *gin.Context, key string, def int) int {
	if v := strings.TrimSpace(c.Query(key)); v != "" {
		n := 0
		for _, ch := range v {
			if ch < '0' || ch > '9' {
				return def
			}
			n = n*10 + int(ch-'0')
		}
		return n
	}
	return def
}

// tokenizeIntent 与平台同口径的意图切词：空白与标点切分，中文补 2-4 字滑窗。
// 复制一份而不是共用：两个仓库各自独立发版，跨仓共享代码会让「节点升级」与
// 「平台升级」绑在一起（本地节点常常比平台旧，这条是我们刻意保留的自由度）。
func tokenizeIntent(s string) []string {
	s = strings.ToLower(strings.TrimSpace(s))
	if s == "" {
		return nil
	}
	fields := strings.FieldsFunc(s, func(r rune) bool {
		switch r {
		case ' ', '\t', '\n', ',', '.', ';', ':', '!', '?', '/', '\\', '(', ')', '[', ']',
			'{', '}', '"', '\'', '“', '”', '，', '。', '、', '；', '：', '！', '？', '（', '）', '「', '」',
			'-', '_', '+', '@', '#', '*', '|', '~', '=', '<', '>':
			return true
		}
		return false
	})
	out := make([]string, 0, len(fields)*3)
	seen := map[string]bool{}
	add := func(t string) {
		t = strings.TrimSpace(t)
		if len([]rune(t)) < 2 || seen[t] {
			return
		}
		seen[t] = true
		out = append(out, t)
	}
	for _, f := range fields {
		if f == "" {
			continue
		}
		if isASCII(f) {
			add(f)
			continue
		}
		runes := []rune(f)
		if n := len(runes); n <= 8 {
			add(f)
		}
		for size := 4; size >= 2; size-- {
			for i := 0; i+size <= len(runes); i++ {
				add(string(runes[i : i+size]))
			}
		}
	}
	if len(out) > 24 {
		out = out[:24]
	}
	return out
}

func isASCII(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] > 127 {
			return false
		}
	}
	return true
}
