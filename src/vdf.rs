//! Valve KeyValues (VDF) 极简解析器。
//! 只覆盖 Steam 本地清单文件（libraryfolders.vdf / appmanifest_*.acf / loginusers.vdf）
//! 用到的子集：引号字符串、嵌套 map、`//` 注释、`\\` 与 `\"` 转义、UTF-8 BOM。

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum VdfValue {
    Str(String),
    Map(Vec<(String, VdfValue)>),
}

#[derive(Debug)]
pub struct VdfError {
    pub pos: usize,
    pub msg: String,
}

impl fmt::Display for VdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VDF 解析错误（字节 {}）: {}", self.pos, self.msg)
    }
}

impl std::error::Error for VdfError {}

/// 解析 VDF 文本，返回根级 map。
pub fn parse(text: &str) -> Result<VdfValue, VdfError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // 去掉 BOM
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    let pairs = p.parse_pairs()?;
    if p.i < p.s.len() {
        return Err(p.err("根级有多余内容"));
    }
    Ok(VdfValue::Map(pairs))
}

impl VdfValue {
    /// 取第一个匹配 key 的子项（同 key 重复时以首个为准）。
    pub fn get(&self, key: &str) -> Option<&VdfValue> {
        match self {
            VdfValue::Map(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            VdfValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(String, VdfValue)]> {
        match self {
            VdfValue::Map(p) => Some(p),
            _ => None,
        }
    }

    /// 便捷取值：`root.str_at("appid")`
    pub fn str_at(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    /// 解析若干 `key value` / `key { ... }` 对；遇到 `}` 或文件结尾时结束（消费 `}`）。
    fn parse_pairs(&mut self) -> Result<Vec<(String, VdfValue)>, VdfError> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia();
            if self.i >= self.s.len() {
                return Ok(out);
            }
            if self.s[self.i] == b'}' {
                self.i += 1;
                return Ok(out);
            }
            let key = if self.s[self.i] == b'"' {
                self.parse_string()?
            } else {
                self.parse_bare_token()?
            };
            self.skip_trivia();
            let value = self.parse_value()?;
            out.push((key, value));
        }
    }

    fn parse_value(&mut self) -> Result<VdfValue, VdfError> {
        if self.i >= self.s.len() {
            return Err(self.err("期望值，遇到文件结尾"));
        }
        match self.s[self.i] {
            b'{' => {
                self.i += 1;
                Ok(VdfValue::Map(self.parse_pairs()?))
            }
            b'"' => Ok(VdfValue::Str(self.parse_string()?)),
            _ => Ok(VdfValue::Str(self.parse_bare_token()?)),
        }
    }

    fn parse_string(&mut self) -> Result<String, VdfError> {
        self.i += 1; // 跳过开引号
        let mut bytes = Vec::new();
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'\\' if self.i + 1 < self.s.len() => {
                    let esc = self.s[self.i + 1];
                    self.i += 2;
                    match esc {
                        b'\\' => bytes.push(b'\\'),
                        b'"' => bytes.push(b'"'),
                        b'n' => bytes.push(b'\n'),
                        b't' => bytes.push(b'\t'),
                        b'r' => bytes.push(b'\r'),
                        other => bytes.push(other), // 未知转义按原字节保留
                    }
                }
                b'"' => {
                    self.i += 1;
                    return String::from_utf8(bytes).map_err(|_| self.err("非法 UTF-8 字符串"));
                }
                b => {
                    bytes.push(b);
                    self.i += 1;
                }
            }
        }
        Err(self.err("字符串未闭合"))
    }

    fn parse_bare_token(&mut self) -> Result<String, VdfError> {
        let start = self.i;
        while self.i < self.s.len()
            && !matches!(self.s[self.i], b' ' | b'\t' | b'\r' | b'\n' | b'{' | b'}' | b'"')
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(self.err(format!("非法字符 '{}'", self.s[start] as char)));
        }
        String::from_utf8(self.s[start..self.i].to_vec()).map_err(|_| self.err("非法 UTF-8"))
    }

    fn skip_trivia(&mut self) {
        loop {
            while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\r' | b'\n') {
                self.i += 1;
            }
            if self.i + 1 < self.s.len() && self.s[self.i] == b'/' && self.s[self.i + 1] == b'/' {
                while self.i < self.s.len() && self.s[self.i] != b'\n' {
                    self.i += 1;
                }
            } else {
                return;
            }
        }
    }

    fn err(&self, msg: impl Into<String>) -> VdfError {
        VdfError { pos: self.i, msg: msg.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_appmanifest() {
        let text = r#""AppState"
{
	"appid"		"105600"
	"name"		"Terraria"
	"StateFlags"		"4"
	"SizeOnDisk"		"123456789"
	"installdir"		"Terraria"
	"UserConfig"
	{
		"language"		"schinese"
	}
}"#;
        let root = parse(text).unwrap();
        let app = root.get("AppState").unwrap();
        assert_eq!(app.str_at("appid"), Some("105600"));
        assert_eq!(app.str_at("name"), Some("Terraria"));
        assert_eq!(app.str_at("SizeOnDisk"), Some("123456789"));
        assert_eq!(
            app.get("UserConfig").unwrap().str_at("language"),
            Some("schinese")
        );
    }

    #[test]
    fn parses_libraryfolders_with_escaped_path() {
        // 文件里的路径形如 "D:\\SteamLibrary"（源码层面四条反斜杠）
        let text = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t\t\"label\"\t\t\"\"\n\t}\n}\n";
        let root = parse(text).unwrap();
        let entry = root.get("libraryfolders").unwrap().get("0").unwrap();
        assert_eq!(entry.str_at("path"), Some(r"D:\SteamLibrary"));
    }

    #[test]
    fn parses_loginusers_and_comments() {
        let text = "// 本机账号\n\"users\"\n{\n\t\"76561198000000000\"\n\t{\n\t\t\"AccountName\"\t\"alice\"\n\t\t\"PersonaName\"\t\"Alice\"\n\t\t\"MostRecent\"\t\"1\"\n\t}\n}\n";
        let root = parse(text).unwrap();
        let users = root.get("users").unwrap().as_map().unwrap();
        assert_eq!(users.len(), 1);
        let (id, v) = &users[0];
        assert_eq!(id, "76561198000000000");
        assert_eq!(v.str_at("AccountName"), Some("alice"));
        assert_eq!(v.str_at("MostRecent"), Some("1"));
    }

    #[test]
    fn strips_bom() {
        let text = "\u{feff}\"key\"\t\"value\"\n";
        let root = parse(text).unwrap();
        assert_eq!(root.str_at("key"), Some("value"));
    }
}
