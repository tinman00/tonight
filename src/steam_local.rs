//! 本地 Steam 数据（design.md C1 / §7.2 steam_local）：
//! 纯读文件，无网络、无副作用。解析 libraryfolders.vdf、appmanifest_*.acf、loginusers.vdf。

use std::path::{Path, PathBuf};

use crate::vdf::{self, VdfValue};

#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("未找到 Steam 安装目录：请在 config.toml 的 [steam] install_dir 指定")]
    SteamDirNotFound,
    #[error("读取 {path} 失败: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("解析 {path} 失败: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: vdf::VdfError,
    },
    #[error("{0} 结构不符合预期")]
    Malformed(String),
}

/// 一个已安装应用（来自某个库的 appmanifest_*.acf）
#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub app_id: u32,
    #[allow(dead_code)] // 推荐里程碑（2a 事实包）使用
    pub name: String,
    pub size_bytes: u64,
    pub fully_installed: bool,
    pub needs_update: bool,
    pub update_bytes: u64,
}

/// loginusers.vdf 中的本机账号
#[derive(Debug, Clone)]
pub struct LoginUser {
    pub steam_id64: String,
    pub account_name: String,
    pub persona_name: String,
    pub most_recent: bool,
    /// 最后登录时间（unix 秒）。新版 Steam 客户端常不写 MostRecent，但有 Timestamp；
    /// 旧客户端/异常条目可能没有，记 0。
    pub timestamp: i64,
}

/// 自动探测 Steam 安装目录：
/// Windows：注册表 → Program Files 常见路径；
/// Linux：~/.steam/steam → ~/.local/share/Steam → ~/.steam/debian-installation → Flatpak；
/// macOS：~/Library/Application Support/Steam。
pub fn detect_steam_dir() -> Result<PathBuf, LocalError> {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        if let Ok(steam) = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam") {
            if let Ok(path) = steam.get_value::<String, _>("SteamPath") {
                if Path::new(&path).is_dir() {
                    return Ok(PathBuf::from(path));
                }
            }
        }
        for candidate in [r"C:\Program Files (x86)\Steam", r"C:\Program Files\Steam"] {
            if Path::new(candidate).is_dir() {
                return Ok(PathBuf::from(candidate));
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        // Linux（~/.steam/steam 通常是符号链接，is_dir 照样成立）
        #[cfg(target_os = "linux")]
        for candidate in [
            home.join(".steam").join("steam"),
            home.join(".local").join("share").join("Steam"),
            home.join(".steam").join("debian-installation"),
            home.join(".var").join("app").join("com.valvesoftware.Steam").join("data").join("Steam"),
        ] {
            if candidate.is_dir() {
                return Ok(candidate);
            }
        }
        // macOS
        #[cfg(target_os = "macos")]
        {
            let p = home.join("Library").join("Application Support").join("Steam");
            if p.is_dir() {
                return Ok(p);
            }
        }
    }
    Err(LocalError::SteamDirNotFound)
}

/// Steam 目录裁决：config.toml `[steam] install_dir` 显式优先（空串视为未设置），
/// 否则自动探测。agent/server 两处消费方共用，避免"配了 config 仍走探测"的口径漂移。
pub fn resolve_steam_dir(cfg: &crate::config::Config) -> Result<PathBuf, LocalError> {
    match cfg.steam_install_dir() {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => detect_steam_dir(),
    }
}

/// 解析单个 appmanifest_*.acf 文本。acf 根级是单键 "AppState" 包着真正的内容。
fn parse_app_manifest(text: &str) -> Option<InstalledApp> {
    let parsed = vdf::parse(text).ok()?;
    let root = parsed.get("AppState").unwrap_or(&parsed);
    let app_id = root.str_at("appid")?.parse::<u32>().ok()?;
    let num = |key: &str| root.str_at(key).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    // StateFlags 位含义来自社区文档：bit2(值 4) 完整安装、bit1(值 2) 待更新；
    // 另以 BytesToDownload > BytesDownloaded 兜底（下载中/已排程更新）
    let state_flags = num("StateFlags");
    let to_download = num("BytesToDownload");
    let downloaded = num("BytesDownloaded");
    Some(InstalledApp {
        app_id,
        name: root.str_at("name").unwrap_or("").to_string(),
        size_bytes: num("SizeOnDisk"),
        fully_installed: state_flags & 4 != 0,
        needs_update: state_flags & 2 != 0 || to_download > downloaded,
        update_bytes: to_download.saturating_sub(downloaded),
    })
}

/// Windows 下同一目录可能写成不同大小写/分隔符（注册表小写正斜杠 vs vdf 大写反斜杠），
/// 判断是否同一物理路径时按“小写 + 正斜杠”归一化比较。
fn same_filesystem_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().to_lowercase().replace('\\', "/");
    norm(a) == norm(b)
}

pub struct SteamLocal {
    steam_dir: PathBuf,
}

impl SteamLocal {
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        SteamLocal { steam_dir: dir.into() }
    }

    fn read_vdf(&self, rel: &str) -> Result<VdfValue, LocalError> {
        let path = self.steam_dir.join(rel);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| LocalError::Io { path: path.clone(), source: e })?;
        vdf::parse(&text).map_err(|e| LocalError::Parse { path, source: e })
    }

    /// libraryfolders.vdf 中各库的 steamapps 目录。
    pub fn library_paths(&self) -> Result<Vec<PathBuf>, LocalError> {
        let root = self.read_vdf("config/libraryfolders.vdf")?;
        let folders = root
            .get("libraryfolders")
            .and_then(|v| v.as_map())
            .ok_or_else(|| LocalError::Malformed("libraryfolders.vdf 缺少 libraryfolders 段".into()))?;
        let mut out = Vec::new();
        for (_, entry) in folders {
            if let Some(p) = entry.str_at("path") {
                let apps = PathBuf::from(p).join("steamapps");
                if apps.is_dir() {
                    out.push(apps);
                }
            }
        }
        // 老版 Steam 的 libraryfolders.vdf 可能不列主库，兜底补上；
        // 注意与 vdf 里的路径做大小写/分隔符不敏感的同一目录判断，避免同库扫两遍
        let main = self.steam_dir.join("steamapps");
        if main.is_dir() && !out.iter().any(|p| same_filesystem_path(p, &main)) {
            out.insert(0, main);
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|p| {
            seen.insert(p.to_string_lossy().to_lowercase().replace('\\', "/"))
        });
        tracing::debug!(steam_dir = %self.steam_dir.display(), paths = ?out, "library_paths 解析结果");
        Ok(out)
    }

    /// 扫描所有库的 appmanifest_*.acf。单个清单损坏只跳过并告警，不中断整体。
    pub fn installed_apps(&self) -> Result<Vec<InstalledApp>, LocalError> {
        let mut out = Vec::new();
        for lib in self.library_paths()? {
            tracing::debug!(dir = %lib.display(), "扫描库目录");
            let entries = match std::fs::read_dir(&lib) {
                Ok(e) => e,
                Err(e) => return Err(LocalError::Io { path: lib, source: e }),
            };
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                if !(fname.starts_with("appmanifest_") && fname.ends_with(".acf")) {
                    continue;
                }
                let path = entry.path();
                let text = match std::fs::read_to_string(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("跳过无法读取的清单 {}: {e}", path.display());
                        continue;
                    }
                };
                match parse_app_manifest(&text) {
                    Some(app) => out.push(app),
                    None => tracing::warn!("跳过无法解析的清单 {}", path.display()),
                }
            }
        }
        Ok(out)
    }

    /// config/loginusers.vdf → 本机登录过的账号。
    pub fn login_users(&self) -> Result<Vec<LoginUser>, LocalError> {
        let root = self.read_vdf("config/loginusers.vdf")?;
        let users = root
            .get("users")
            .and_then(|v| v.as_map())
            .ok_or_else(|| LocalError::Malformed("loginusers.vdf 缺少 users 段".into()))?;
        Ok(users
            .iter()
            .filter_map(|(id, v)| {
                let timestamp = v
                    .str_at("Timestamp")
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                Some(LoginUser {
                    steam_id64: id.clone(),
                    // 残留/半登录条目可能没有 AccountName——不能因此丢掉整个条目，
                    // 否则唯一账号的场景会退化成"检测不到"（真机踩过）
                    account_name: v.str_at("AccountName").unwrap_or("").to_string(),
                    persona_name: v.str_at("PersonaName").unwrap_or("").to_string(),
                    most_recent: v.str_at("MostRecent") == Some("1"),
                    timestamp,
                })
            })
            .collect())
    }

    /// 最近一次在本机登录的账号（首次引导自动带出，design.md F1）。
    /// 裁决顺序：MostRecent="1" → Timestamp 最新（新版 Steam 常不写 MostRecent）→
    /// 仅一个账号 → 放弃。
    pub fn most_recent_user(&self) -> Result<Option<LoginUser>, LocalError> {
        Ok(pick_recent_user(&self.login_users()?))
    }

    /// 主库所在磁盘的剩余空间（字节）。预约下载的权衡提示用。
    pub fn disk_free_bytes(&self) -> Option<u64> {
        let lib = self.library_paths().ok()?.into_iter().next()?;
        fs4::free_space(&lib).ok()
    }
}

/// 纯函数：MostRecent="1" 优先 → Timestamp 最新且 >0（新版 Steam 无 MostRecent 但有
/// Timestamp）→ 仅一个账号 → 放弃。
fn pick_recent_user(users: &[LoginUser]) -> Option<LoginUser> {
    if let Some(u) = users.iter().find(|u| u.most_recent) {
        return Some(u.clone());
    }
    if let Some(u) = users
        .iter()
        .filter(|u| u.timestamp > 0)
        .max_by_key(|u| u.timestamp)
    {
        return Some(u.clone());
    }
    (users.len() == 1).then(|| users[0].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取自本机真实 appmanifest_730.acf 的结构与字段值（CS2）
    #[test]
    fn parses_real_app_manifest() {
        let text = "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"StateFlags\"\t\t\"6\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n\t\"SizeOnDisk\"\t\t\"71579839350\"\n\t\"BytesToDownload\"\t\t\"26179728\"\n\t\"BytesDownloaded\"\t\t\"0\"\n}\n";
        let app = parse_app_manifest(text).unwrap();
        assert_eq!(app.app_id, 730);
        assert_eq!(app.name, "Counter-Strike 2");
        assert_eq!(app.size_bytes, 71_579_839_350);
        // StateFlags 6 = 4(完整安装) | 2(待更新)
        assert!(app.fully_installed);
        assert!(app.needs_update);
        assert_eq!(app.update_bytes, 26_179_728);
    }

    #[test]
    fn parses_manifest_without_appstate_wrapper() {
        let text = "\"appid\"\t\"105600\"\n\"name\"\t\"Terraria\"\n\"StateFlags\"\t\"4\"\n";
        let app = parse_app_manifest(text).unwrap();
        assert_eq!(app.app_id, 105600);
        assert!(app.fully_installed);
        assert!(!app.needs_update);
    }

    #[test]
    fn rejects_manifest_without_appid() {
        assert!(parse_app_manifest("\"name\"\t\"无 id 的清单\"\n").is_none());
    }

    #[test]
    fn same_path_compares_case_and_separator_insensitively() {
        assert!(same_filesystem_path(
            Path::new(r"D:\04_Entertainment\Steam\steamapps"),
            Path::new("d:/04_entertainment/steam/steamapps"),
        ));
        assert!(!same_filesystem_path(
            Path::new(r"D:\Steam\steamapps"),
            Path::new(r"D:\SteamLibrary\steamapps"),
        ));
    }

    #[test]
    fn picks_recent_user_with_and_without_flag() {
        let user = |name: &str, most_recent: bool| LoginUser {
            steam_id64: format!("765611980000{name}"),
            account_name: name.into(),
            persona_name: name.into(),
            most_recent,
            timestamp: 0,
        };
        // 有 MostRecent 标记：选它（即使不是第一个）
        let users = vec![user("a", false), user("b", true)];
        assert_eq!(pick_recent_user(&users).unwrap().account_name, "b");
        // 无标记 + 单账号：直接用（新版 Steam 客户端场景）
        let users = vec![user("only", false)];
        assert_eq!(pick_recent_user(&users).unwrap().account_name, "only");
        // 无标记 + 多账号但都有 Timestamp：取最近登录的（新版 Steam 客户端场景）
        let mut users = vec![user("old", false), user("new", false)];
        users[0].timestamp = 1_000;
        users[1].timestamp = 2_000;
        assert_eq!(pick_recent_user(&users).unwrap().account_name, "new");
        // 无标记、无 Timestamp、多账号：无法裁决
        let users = vec![user("a", false), user("b", false)];
        assert!(pick_recent_user(&users).is_none());
        // 无标记 + 单账号但 Timestamp=0：仍走单账号兜底
        let users = vec![user("solo", false)];
        assert_eq!(pick_recent_user(&users).unwrap().account_name, "solo");
    }

    #[test]
    fn entry_without_account_name_is_kept() {
        // 残留/半登录条目可能没有 AccountName：不能丢弃，否则唯一账号场景检测失效
        let text = "\"users\"\n{\n\"76561198000000001\"\n{\n\"PersonaName\"\t\"只有昵称的人\"\n\"Timestamp\"\t\"1700000000\"\n}\n}\n";
        let parsed = crate::vdf::parse(text).unwrap();
        let users = parsed.get("users").and_then(|v| v.as_map()).unwrap();
        let kept: Vec<_> = users
            .iter()
            .filter_map(|(id, v)| {
                Some(LoginUser {
                    steam_id64: id.clone(),
                    account_name: v.str_at("AccountName").unwrap_or("").to_string(),
                    persona_name: v.str_at("PersonaName").unwrap_or("").to_string(),
                    most_recent: v.str_at("MostRecent") == Some("1"),
                    timestamp: v.str_at("Timestamp").and_then(|s| s.parse().ok()).unwrap_or(0),
                })
            })
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].account_name, "");
        assert_eq!(kept[0].persona_name, "只有昵称的人");
        assert_eq!(kept[0].timestamp, 1_700_000_000);
    }
}
