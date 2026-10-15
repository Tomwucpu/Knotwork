//! 对象标识 [`Oid`]。
//!
//! v1.0 只承诺 SHA-1 仓库（NFR-OS-01 的 Windows 场景下绝大多数仓库如此），
//! 因此采用 20 字节定长数组。SHA-256 仓库（32 字节）在 `gt-backend` 的
//! `gix` 适配层被拒绝并给出明确错误，而不是静默截断。

use std::fmt;
use std::str::FromStr;

/// SHA-1 对象标识的字节长度。
pub const OID_LEN: usize = 20;

/// 十六进制形式的完整长度。
const OID_HEX_LEN: usize = OID_LEN * 2;

/// 短标识长度，与 `git log --abbrev-commit` 默认值一致。
const SHORT_LEN: usize = 7;

/// Git 对象标识（SHA-1）。
///
/// `Ord` 按字节序排列，与 `git rev-list` 的字典序一致，因此可以直接用于
/// 集合比对（一致性测试）而不需要转成字符串。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid([u8; OID_LEN]);

impl Oid {
    /// 全零标识，代表"不存在"。
    pub const ZERO: Self = Self([0; OID_LEN]);

    /// 由原始字节构造；长度不是 [`OID_LEN`] 时返回 [`None`]。
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let mut buf = [0u8; OID_LEN];
        if bytes.len() != OID_LEN {
            return None;
        }
        buf.copy_from_slice(bytes);
        Some(Self(buf))
    }

    /// 返回原始字节。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; OID_LEN] {
        &self.0
    }

    /// 是否为全零标识。
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0 == [0; OID_LEN]
    }

    /// 完整十六进制表示（40 字符小写）。
    #[must_use]
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(OID_HEX_LEN);
        for byte in self.0 {
            // 每个字节固定写两位，避免 `format!` 的分配开销。
            out.push(HEX[(byte >> 4) as usize]);
            out.push(HEX[(byte & 0x0f) as usize]);
        }
        out
    }

    /// 短标识（默认 7 字符），用于列表展示。
    #[must_use]
    pub fn short(self) -> String {
        self.to_hex()[..SHORT_LEN].to_owned()
    }

    /// 按指定长度取短标识；`len` 会被裁剪到 `1..=40`。
    #[must_use]
    pub fn short_with(self, len: usize) -> String {
        let len = len.clamp(1, OID_HEX_LEN);
        self.to_hex()[..len].to_owned()
    }

    /// 解析十六进制字符串；长度或字符非法时返回 [`None`]。
    ///
    /// 长度为 40（完整）或 4..=39（前缀）都接受；前缀按右侧补零处理，
    /// 仅用于显示场景，**不得用于对象查找**。
    #[must_use]
    pub fn from_prefix_hex(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() < 4 || bytes.len() > OID_HEX_LEN {
            return None;
        }

        let mut buf = [0u8; OID_LEN];
        let mut index = 0usize;
        while index < bytes.len() {
            let hi = hex_value(bytes[index])?;
            // 奇数长度时末位半字节补零，保证"前缀"也能解析成功。
            let lo = match bytes.get(index + 1) {
                Some(byte) => hex_value(*byte)?,
                None => 0,
            };
            buf[index / 2] = (hi << 4) | lo;
            index += 2;
        }
        Some(Self(buf))
    }
}

/// 十六进制字符表，供 [`Oid::to_hex`] 直接索引。
const HEX: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// 单个十六进制字符转半字节。
const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug 输出短标识，避免日志被 40 字符哈希刷屏。
        write!(f, "Oid({})", self.short())
    }
}

/// 解析错误：长度或字符不合法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseOidError;

impl fmt::Display for ParseOidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("对象标识必须是 40 位十六进制字符串")
    }
}

impl std::error::Error for ParseOidError {}

impl FromStr for Oid {
    type Err = ParseOidError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != OID_HEX_LEN {
            return Err(ParseOidError);
        }
        Self::from_prefix_hex(text).ok_or(ParseOidError)
    }
}

#[cfg(test)]
mod tests {
    use super::{Oid, ParseOidError};
    use std::str::FromStr;

    fn sample() -> Oid {
        Oid::from_bytes(&[
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff, 0x01, 0x23, 0x45, 0x67,
        ])
        .expect("长度正确")
    }

    #[test]
    fn hex_round_trips() {
        let oid = sample();
        assert_eq!(oid.to_hex(), "00112233445566778899aabbccddeeff01234567");
        assert_eq!(Oid::from_str(&oid.to_hex()), Ok(oid));
    }

    #[test]
    fn short_id_is_seven_chars() {
        assert_eq!(sample().short(), "0011223");
        assert_eq!(sample().short_with(10), "0011223344");
    }

    #[test]
    fn wrong_length_is_rejected() {
        assert_eq!(Oid::from_bytes(&[0u8; 19]), None);
        assert_eq!(Oid::from_bytes(&[0u8; 21]), None);
        assert_eq!(Oid::from_str("abc"), Err(ParseOidError));
    }

    #[test]
    fn uppercase_prefix_is_accepted() {
        let mut expected = [0u8; super::OID_LEN];
        expected[0] = 0x00;
        expected[1] = 0x11;
        assert_eq!(Oid::from_prefix_hex("0011"), Oid::from_bytes(&expected));
        assert_eq!(
            Oid::from_prefix_hex("0011")
                .map(Oid::to_hex)
                .map(|s| s.len()),
            Some(40)
        );
        assert_eq!(Oid::from_prefix_hex("zzzz"), None);
        assert_eq!(Oid::from_prefix_hex("001"), None, "少于 4 位视为非法");
    }

    #[test]
    fn odd_length_prefix_pads_low_nibble() {
        // "000ab" 共 5 位：0x00、0x0a、0xb0（末位半字节补零）。
        let mut expected = [0u8; super::OID_LEN];
        expected[0] = 0x00;
        expected[1] = 0x0a;
        expected[2] = 0xb0;
        assert_eq!(Oid::from_prefix_hex("000ab"), Oid::from_bytes(&expected));
    }

    #[test]
    fn zero_detection() {
        assert!(Oid::ZERO.is_zero());
        assert!(!sample().is_zero());
    }

    #[test]
    fn ordering_follows_byte_order() {
        let mut values = [sample(), Oid::ZERO];
        values.sort_unstable();
        assert_eq!(values[0], Oid::ZERO);
    }
}
