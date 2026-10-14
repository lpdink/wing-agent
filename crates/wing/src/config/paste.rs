//! 粘贴（paste）配置：composer 里贴进来的内容在发出前怎么处理。

use serde::Deserialize;
use serde::Serialize;

use crate::util::image_compress::CompressOptions;

/// `paste` 配置段。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PasteConfig {
    pub image: PasteImageConfig,
}

/// 粘贴图片的压缩参数（`paste.image.*`）。
///
/// 默认值 = 契约值：1 MiB / 质量 85 / 长边 2048。字段用宽整数（`u32` / `u64`）而不是
/// `u8` / `usize`：越界值（例如 `quality: 300`）在这里**不**让整份配置回落默认
/// （`AppConfig` 的类型错误是整体降级），而是在 [`Self::compress_options`] 里夹紧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PasteImageConfig {
    /// 发送前每张图的字节上限（JPEG 产物）。
    pub max_bytes: u64,
    /// JPEG 质量（1..=100；越界夹紧）。
    pub quality: u32,
    /// 长边像素上限（超出则等比缩小；越界夹紧到 ≥ 1）。
    pub max_edge: u32,
}

impl Default for PasteImageConfig {
    fn default() -> Self {
        Self {
            max_bytes: 1_048_576,
            quality: 85,
            max_edge: 2048,
        }
    }
}

impl PasteImageConfig {
    /// 压缩器参数——越界值在这里夹紧，压缩循环只处理合法域。
    pub fn compress_options(&self) -> CompressOptions {
        CompressOptions {
            max_bytes: self.max_bytes.max(1) as usize,
            quality: self.quality.clamp(1, 100) as u8,
            max_edge: self.max_edge.max(1),
        }
    }
}

impl From<&PasteImageConfig> for CompressOptions {
    fn from(config: &PasteImageConfig) -> Self {
        config.compress_options()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_defaults_are_the_declared_ones() {
        let image = PasteImageConfig::default();
        assert_eq!(image.max_bytes, 1_048_576);
        assert_eq!(image.quality, 85);
        assert_eq!(image.max_edge, 2048);
        assert_eq!(PasteConfig::default().image, image);
    }

    #[test]
    fn the_section_parses_from_yaml_and_keeps_absent_keys_at_default() {
        let config: PasteConfig =
            serde_yaml::from_str("image:\n  max_bytes: 2097152\n  quality: 70\n  max_edge: 1600\n")
                .unwrap();
        assert_eq!(config.image.max_bytes, 2_097_152);
        assert_eq!(config.image.quality, 70);
        assert_eq!(config.image.max_edge, 1600);

        // 缺席的键各自回落默认（稀疏配置是常态）。
        let partial: PasteConfig = serde_yaml::from_str("image:\n  quality: 60\n").unwrap();
        assert_eq!(partial.image.quality, 60);
        assert_eq!(partial.image.max_bytes, 1_048_576);
        assert_eq!(partial.image.max_edge, 2048);

        // 整个段缺席（旧配置文件）。
        let absent: PasteConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(absent.image, PasteImageConfig::default());
    }

    #[test]
    fn out_of_range_values_are_clamped_for_the_compressor_only() {
        let config: PasteImageConfig =
            serde_yaml::from_str("max_bytes: 0\nquality: 300\nmax_edge: 0\n").unwrap();
        // 解析保留用户写的值（面板回读 / 保存不会"吃掉"它）……
        assert_eq!(config.max_bytes, 0);
        assert_eq!(config.quality, 300);
        assert_eq!(config.max_edge, 0);
        // ……压缩器拿到的是夹紧后的合法域。
        let opts: CompressOptions = (&config).into();
        assert_eq!(opts.max_bytes, 1);
        assert_eq!(opts.quality, 100);
        assert_eq!(opts.max_edge, 1);

        let zero_quality: PasteImageConfig = serde_yaml::from_str("quality: 0\n").unwrap();
        assert_eq!(zero_quality.compress_options().quality, 1);
    }

    /// `--dump-config` 的往返：这一段写出去要能原样读回来（variant 名 / 数字都对得上）。
    #[test]
    fn the_section_round_trips_through_its_own_dump() {
        let config = PasteConfig {
            image: PasteImageConfig {
                max_bytes: 512_000,
                quality: 75,
                max_edge: 1024,
            },
        };
        let dumped = serde_yaml::to_string(&config).expect("dump");
        let parsed: PasteConfig = serde_yaml::from_str(&dumped).expect("parse back");
        assert_eq!(parsed.image, config.image);
        assert!(!dumped.contains("null"), "稀疏 dump 不带空值：{dumped}");
    }
}
