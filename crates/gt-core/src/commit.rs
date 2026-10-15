//! 提交元数据与提交图输入。

use crate::oid::Oid;

/// 一条提交的展示与图计算所需的元数据。
///
/// 只保留 Log 列表与泳道图渲染需要的字段；提交正文（body）按需二次读取，
/// 避免为百万级提交列表常驻大量文本（架构规划 5.2 / 5.8）。
///
/// 时间字段为 Unix 秒（`i64`，与 `git log --format=%at` 一致），
/// 相对时间（"3 天前"）由 UI 层格式化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMeta {
    /// 提交对象标识。
    pub id: Oid,
    /// 父提交标识，顺序即 `git` 的父顺序（第一个为第一父项）。
    pub parents: Vec<Oid>,
    /// 提交信息首行；空信息时为空串。
    pub summary: String,
    /// 作者名。
    pub author_name: String,
    /// 作者邮箱。
    pub author_email: String,
    /// 作者时间（Unix 秒）。
    pub author_time: i64,
    /// 提交者时间（Unix 秒）。
    pub committer_time: i64,
}

impl CommitMeta {
    /// 短标识（7 字符）。
    #[must_use]
    pub fn short_id(&self) -> String {
        self.id.short()
    }

    /// 是否为合并提交（父多于一个）。
    #[must_use]
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }

    /// 第一父项；根提交返回 [`None`]。
    #[must_use]
    pub fn first_parent(&self) -> Option<Oid> {
        self.parents.first().copied()
    }

    /// 转换为泳道图输入节点。
    #[must_use]
    pub fn as_log_node(&self) -> LogNode {
        LogNode {
            id: self.id,
            parents: self.parents.clone(),
        }
    }
}

/// 泳道图布局的最小输入单元。
///
/// `gt-graph` 只依赖该结构，从而与 [`CommitMeta`] 的其余字段解耦——
/// 布局算法不需要作者、消息等展示数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogNode {
    /// 提交对象标识。
    pub id: Oid,
    /// 父提交标识，顺序即父顺序。
    pub parents: Vec<Oid>,
}

impl LogNode {
    /// 是否为合并提交。
    #[must_use]
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

impl From<&CommitMeta> for LogNode {
    fn from(meta: &CommitMeta) -> Self {
        Self {
            id: meta.id,
            parents: meta.parents.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CommitMeta, LogNode};
    use crate::oid::Oid;

    fn oid(seed: u8) -> Oid {
        let mut bytes = [0u8; crate::OID_LEN];
        bytes[0] = seed;
        Oid::from_bytes(&bytes).expect("长度正确")
    }

    fn meta(parents: Vec<Oid>) -> CommitMeta {
        CommitMeta {
            id: oid(1),
            parents,
            summary: "feat: 示例".to_owned(),
            author_name: "Alice".to_owned(),
            author_email: "alice@example.com".to_owned(),
            author_time: 1_700_000_000,
            committer_time: 1_700_000_100,
        }
    }

    #[test]
    fn merge_detection_matches_parent_count() {
        assert!(!meta(vec![oid(2)]).is_merge());
        assert!(meta(vec![oid(2), oid(3)]).is_merge());
    }

    #[test]
    fn first_parent_is_first_in_order() {
        let commit = meta(vec![oid(2), oid(3)]);
        assert_eq!(commit.first_parent(), Some(oid(2)));
        assert_eq!(meta(vec![]).first_parent(), None);
    }

    #[test]
    fn log_node_conversion_keeps_parents() {
        let commit = meta(vec![oid(2), oid(3)]);
        let node = LogNode::from(&commit);
        assert_eq!(node.id, commit.id);
        assert_eq!(node.parents, commit.parents);
        assert!(node.is_merge());
        assert_eq!(commit.as_log_node(), node);
    }
}
