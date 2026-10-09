//! TUI 配置文档的读写（`$WING_HOME/tui/config.yaml`）。
//!
//! Interface 根的全部 I/O 都在这里：稀疏文档读取 + 文件字节指纹 + 原子写（tmp + `sync_all` +
//! `rename`）+ 写前 `.bak`。三个语义约定：
//!
//! 1. **稀疏**：`doc` 只有用户显式写下的键（`{}` 表示空文件 / 文件不存在）。缺席 = 跟随默认，
//!    这正是 `dump_config_yaml` 把默认值写成注释的原因。
//! 2. **读失败不吞**：文件坏掉时返回 [`StoreError::Parse`]（原文一字不动），不静默回落到默认值
//!    ——调用方（10 步骤的面板）该展示问题并拒绝保存，否则一次保存就会覆盖掉用户手写的文件。
//! 3. **指纹**：文件字节的 FNV-1a 64（`fnv1a64:<16 hex>`），文件不存在 = `"absent"`
//!    （与后端 `config/document.py` 的 sha256 同口径：文件变了指纹就变）。Rust 侧没有 `sha2`
//!    （不为它加依赖），而乐观并发只需要"变没变"，不需要抗碰撞。
//!
//! 公开入口按环境变量解析路径（[`interface_config_path`]），内核（`*_from` / `*_to`）吃显式路径
//! ——测试因此不需要改 `WING_HOME`（edition 2024 的 `set_var` 是 `unsafe`，而且并行单测会互踩）。

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;

use crate::config::AppConfig;
use crate::config::catalog::DumpMode;
use crate::config::catalog::dump_config_yaml;

/// 文件不存在时的指纹字面量（与后端同口径）。
pub const ABSENT_FINGERPRINT: &str = "absent";

/// 读到的 TUI 配置文档。
#[derive(Debug, Clone, PartialEq)]
pub struct InterfaceDoc {
    /// 稀疏文档（只有文件里写下的键）。
    pub doc: Value,
    /// 文件字节指纹；文件不存在 = [`ABSENT_FINGERPRINT`]。
    pub fingerprint: String,
    /// 实际读的路径（面板标题与回执展示）。
    pub path: PathBuf,
}

/// 写盘结论（10 步骤用它更新本地指纹 / 写回执）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub path: PathBuf,
    /// 写入后文件字节的指纹。
    pub fingerprint: String,
    /// 写前备份的路径（目标原来不存在时为 `None`）。
    pub backup_path: Option<PathBuf>,
}

/// 读写失败面。
#[derive(Debug)]
pub enum StoreError {
    /// `$WING_HOME` 与 home 目录都不可得（无法定位配置文件）。
    NoConfigPath,
    /// 读文件失败（非 NotFound）。
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// YAML 解析失败：原文原样保留在 `path`。
    Parse { path: PathBuf, message: String },
    /// 写文件失败（含备份、建目录、原子替换）。
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigPath => write!(
                f,
                "cannot determine the TUI config directory (neither $WING_HOME nor $HOME is set)"
            ),
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { path, message } => {
                write!(f, "cannot parse {}: {message}", path.display())
            }
            Self::Write { path, source } => write!(f, "cannot write {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// `$WING_HOME/tui/config.yaml`（`WING_HOME` 覆盖 `~/.wing`）；无法确定时 `None`。
pub fn interface_config_path() -> Option<PathBuf> {
    let home = match std::env::var("WING_HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home),
        _ => dirs::home_dir()?.join(".wing"),
    };
    Some(home.join("tui").join("config.yaml"))
}

/// 读 `$WING_HOME/tui/config.yaml` 为稀疏文档；文件不存在 → `(json!({}), "absent")`。
pub fn read_interface_doc() -> Result<InterfaceDoc, StoreError> {
    let path = interface_config_path().ok_or(StoreError::NoConfigPath)?;
    read_interface_doc_from(&path)
}

/// [`read_interface_doc`] 的显式路径内核（测试与显式路径调用走这里）。
pub(crate) fn read_interface_doc_from(path: &Path) -> Result<InterfaceDoc, StoreError> {
    match fs::read(path) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).map_err(|e| StoreError::Parse {
                path: path.to_owned(),
                message: format!("not valid UTF-8: {e}"),
            })?;
            let doc = parse_document(text, path)?;
            Ok(InterfaceDoc {
                doc,
                fingerprint: fingerprint(&bytes),
                path: path.to_owned(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(InterfaceDoc {
            doc: Value::Object(serde_json::Map::new()),
            fingerprint: ABSENT_FINGERPRINT.to_owned(),
            path: path.to_owned(),
        }),
        Err(e) => Err(StoreError::Read {
            path: path.to_owned(),
            source: e,
        }),
    }
}

/// 原子写 `$WING_HOME/tui/config.yaml`：`.bak` → tmp + `sync_all` + `rename`。
///
/// 写盘内容 = [`dump_config_yaml`] 的 [`DumpMode::Raw`] 输出（`--dump-config` 与保存共用同一个
/// emitter，但模式相反：**保存永远写密文真值**，展示默认写掩码）。
pub fn write_interface_doc(doc: &Value) -> Result<WriteOutcome, StoreError> {
    let path = interface_config_path().ok_or(StoreError::NoConfigPath)?;
    write_interface_doc_to(&path, doc)
}

/// [`write_interface_doc`] 的显式路径内核（测试走这里）。
pub(crate) fn write_interface_doc_to(path: &Path, doc: &Value) -> Result<WriteOutcome, StoreError> {
    let backup_path = if path.exists() {
        let backup = backup_path(path)?;
        // 读旧文件 → 原子写：`fs::copy` 中途被杀会留下截断的备份（B2），这里与后端
        // `common/fs.py::atomic_write_bytes` 同语义（`atomic_write` 只有文本版，
        // 按既有口径不改它）。旧文件是配置文本，原样逐字节写回；非 UTF-8 的旧文件根本
        // 进不了配置（`read_interface_doc_from` 同样拒绝），这里直接以 Read 错误拒绝整条保存。
        let previous = fs::read_to_string(path).map_err(|e| StoreError::Read {
            path: path.to_owned(),
            source: e,
        })?;
        atomic_write(&backup, &previous)?;
        Some(backup)
    } else {
        None
    };
    // **保存路径永远是 `Raw`**：写掩码 = 把用户的密钥换成掩码字符串 = 数据损坏。
    // 展示路径（`--dump-config`）在 `cmd/mod.rs` 里显式传 `Masked`。
    let text = dump_config_yaml(doc, DumpMode::Raw);
    atomic_write(path, &text)?;
    Ok(WriteOutcome {
        path: path.to_owned(),
        fingerprint: fingerprint(text.as_bytes()),
        backup_path,
    })
}

/// 稀疏文档 → `AppConfig`（实时预览用）。
///
/// 与 [`AppConfig::load`] **共用同一条解析实现**（`AppConfig::from_doc`）：同样的大小写不敏感
/// enum、同样的非法值回落 + warn、同样的 `resolve()`（`rendering.math` → `colors.math_mode`）。
pub fn appconfig_from_doc(doc: &Value) -> AppConfig {
    AppConfig::from_doc(doc)
}

/// YAML 文本 → 稀疏文档。
///
/// 空文件 → `{}`（不是 `null`）；根不是映射 → 解析错误（`42` 之类不可能是配置）。
pub(crate) fn parse_document(text: &str, path: &Path) -> Result<Value, StoreError> {
    let parse_error = |message: String| StoreError::Parse {
        path: path.to_owned(),
        message,
    };
    let value: serde_yaml::Value = if text.trim().is_empty() {
        serde_yaml::Value::Null
    } else {
        serde_yaml::from_str(text).map_err(|e| parse_error(e.to_string()))?
    };
    let document = yaml_to_json(value).map_err(parse_error)?;
    match document {
        Value::Null => Ok(Value::Object(serde_json::Map::new())),
        Value::Object(_) => Ok(document),
        other => Err(parse_error(format!(
            "the document root must be a mapping, got {}",
            kind_of(&other)
        ))),
    }
}

/// YAML 值 → JSON 值（显式转换：非字符串键 / 无穷大这类 JSON 装不下的东西要报错，不能悄悄变 null）。
fn yaml_to_json(value: serde_yaml::Value) -> Result<Value, String> {
    Ok(match value {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(int) = n.as_i64() {
                Value::from(int)
            } else if let Some(uint) = n.as_u64() {
                Value::from(uint)
            } else if let Some(float) = n.as_f64() {
                serde_json::Number::from_f64(float)
                    .map(Value::Number)
                    .ok_or_else(|| format!("cannot represent {n} in JSON (NaN / Infinity)"))?
            } else {
                return Err(format!("unsupported YAML number: {n}"));
            }
        }
        serde_yaml::Value::String(s) => Value::String(s),
        serde_yaml::Value::Sequence(items) => Value::Array(
            items
                .into_iter()
                .map(yaml_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        serde_yaml::Value::Mapping(mapping) => {
            let mut object = serde_json::Map::with_capacity(mapping.len());
            for (key, value) in mapping {
                let serde_yaml::Value::String(key) = key else {
                    return Err(format!(
                        "configuration keys must be strings, got {}",
                        key_kind(&key)
                    ));
                };
                object.insert(key, yaml_to_json(value)?);
            }
            Value::Object(object)
        }
        // tag（`!!str` 之类）不携带配置语义：解包取它的值。
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(tagged.value)?,
    })
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a sequence",
        Value::Object(_) => "a mapping",
    }
}

fn key_kind(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "a boolean",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a sequence",
        serde_yaml::Value::Mapping(_) => "a mapping",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

/// 内容指纹：FNV-1a 64（`fnv1a64:<16 hex>`）。
///
/// 不用 `DefaultHasher`（种子不保证跨进程稳定），也不为 `sha2` 加依赖——这个指纹只回答
/// "文件变了没有"。格式与后端的 sha256 不同、语义相同：同一份字节同一个值，不存在 = `"absent"`。
fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn backup_path(path: &Path) -> Result<PathBuf, StoreError> {
    let file_name = path.file_name().ok_or_else(|| StoreError::Write {
        path: path.to_owned(),
        source: std::io::Error::other("path has no file name"),
    })?;
    Ok(path.with_file_name(format!("{}.bak", file_name.to_string_lossy())))
}

/// tmp + `sync_all` + `rename`（与后端 `common/fs.py::atomic_write_text` 同语义）。
fn atomic_write(path: &Path, text: &str) -> Result<(), StoreError> {
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let write_error = |e: std::io::Error| StoreError::Write {
        path: path.to_owned(),
        source: e,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(write_error)?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| write_error(std::io::Error::other("path has no file name")))?;
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(write_error(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::config::ColorPreset;
    use crate::config::LayoutConfig;
    use crate::config::ThemePalette;
    use crate::config::rendering::MathMode;
    use crate::config::rendering::ThinkingMode;

    /// 一个自清理的临时目录（`tempfile` 不是依赖，也不为此加依赖）。
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "wing-config-store-{}-{}-{}",
                std::process::id(),
                name,
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.0.join(relative)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read_text(path: &std::path::Path) -> String {
        fs::read_to_string(path).expect("the file is there")
    }

    #[test]
    fn a_missing_file_reads_as_absent() {
        let dir = TestDir::new("missing");
        let file = dir.path("tui/config.yaml");
        let read = read_interface_doc_from(&file).expect("a missing file is not an error");
        assert_eq!(read.doc, json!({}));
        assert_eq!(read.fingerprint, ABSENT_FINGERPRINT);
        assert_eq!(read.path, file);
    }

    #[test]
    fn an_empty_file_reads_as_an_empty_document() {
        let dir = TestDir::new("empty");
        let file = dir.path("config.yaml");
        fs::write(&file, "").unwrap();
        let read = read_interface_doc_from(&file).expect("an empty file is an empty document");
        assert_eq!(read.doc, json!({}));
        assert_ne!(
            read.fingerprint, ABSENT_FINGERPRINT,
            "空文件与不存在的文件不是一回事"
        );
    }

    #[test]
    fn a_sparse_document_reads_back_verbatim_and_the_fingerprint_tracks_the_bytes() {
        let dir = TestDir::new("sparse");
        let file = dir.path("config.yaml");
        fs::write(
            &file,
            "colors:\n  accent: \"#ff00ff\"\nrendering:\n  math: off\n",
        )
        .unwrap();
        let read = read_interface_doc_from(&file).unwrap();
        assert_eq!(
            read.doc,
            json!({"colors": {"accent": "#ff00ff"}, "rendering": {"math": "off"}})
        );
        let first = read.fingerprint.clone();
        assert_eq!(read_interface_doc_from(&file).unwrap().fingerprint, first);

        // 字节变了 → 指纹变了
        fs::write(
            &file,
            "colors:\n  accent: \"#ff00ff\"\nrendering:\n  math: text\n",
        )
        .unwrap();
        assert_ne!(read_interface_doc_from(&file).unwrap().fingerprint, first);

        // 指纹只看内容，不看路径
        let other = dir.path("elsewhere.yaml");
        fs::write(&other, read_text(&file)).unwrap();
        assert_eq!(
            read_interface_doc_from(&other).unwrap().fingerprint,
            read_interface_doc_from(&file).unwrap().fingerprint
        );
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_silent_default() {
        let dir = TestDir::new("broken");
        let file = dir.path("config.yaml");
        for text in ["colors: [\n", "42\n", "1: 2\n"] {
            fs::write(&file, text).unwrap();
            match read_interface_doc_from(&file) {
                Err(StoreError::Parse { path, .. }) => assert_eq!(path, file),
                other => panic!("expected a parse error for {text:?}, got {other:?}"),
            }
            assert_eq!(read_text(&file), text, "原文一字不动");
        }
    }

    /// 面板路径报错，`load()` 路径按既有契约回落默认值——两种口径各自明确。
    #[test]
    fn a_broken_file_falls_back_to_defaults_for_load_but_errors_for_the_panel() {
        let dir = TestDir::new("load-broken");
        let file = dir.path("config.yaml");
        fs::write(&file, "colors: [\n").unwrap();
        assert_eq!(
            serde_json::to_value(AppConfig::load_from_path(&file)).unwrap(),
            serde_json::to_value(AppConfig::default()).unwrap()
        );
        assert!(read_interface_doc_from(&file).is_err());
    }

    #[test]
    fn a_write_is_atomic_backs_up_and_leaves_no_temp_files() {
        let dir = TestDir::new("write");
        let file = dir.path("nested/tui/config.yaml");
        let doc = json!({"colors": {"accent": "#ff00ff"}});

        let outcome = write_interface_doc_to(&file, &doc).expect("write creates parent dirs");
        assert_eq!(outcome.path, file);
        assert!(outcome.backup_path.is_none(), "首次写入没有可备份的旧文件");
        assert_eq!(
            read_text(&file),
            dump_config_yaml(&doc, DumpMode::Raw),
            "写盘内容 = catalog dump（Raw）"
        );
        let read = read_interface_doc_from(&file).unwrap();
        assert_eq!(read.doc, doc, "写出去的是规范形，读回来还是同一份稀疏文档");
        assert_eq!(read.fingerprint, outcome.fingerprint);

        let outcome = write_interface_doc_to(&file, &json!({})).expect("second write");
        let backup = outcome.backup_path.expect("第二次写入必须备份旧文件");
        assert_eq!(
            read_text(&backup),
            dump_config_yaml(&doc, DumpMode::Raw),
            "备份 = 改前的文件"
        );

        // 目录里只剩目标 + 备份：没有 tmp 残留
        let mut entries: Vec<String> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, ["config.yaml", "config.yaml.bak"]);
    }

    /// 含密文的样本（保存路径的不变量测试用）。
    const SAVE_SECRET: &str = "sk-live-secret-1234";

    /// **数据损坏级不变量**：保存路径写的是密文**真值**，不是掩码。
    ///
    /// `dump_config_yaml` 是 `--dump-config` 与保存共用的同一个 emitter——掩码只能出现在展示路径上
    /// （`cmd/mod.rs` 显式传 `DumpMode::Masked`）。谁把 `store` 这边"顺手统一"成 Masked，
    /// 用户的密钥就会被 8 个实心点覆盖，而且在用户下次 401 之前无人察觉。
    #[test]
    fn the_save_path_writes_the_real_secret_never_the_mask() {
        let dir = TestDir::new("save-secret");
        let file = dir.path("config.yaml");
        let doc = json!({"api_key": SAVE_SECRET, "colors": {"accent": "#ff00ff"}});

        write_interface_doc_to(&file, &doc).unwrap();
        let text = read_text(&file);
        assert!(text.contains(SAVE_SECRET), "保存要写真值：\n{text}");
        assert!(!text.contains('•'), "保存路径不许出现掩码字符：\n{text}");
        assert!(
            !text.contains("已掩码"),
            "保存路径不许出现掩码注释：\n{text}"
        );
        assert_eq!(
            text,
            dump_config_yaml(&doc, DumpMode::Raw),
            "保存内容 = Raw 模式的规范形"
        );
        assert_ne!(
            text,
            dump_config_yaml(&doc, DumpMode::Masked),
            "保存内容必须与展示（掩码）内容不同"
        );
        assert_eq!(
            read_interface_doc_from(&file).unwrap().doc,
            doc,
            "真值仍在文档里，读回来逐值相等"
        );

        // 第二次保存：备份里也是真值（B2 的改动不许把掩码引进备份）。
        let outcome = write_interface_doc_to(&file, &json!({"api_key": "sk-next-9999"})).unwrap();
        let backup = read_text(&outcome.backup_path.expect("第二次写入备份旧文件"));
        assert!(backup.contains(SAVE_SECRET), "备份要写真值：\n{backup}");
        assert!(!backup.contains('•'), "备份里不许有掩码：\n{backup}");
    }

    /// B2：`.bak` 是改前文件的**逐字节**副本，且写它是原子写（读字节 → tmp + rename），
    /// 不再用 `fs::copy`（拷贝中途被杀会留下截断的备份）。
    #[test]
    fn the_backup_is_the_previous_file_byte_for_byte() {
        let dir = TestDir::new("backup-bytes");
        let file = dir.path("config.yaml");
        // 手写的、非规范形的旧文件：CRLF、行尾注释、奇怪的空白、未知键、明文密钥。
        let hand_written = "# 手写文件（非规范形）\r\n\
                            api_key: sk-live-secret-1234   # 行尾注释\r\n\
                            \r\n\
                            colors:\r\n    accent:   \"#ff00ff\"\r\n\
                            unknown_key: 1\r\n";
        fs::write(&file, hand_written).unwrap();

        let outcome = write_interface_doc_to(&file, &json!({"layout": {"max_input_lines": 25}}))
            .expect("write");
        let backup = outcome.backup_path.expect("旧文件存在 → 必须备份");
        assert_eq!(
            fs::read(&backup).unwrap(),
            hand_written.as_bytes(),
            "备份必须逐字节等于改前文件（不重新序列化、不换行尾）"
        );
        // 主文件已被新内容原子替换。
        assert_eq!(
            read_interface_doc_from(&file).unwrap().doc,
            json!({"layout": {"max_input_lines": 25}})
        );
    }

    /// B2 不退化：目标不存在时**不产生** `.bak`（今天的行为）。
    #[test]
    fn no_backup_is_created_when_the_target_does_not_exist() {
        let dir = TestDir::new("no-backup");
        let file = dir.path("config.yaml");
        let outcome = write_interface_doc_to(&file, &json!({"api_key": SAVE_SECRET})).unwrap();
        assert!(outcome.backup_path.is_none(), "首次写入没有可备份的旧文件");
        assert!(!dir.path("config.yaml.bak").exists());
        let entries: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["config.yaml"], "目录里只有目标文件");
    }

    #[test]
    fn a_second_write_overwrites_the_previous_backup() {
        let dir = TestDir::new("backup");
        let file = dir.path("config.yaml");
        write_interface_doc_to(&file, &json!({"api_key": "one"})).unwrap();
        let second = write_interface_doc_to(&file, &json!({"api_key": "two"})).unwrap();
        let third = write_interface_doc_to(&file, &json!({"api_key": "three"})).unwrap();
        assert_eq!(
            second.backup_path, third.backup_path,
            "覆盖式：只留最近一份"
        );
        let backup = read_text(&third.backup_path.unwrap());
        assert!(backup.contains("api_key: two"), "{backup}");
        assert!(!backup.contains("api_key: one"), "{backup}");
    }

    #[test]
    fn a_write_then_read_round_trips_the_document() {
        let dir = TestDir::new("roundtrip");
        let file = dir.path("config.yaml");
        for doc in [
            json!({}),
            json!({"colors": {"preset": "terminal", "accent": "#ff00ff"}}),
            json!({"api_key": "sk-x", "layout": {"max_input_lines": 25}}),
        ] {
            write_interface_doc_to(&file, &doc).unwrap();
            let read = read_interface_doc_from(&file).unwrap();
            assert_eq!(read.doc, doc, "{doc}");
        }
    }

    /// `AppConfig::load()` 与 `appconfig_from_doc()` 是**同一段解析**：同一批 YAML 走两条路径，
    /// 结果逐值相等（这是"语义一致"的硬证明，不是"两处写得一样"）。
    #[test]
    fn load_and_appconfig_from_doc_agree_on_the_same_inputs() {
        let dir = TestDir::new("parity");
        let cases = [
            "",
            "{}\n",
            "colors:\n  preset: terminal\n  accent: cyan\n",
            "colors:\n  accent: neon_pink\n",
            "colors:\n  preset: NEON\n",
            "colors:\n  math_mode: off\n",
            "layout:\n  max_input_lines: 33\n  max_popup_rows: 4\n  tool_output_max: 2\n",
            "rendering:\n  thinking: hidden\n  math: KATEX\n  images: off\n",
            "api_key: \"sk-abc\"\n",
            "colors:\n  accent: \"#ff00ff\"\nrendering:\n  math: off\napi_key: sk-x\n",
        ];
        for (index, yaml) in cases.iter().enumerate() {
            let file = dir.path(&format!("case-{index}.yaml"));
            fs::write(&file, yaml).unwrap();
            let via_load = AppConfig::load_from_path(&file);
            let doc = read_interface_doc_from(&file).expect("parseable case").doc;
            let via_doc = appconfig_from_doc(&doc);
            assert_eq!(
                serde_json::to_value(&via_load).unwrap(),
                serde_json::to_value(&via_doc).unwrap(),
                "两条路径不一致：{yaml:?}"
            );
        }
    }

    /// 共享解析的语义：大小写不敏感 enum、非法值 warn + 回落、`resolve()` 折进载体字段。
    #[test]
    fn appconfig_from_doc_applies_the_documented_fallbacks() {
        let cfg = appconfig_from_doc(&json!({
            "colors": {"preset": "NEON", "accent": "neon_pink"},
            "rendering": {"math": "KATEX", "thinking": "deep"},
            "layout": {"max_input_lines": 33},
        }));
        assert_eq!(cfg.colors.preset, ColorPreset::Wing, "非法 preset 回落默认");
        assert_eq!(cfg.rendering.math, MathMode::Text, "非法 math 回落默认");
        assert_eq!(cfg.rendering.thinking, ThinkingMode::Visible);
        // 非法色值原样留在配置里，回落发生在调色板层（既有语义，没被改）
        assert_eq!(cfg.colors.accent.as_deref(), Some("neon_pink"));
        assert_eq!(
            ThemePalette::from_config(&cfg.colors).accent,
            ThemePalette::default().accent
        );
        assert_eq!(cfg.layout.max_input_lines, 33);
        assert_eq!(
            cfg.layout.max_popup_rows,
            LayoutConfig::default().max_popup_rows,
            "缺席键跟随默认"
        );
        // 大小写不敏感 + resolve()
        let cfg = appconfig_from_doc(
            &json!({"colors": {"preset": "Terminal"}, "rendering": {"math": "Off"}}),
        );
        assert_eq!(cfg.colors.preset, ColorPreset::Terminal);
        assert_eq!(
            cfg.colors.math_mode,
            MathMode::Off,
            "resolve() 折进了载体字段"
        );
    }
}
