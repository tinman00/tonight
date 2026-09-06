//! .env 密钥写入（Web 设置页 / 首次引导用，design.md R3）。
//! 设计约束：密钥值只落盘、只进进程环境，永不进日志或错误信息；
//! 写入是原子的（临时文件 + rename），并保留文件里既有的注释与未知行。

use std::io::Write;
use std::path::Path;

/// 新建 .env 时的文件头（保留用户手写注释的说明作用）。
const HEADER: &str = "# 「今晚玩什么」密钥文件（本机保存，勿提交仓库 / 勿外传）";

/// 在 `path`（通常是项目根 .env）中设置 `NAME=VALUE`：
/// 已有未注释的同名行则原位替换（保留其余行与顺序），否则追加到末尾；
/// 文件不存在则创建并带头注释。CRLF 统一归一为 LF（dotenvy 两者都认）。
pub fn set_env_var(path: &Path, name: &str, value: &str) -> std::io::Result<()> {
    debug_assert!(!name.contains('=') && !name.is_empty());
    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(_) => vec![HEADER.to_string()],
    };
    let new_line = format!("{name}={value}");
    let mut replaced = false;
    for line in lines.iter_mut() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let key = trimmed.split('=').next().unwrap_or("").trim();
        if key == name {
            *line = new_line.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(new_line);
    }
    let tmp = path.with_extension("env.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        for l in &lines {
            f.write_all(l.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
        f.sync_all().ok();
    }
    // Windows 上 rename 直接覆盖已存在目标（MOVEFILE_REPLACE_EXISTING）
    std::fs::rename(&tmp, path)
}

/// 密钥值校验：非空、无空白与控制字符、长度上限（API key 不会含空格/换行，
/// 误贴多行文本在这里拦下）。错误信息只描述原因，不回显值。
pub fn validate_key(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("密钥为空".into());
    }
    if v.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("密钥格式无效（含空格或换行，请只粘贴密钥本身）".into());
    }
    if v.len() > 512 {
        return Err("密钥过长（超过 512 字符），请确认粘贴的是密钥本身".into());
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tonight-secrets-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn creates_new_file_with_header() {
        let dir = tmpdir("create");
        let p = dir.join(".env");
        set_env_var(&p, "STEAM_WEB_API_KEY", "ABC123").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with('#'));
        assert!(text.contains("STEAM_WEB_API_KEY=ABC123"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replaces_existing_and_preserves_comments_and_unknown_lines() {
        let dir = tmpdir("replace");
        let p = dir.join(".env");
        std::fs::write(
            &p,
            "# 我的注释\r\nSTEAM_WEB_API_KEY=old\r\nDEEPSEEK_API_KEY=keep-me\r\n\r\nFOO=bar\r\n",
        )
        .unwrap();
        set_env_var(&p, "STEAM_WEB_API_KEY", "new").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("STEAM_WEB_API_KEY=new"));
        assert!(!text.contains("old"));
        assert!(text.contains("# 我的注释"));
        assert!(text.contains("DEEPSEEK_API_KEY=keep-me"));
        assert!(text.contains("FOO=bar"));
        // 被注释掉的同名行不算既有行：应追加，而不是动注释
        std::fs::write(&p, "# STEAM_WEB_API_KEY=commented\n").unwrap();
        set_env_var(&p, "STEAM_WEB_API_KEY", "v2").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# STEAM_WEB_API_KEY=commented"));
        assert!(text.ends_with("STEAM_WEB_API_KEY=v2\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validate_key_rejects_bad_input_without_echoing_value() {
        assert!(validate_key("  ").is_err());
        let multi = validate_key("abc def");
        assert!(multi.is_err());
        assert!(!format!("{multi:?}").contains("abc")); // 错误信息不回显值
        assert!(validate_key("  sk-abc123 ").is_ok());
    }
}
