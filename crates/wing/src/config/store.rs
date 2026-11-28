//! TUI 配置文档的读写（`$WING_HOME/tui/config.yaml`）。
//!
//! Interface 根的全部 I/O 都在这里：稀疏文档读取 + 文件字节指纹 + 原子写（tmp + `sync_all` +
//! `rename`）+ 写前 `.bak`。三个语义约定：
//!
//! 1. **稀疏**：`doc` 只有用户显式写下的键（`{}` 表示空文件 / 文件不存在）。缺席 = 跟随默认，
//!    这正是 `dump_config_yaml` 把默认值写成注释的原因。
//! 2. **读失败不吞**：文件坏掉时返回 [`StoreError::Parse`]（原文一字不动），不静默回落到默认值
//!    ——调用方（设置面板）该展示问题并拒绝保存，否则一次保存就会覆盖掉用户手写的文件。
//! 3. **宽度补丁**：分隔线拖动通过同一原子写事务更新 YAML 的单个值，保留其余文本。
//! 4. **指纹**：文件字节的 FNV-1a 64（`fnv1a64:<16 hex>`），文件不存在 = `"absent"`
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
use yaml_edit::{YamlFile, path::YamlPath};

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

/// 写盘结论（设置面板用它更新本地指纹 / 写回执）。
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
    Conflict {
        path: PathBuf,
    },
    /// 读文件失败（非 NotFound）。
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// YAML 解析失败：原文原样保留在 `path`。
    Parse {
        path: PathBuf,
        message: String,
    },
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
            Self::Conflict { path } => write!(f, "配置正在被修改，请重试（{}）", path.display()),
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

/// `$WING_HOME/tui/config.yaml`（`WING_HOME` 覆盖 `~/.wing`，空串视同未设置，与后端
/// `get_wing_home()` 一致；取值按字节保留，见 [`crate::util::wing_home`]）；无法确定时 `None`。
pub fn interface_config_path() -> Option<PathBuf> {
    let home =
        crate::util::wing_home::env_override().or_else(crate::util::wing_home::home_default)?;
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
/// 设置面板的写盘内容 = [`dump_config_yaml`] 的 [`DumpMode::Raw`] 输出（`--dump-config` 与保存共用同一个
/// emitter，但模式相反：**保存永远写密文真值**，展示默认写掩码）。
pub fn write_interface_doc(doc: &Value) -> Result<WriteOutcome, StoreError> {
    let path = interface_config_path().ok_or(StoreError::NoConfigPath)?;
    write_interface_doc_to(&path, doc)
}

/// [`write_interface_doc`] 的显式路径内核（测试走这里）。
pub(crate) fn write_interface_doc_to(path: &Path, doc: &Value) -> Result<WriteOutcome, StoreError> {
    let _lock = lock_interface(path)?;
    let previous = read_interface_text(path)?;
    // The dump command may mask secrets. A save always uses their original values.
    let text = dump_config_yaml(doc, DumpMode::Raw);
    write_interface_text(path, &text, previous.as_deref())
}

/// Patch an integer on the latest Interface file, retaining the YAML source around it.
/// `edit` applies the same change to the semantic document for validation before commit.
pub(crate) fn update_interface_int(
    key: &str,
    value: u16,
    edit: impl FnOnce(&mut Value) -> Result<(), String>,
) -> Result<(), StoreError> {
    let path = interface_config_path().ok_or(StoreError::NoConfigPath)?;
    update_interface_int_at(&path, key, value, edit)
}

fn update_interface_int_at(
    path: &Path,
    key: &str,
    value: u16,
    edit: impl FnOnce(&mut Value) -> Result<(), String>,
) -> Result<(), StoreError> {
    let _lock = lock_interface(path)?;
    let previous = read_interface_text(path)?;
    let before = parse_document(previous.as_deref().unwrap_or_default(), path)?;
    let mut expected = before.clone();
    let parse_error = |message: String| StoreError::Parse {
        path: path.to_owned(),
        message,
    };
    edit(&mut expected).map_err(parse_error)?;
    if before == expected {
        return Ok(());
    }
    let text = if let Some(source) = &previous {
        let yaml: YamlFile = source
            .parse()
            .map_err(|error: yaml_edit::YamlError| parse_error(error.to_string()))?;
        yaml.ensure_document()
            .try_set_path(key, i64::from(value))
            .map_err(|error| parse_error(error.to_string()))?;
        yaml.to_string()
    } else {
        dump_config_yaml(&expected, DumpMode::Raw)
    };
    // The config reader interprets aliases and scalar types. Verify that the CST patch
    // did not change any value outside the requested path before replacing the file.
    if parse_document(&text, path)? != expected {
        return Err(parse_error("单键修改会改变其他配置值".into()));
    }
    if read_interface_text(path)? != previous {
        return Err(StoreError::Conflict {
            path: path.to_owned(),
        });
    }
    write_interface_text(path, &text, previous.as_deref())?;
    Ok(())
}

fn read_interface_text(path: &Path) -> Result<Option<String>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn lock_interface(path: &Path) -> Result<Option<fs::File>, StoreError> {
    let write_error = |source| StoreError::Write {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(write_error)?;
    }
    let name = path
        .file_name()
        .ok_or_else(|| write_error(std::io::Error::other("path has no file name")))?;
    // The config inode changes on every atomic replacement. Lock a stable sibling.
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_file_name(format!(".{}.lock", name.to_string_lossy())))
        .map_err(write_error)?;
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(fs::TryLockError::WouldBlock) => Err(StoreError::Conflict {
            path: path.to_owned(),
        }),
        Err(fs::TryLockError::Error(error)) if unsupported_lock(&error) => Ok(None),
        Err(fs::TryLockError::Error(error)) => Err(write_error(error)),
    }
}

fn unsupported_lock(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOLCK | libc::ENOSYS | libc::EINVAL)
    ) {
        return true;
    }
    error.kind() == std::io::ErrorKind::Unsupported
}

fn write_interface_text(
    path: &Path,
    text: &str,
    previous: Option<&str>,
) -> Result<WriteOutcome, StoreError> {
    let backup_path = if let Some(previous) = previous {
        let backup = backup_path(path)?;
        atomic_write(&backup, previous)?;
        Some(backup)
    } else {
        None
    };
    atomic_write(path, text)?;
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
    let value: serde_yaml_ng::Value = if text.trim().is_empty() {
        serde_yaml_ng::Value::Null
    } else {
        serde_yaml_ng::from_str(text).map_err(|e| parse_error(e.to_string()))?
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
fn yaml_to_json(value: serde_yaml_ng::Value) -> Result<Value, String> {
    Ok(match value {
        serde_yaml_ng::Value::Null => Value::Null,
        serde_yaml_ng::Value::Bool(b) => Value::Bool(b),
        serde_yaml_ng::Value::Number(n) => {
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
        serde_yaml_ng::Value::String(s) => Value::String(s),
        serde_yaml_ng::Value::Sequence(items) => Value::Array(
            items
                .into_iter()
                .map(yaml_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        serde_yaml_ng::Value::Mapping(mapping) => {
            let mut object = serde_json::Map::with_capacity(mapping.len());
            for (key, value) in mapping {
                let serde_yaml_ng::Value::String(key) = key else {
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
        serde_yaml_ng::Value::Tagged(tagged) => yaml_to_json(tagged.value)?,
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

fn key_kind(value: &serde_yaml_ng::Value) -> &'static str {
    match value {
        serde_yaml_ng::Value::Null => "null",
        serde_yaml_ng::Value::Bool(_) => "a boolean",
        serde_yaml_ng::Value::Number(_) => "a number",
        serde_yaml_ng::Value::String(_) => "a string",
        serde_yaml_ng::Value::Sequence(_) => "a sequence",
        serde_yaml_ng::Value::Mapping(_) => "a mapping",
        serde_yaml_ng::Value::Tagged(_) => "a tagged value",
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

        // 目录保留目标、备份和写锁。
        let mut entries: Vec<String> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            [".config.yaml.lock", "config.yaml", "config.yaml.bak"]
        );
    }

    fn save_sidebar_width_to(path: &Path, width: u16) -> Result<(), StoreError> {
        let key = "layout.session_sidebar.width";
        let steps = wing_api_client::models::parse_path(key).unwrap();
        update_interface_int_at(path, key, width, |doc| {
            crate::shared::doc_edit::set_path(
                doc,
                &steps,
                json!(width),
                crate::shared::doc_edit::Policy::Strict,
            )
        })
    }

    #[test]
    fn sidebar_width_updates_preserve_existing_and_future_settings() {
        let dir = TestDir::new("sidebar-width");
        let file = dir.path("config.yaml");
        save_sidebar_width_to(&file, 28).unwrap();
        assert_eq!(
            AppConfig::load_from_path(&file)
                .layout
                .session_sidebar
                .width,
            28
        );
        assert!(!backup_path(&file).unwrap().exists());

        let text = "api_key: sample-secret\nlayout:\n  tool_output_max: 7\n  session_sidebar:\n    side: right\n    width: 33\n    future_knob: 5\nunknown: { enabled: true }\n";
        fs::write(&file, text).unwrap();
        save_sidebar_width_to(&file, 20).unwrap();
        assert_eq!(
            read_interface_doc_from(&file).unwrap().doc,
            json!({
                "api_key": "sample-secret",
                "layout": {"tool_output_max": 7, "session_sidebar": {"side": "right", "width": 20, "future_knob": 5}},
                "unknown": {"enabled": true},
            })
        );
        assert_eq!(read_text(&backup_path(&file).unwrap()), text);
        let saved = read_interface_doc_from(&file).unwrap().fingerprint;
        save_sidebar_width_to(&file, 20).unwrap();
        assert_eq!(saved, read_interface_doc_from(&file).unwrap().fingerprint);
        assert_eq!(read_text(&backup_path(&file).unwrap()), text);
    }

    #[test]
    fn updating_the_width_keeps_yaml_comments_and_layout() {
        let dir = TestDir::new("width-comments");
        let file = dir.path("config.yaml");
        let notes = "# My TUI notes\nlayout: # section\n  # An optional tweak\n  session_sidebar:\n    side: left # My preference\n    width: 24 # Width in columns\n\nother: {wire: true} # Future setting\n";
        fs::write(&file, notes).unwrap();
        save_sidebar_width_to(&file, 32).unwrap();
        assert_eq!(read_text(&file), notes.replace("width: 24", "width: 32"));
        save_sidebar_width_to(&file, 21).unwrap();
        assert_eq!(read_text(&file), notes.replace("width: 24", "width: 21"));
    }

    #[test]
    fn a_width_update_retains_the_comment_only_default_template() {
        let dir = TestDir::new("width-default-template");
        let file = dir.path("config.yaml");
        let template = dump_config_yaml(&json!({}), DumpMode::Raw);
        fs::write(&file, &template).unwrap();
        save_sidebar_width_to(&file, 31).unwrap();
        let result = read_text(&file);
        assert!(result.starts_with(&template), "{result}");
        assert_eq!(
            AppConfig::load_from_path(&file)
                .layout
                .session_sidebar
                .width,
            31
        );
    }

    #[test]
    fn a_config_edit_after_the_width_read_survives_the_conflict() {
        let dir = TestDir::new("width-concurrent-edit");
        let file = dir.path("config.yaml");
        let changed = "colors: {preset: light}\n";
        fs::write(&file, "{}\n").unwrap();
        let key = "layout.session_sidebar.width";
        let steps = wing_api_client::models::parse_path(key).unwrap();
        let result = update_interface_int_at(&file, key, 29, |doc| {
            fs::write(&file, changed).unwrap();
            crate::shared::doc_edit::set_path(
                doc,
                &steps,
                json!(29),
                crate::shared::doc_edit::Policy::Strict,
            )
        });
        assert!(matches!(result, Err(StoreError::Conflict { .. })));
        assert_eq!(read_text(&file), changed);
        assert!(!backup_path(&file).unwrap().exists());
    }

    #[test]
    fn sidebar_width_preserves_invalid_or_locked_files() {
        let dir = TestDir::new("sidebar-invalid");
        let file = dir.path("config.yaml");
        for text in [
            "layout: [",
            "layout: false",
            "layout: {session_sidebar: []}",
        ] {
            fs::write(&file, text).unwrap();
            assert!(matches!(
                save_sidebar_width_to(&file, 28),
                Err(StoreError::Parse { .. })
            ));
            assert_eq!(read_text(&file), text);
        }
        fs::write(&file, "layout: {}\n").unwrap();
        let lock = lock_interface(&file).unwrap();
        assert!(matches!(
            save_sidebar_width_to(&file, 28),
            Err(StoreError::Conflict { .. })
        ));
        assert!(matches!(
            write_interface_doc_to(&file, &json!({})),
            Err(StoreError::Conflict { .. })
        ));
        assert_eq!(read_text(&file), "layout: {}\n");
        drop(lock);
        save_sidebar_width_to(&file, 28).unwrap();
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

        // 第二次保存：备份里也是真值（掩码不许引进备份）。
        let outcome = write_interface_doc_to(&file, &json!({"api_key": "sk-next-9999"})).unwrap();
        let backup = read_text(&outcome.backup_path.expect("第二次写入备份旧文件"));
        assert!(backup.contains(SAVE_SECRET), "备份要写真值：\n{backup}");
        assert!(!backup.contains('•'), "备份里不许有掩码：\n{backup}");
    }

    /// `.bak` 是改前文件的**逐字节**副本，且写它是原子写（读字节 → tmp + rename），
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

    /// 目标不存在时**不产生** `.bak`（今天的行为）。
    #[test]
    fn no_backup_is_created_when_the_target_does_not_exist() {
        let dir = TestDir::new("no-backup");
        let file = dir.path("config.yaml");
        let outcome = write_interface_doc_to(&file, &json!({"api_key": SAVE_SECRET})).unwrap();
        assert!(outcome.backup_path.is_none(), "首次写入没有可备份的旧文件");
        assert!(!dir.path("config.yaml.bak").exists());
        let mut entries: Vec<String> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, [".config.yaml.lock", "config.yaml"]);
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
        assert_eq!(cfg.colors.preset, ColorPreset::Auto, "非法 preset 回落默认");
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
            &json!({"colors": {"preset": "Dark"}, "rendering": {"math": "Off"}}),
        );
        assert_eq!(cfg.colors.preset, ColorPreset::Dark);
        assert_eq!(
            cfg.colors.math_mode,
            MathMode::Off,
            "resolve() 折进了载体字段"
        );
    }
}
