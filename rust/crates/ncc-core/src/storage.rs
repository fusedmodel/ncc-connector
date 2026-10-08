//! 制品字节的本地磁盘存储。
//!
//! 两个服务共用同一套实现，差别只有「对外 URL 前缀」：
//! 内网节点是 `/blobs/<name>`，平台是 `/uploads/<name>`。刻意保留各自前缀，
//! 因为这两种前缀都写进了既有客户端与文档。

use std::path::{Path, PathBuf};

/// 本地磁盘存储：字节落在 `<dir>/<name>`。
pub struct LocalStorage {
    dir: PathBuf,
    public_prefix: String,
    public_base: String,
}

impl LocalStorage {
    /// `name_in_url` 例如 `blobs` / `uploads`（不带斜杠）。
    pub fn new(dir: impl AsRef<Path>, public_base: &str, name_in_url: &str) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            public_prefix: format!("/{}", name_in_url.trim_matches('/')),
            public_base: public_base.trim_end_matches('/').to_string(),
        })
    }

    /// 防目录穿越：只允许「干净的相对路径」。
    ///
    /// 对象名是**斜杠分隔**的标识（会进 URL、也会在 master/worker 之间原样传递），
    /// 所以一律按 posix 语义处理：反斜杠与冒号只可能来自越权输入
    /// （Windows 上分别代表分隔符与盘符/ADS）。
    fn safe_name(name: &str) -> Result<String, String> {
        if name.is_empty() || name.contains('\\') || name.contains(':') {
            return Err(format!("非法对象名 {name:?}"));
        }
        // 手工规范化：只用 '/'，不引入平台差异
        let mut parts: Vec<&str> = Vec::new();
        for seg in name.split('/') {
            match seg {
                "" | "." => continue,
                ".." => {
                    return Err(format!("非法对象名 {name:?}"));
                }
                s => parts.push(s),
            }
        }
        let clean = parts.join("/");
        if clean.is_empty() {
            return Err(format!("非法对象名 {name:?}"));
        }
        Ok(clean)
    }

    fn path_of(&self, name: &str) -> Result<PathBuf, String> {
        let clean = Self::safe_name(name)?;
        Ok(self.dir.join(clean.replace('/', std::path::MAIN_SEPARATOR_STR)))
    }

    /// 写入字节（原子写：先写临时文件再 rename），返回可对外访问的地址。
    pub fn put(&self, name: &str, data: &[u8]) -> Result<String, String> {
        let p = self.path_of(name)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
        }
        let tmp = p.with_extension(format!(
            "{}tmp",
            p.extension()
                .map(|e| format!("{}.", e.to_string_lossy()))
                .unwrap_or_default()
        ));
        std::fs::write(&tmp, data).map_err(|e| format!("写入文件失败: {e}"))?;
        std::fs::rename(&tmp, &p).map_err(|e| format!("落盘失败: {e}"))?;
        Ok(self.public_url(name))
    }

    /// 读取字节。
    pub fn get(&self, name: &str) -> Result<Vec<u8>, String> {
        let p = self.path_of(name)?;
        std::fs::read(&p).map_err(|e| format!("读取文件失败: {e}"))
    }

    /// 字节大小。
    pub fn size(&self, name: &str) -> Result<u64, String> {
        let p = self.path_of(name)?;
        std::fs::metadata(&p)
            .map(|m| m.len())
            .map_err(|e| format!("读取文件失败: {e}"))
    }

    /// 删除。文件本来就不在算成功 —— 重入的清理任务不该因为「第二次删」报错。
    pub fn delete(&self, name: &str) -> Result<(), String> {
        let p = self.path_of(name)?;
        match std::fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("删除失败: {e}")),
        }
    }

    /// 某个已存对象的对外地址。
    pub fn public_url(&self, name: &str) -> String {
        let clean = Self::safe_name(name).unwrap_or_else(|_| name.to_string());
        format!("{}{}/{}", self.public_base, self.public_prefix, clean)
    }

    /// 字节目录（静态文件服务要用）。
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 目录穿越被拒() {
        assert!(LocalStorage::safe_name("../etc/passwd").is_err());
        assert!(LocalStorage::safe_name("a/../../b").is_err());
        assert!(LocalStorage::safe_name("").is_err());
        assert!(LocalStorage::safe_name("a\\b").is_err());
        assert!(LocalStorage::safe_name("C:evil").is_err());
        assert_eq!(LocalStorage::safe_name("a/b/c.hur.gz").unwrap(), "a/b/c.hur.gz");
        assert_eq!(LocalStorage::safe_name("/a//b/").unwrap(), "a/b");
    }

    #[test]
    fn 落盘与地址() {
        let dir = std::env::temp_dir().join(format!("ncc-storage-test-{}", std::process::id()));
        let s = LocalStorage::new(&dir, "http://localhost:8282/", "blobs").unwrap();
        let url = s.put("x/y.bin", b"hello").unwrap();
        assert_eq!(url, "http://localhost:8282/blobs/x/y.bin");
        assert_eq!(s.get("x/y.bin").unwrap(), b"hello");
        assert_eq!(s.size("x/y.bin").unwrap(), 5);
        s.delete("x/y.bin").unwrap();
        assert!(s.get("x/y.bin").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
