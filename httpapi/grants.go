package httpapi

import (
	"strings"

	"github.com/gin-gonic/gin"
	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

// 分发授权（Grant）。
//
// 连接与授权是两件事：
//
//	NodeLink（见 nodes.go）——「我能连到哪些节点」：找得到
//	Grant（本文件）      ——「谁能看/取我的东西」：拿得到
//
// 私有制品 / 私有节点一律要求显式授权：否则「连一下就能把我所有私有内容拿走」，
// 边界太糊。授权按「授权人 + 被授权人 + 类型 + 命名空间」唯一，重复授予即更新备注。

/* ---------------- 序列化 ---------------- */

func (s *Server) userBrief(userID string) gin.H {
	b := gin.H{"id": userID}
	u, err := s.St.FindUserByID(userID)
	if err != nil {
		return b
	}
	b["name"] = u.Name
	b["displayName"] = u.Name
	if ns, err := s.St.PersonalNamespace(u.ID); err == nil {
		b["handle"] = "@" + ns.Slug
		b["namespace"] = ns.Slug
	}
	return b
}

func grantJSON(g *model.Grant, grantee gin.H, ns gin.H) gin.H {
	return gin.H{
		"id": g.ID, "kind": g.Kind, "namespaceId": g.NamespaceID,
		"namespace": ns, "note": g.Note,
		"grantee": grantee, "createdAt": g.CreatedAt,
	}
}

// resolveUser 按 @handle（个人命名空间 slug）或用户 id 指人。
func (s *Server) resolveUser(ref string) (*model.User, error) {
	ref = strings.TrimSpace(ref)
	if ref == "" {
		return nil, gorm.ErrRecordNotFound
	}
	if !strings.HasPrefix(ref, "@") {
		if u, err := s.St.FindUserByID(ref); err == nil {
			return u, nil
		}
	}
	ns, err := s.St.FindNamespaceBySlug(strings.TrimPrefix(ref, "@"))
	if err != nil {
		return nil, gorm.ErrRecordNotFound
	}
	return s.St.FindUserByID(ns.OwnerID)
}

/* ---------------- 接口 ---------------- */

// listGrants GET /api/grants?direction=outgoing|incoming
func (s *Server) listGrants(c *gin.Context) {
	a := authOf(c)
	incoming := c.Query("direction") == "incoming"
	var owner, grantee string
	if incoming {
		grantee = a.UserID
	} else {
		owner = a.UserID
	}
	rows, err := s.St.ListGrants(owner, grantee)
	if err != nil {
		fail(c, 500, "internal", "服务内部错误")
		return
	}
	list := make([]gin.H, 0, len(rows))
	for i := range rows {
		g := rows[i]
		otherID := g.GranteeUserID
		if incoming {
			otherID = g.OwnerID
		}
		var ns gin.H
		if g.NamespaceID != "" {
			slug := ""
			if n, err := s.St.FindNamespaceByID(g.NamespaceID); err == nil {
				slug = n.Slug
			}
			ns = gin.H{"id": g.NamespaceID, "slug": slug}
		}
		list = append(list, grantJSON(&g, s.userBrief(otherID), ns))
	}
	dir := "outgoing"
	if incoming {
		dir = "incoming"
	}
	ok(c, 200, gin.H{"grants": list, "direction": dir, "total": len(list)})
}

// createGrant POST /api/grants —— body: {ref, kind, namespace, note}
func (s *Server) createGrant(c *gin.Context) {
	a := authOf(c)
	var body struct {
		Ref       string `json:"ref"`
		UserID    string `json:"userId"`
		Kind      string `json:"kind"`
		Namespace string `json:"namespace"`
		Note      string `json:"note"`
	}
	if err := c.ShouldBindJSON(&body); err != nil {
		fail(c, 400, "bad_request", "请求体格式错误")
		return
	}
	if !model.ValidGrantKind(body.Kind) {
		fail(c, 400, "bad_request", "kind 只能是 "+strings.Join(model.GrantKinds, " | "))
		return
	}
	u, err := s.resolveUser(firstNonEmpty(body.Ref, body.UserID))
	if err != nil {
		fail(c, 404, "not_found", "找不到这个用户（用 @用户名 或用户 id）")
		return
	}
	if u.ID == a.UserID {
		fail(c, 400, "bad_request", "不需要给自己授权")
		return
	}

	nsID := ""
	if ns := strings.TrimSpace(body.Namespace); ns != "" {
		if body.Kind != model.GrantArtifact {
			fail(c, 400, "bad_request", "只有 artifact 授权可以限定命名空间")
			return
		}
		n, err := s.St.FindNamespaceBySlug(strings.TrimPrefix(ns, "@"))
		if err != nil {
			fail(c, 404, "not_found", "命名空间不存在："+ns)
			return
		}
		if !s.canManage(n.ID, a.UserID) {
			fail(c, 403, "forbidden", "只能授权你自己的命名空间")
			return
		}
		nsID = n.ID
	}

	note := strings.TrimSpace(body.Note)
	if len([]rune(note)) > 200 {
		note = string([]rune(note)[:200])
	}
	g, err := s.St.UpsertGrant(store.GrantInput{
		OwnerID: a.UserID, GranteeUserID: u.ID, Kind: body.Kind, NamespaceID: nsID, Note: note,
	})
	if err != nil {
		fail(c, 500, "internal", "授权失败")
		return
	}
	var nsBrief gin.H
	if nsID != "" {
		slug := ""
		if n, err := s.St.FindNamespaceByID(nsID); err == nil {
			slug = n.Slug
		}
		nsBrief = gin.H{"id": nsID, "slug": slug}
	}
	ok(c, 201, gin.H{"grant": grantJSON(g, s.userBrief(u.ID), nsBrief)})
}

// deleteGrant DELETE /api/grants/:id
func (s *Server) deleteGrant(c *gin.Context) {
	a := authOf(c)
	g, err := s.St.FindGrant(c.Param("id"))
	if err != nil || g.OwnerID != a.UserID {
		fail(c, 404, "not_found", "授权不存在或不是你发出的")
		return
	}
	if err := s.St.DeleteGrant(g.ID, a.UserID); err != nil {
		fail(c, 500, "internal", "撤销失败")
		return
	}
	ok(c, 200, gin.H{"ok": true})
}

/* ---------------- 判定 ---------------- */

// canReadArtifact 读权限：公开已发布人人可读；其余要么是命名空间成员，要么拿到授权。
func (s *Server) canReadArtifact(row *store.ArtifactRow, userID string) bool {
	if row.Status == "published" && row.Visibility == "public" {
		return true
	}
	if userID == "" {
		return false
	}
	if s.canManage(row.NamespaceID, userID) {
		return true
	}
	// 发布者把「制品获取权」授给了这个人（可按命名空间限定）
	return s.St.HasGrant(row.CreatedBy, userID, model.GrantArtifact, row.NamespaceID)
}

// canSeeNode 可见性：公开节点人人可见；私有节点要么归属自己，要么拿到 node 授权。
func (s *Server) canSeeNode(n *store.NodeRow, userID string) bool {
	if n.Visibility == "public" {
		return true
	}
	if userID == "" {
		return false
	}
	if n.OwnerID == userID || s.canManage(n.NamespaceID, userID) {
		return true
	}
	return s.St.HasGrant(n.OwnerID, userID, model.GrantNode, n.NamespaceID)
}

// grantedOwners 我拿到 node 授权的人 —— 让他们的私有节点出现在 discover / 连接里。
func (s *Server) grantedOwners(userID string) []string {
	if userID == "" {
		return nil
	}
	owners, err := s.St.GrantedOwners(userID, model.GrantNode)
	if err != nil {
		return nil
	}
	return owners
}
