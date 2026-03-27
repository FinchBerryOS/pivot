use serde::Deserialize;
use std::fs;

use crate::fatal_error;
use crate::storage::parse_partuuid_to_bytes;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum BootMode {
    Installed,
    Live,
}

#[derive(Deserialize, Debug)]
pub enum ActiveSlot {
    #[serde(alias = "a")]
    A,
    #[serde(alias = "b")]
    B,
}

#[derive(Deserialize, Debug)]
pub struct PivotConfig {
    pub system: SystemConfig,
    pub hardware: HardwareConfig,
    pub images: ImagesConfig,
}

#[derive(Deserialize, Debug)]
pub struct SystemConfig {
    pub mode: BootMode,
    pub active_slot: ActiveSlot,
}

#[derive(Deserialize, Debug)]
pub struct HardwareConfig {
    pub boot_partition_uuid: String,
    pub system_partition_uuid: String,
}

#[derive(Deserialize, Debug)]
pub struct ImagesConfig {
    pub slot_a: String,
    pub slot_b: String,
}

impl ImagesConfig {
    pub fn all_slots(&self) -> [(&'static str, &String); 2] {
        [("slot_a", &self.slot_a), ("slot_b", &self.slot_b)]
    }
}

pub fn validate_config(cfg: &PivotConfig) {
    if crate::core::is_container() {
        klog!("Container mode: skipping hardware PARTUUID validation");
    } else {
        for (name, uuid) in &[
            ("boot_partition_uuid", &cfg.hardware.boot_partition_uuid),
            ("system_partition_uuid", &cfg.hardware.system_partition_uuid),
        ] {
            if parse_partuuid_to_bytes(uuid).is_none() {
                fatal_error(&format!(
                    "config: {} '{}' is not a valid GPT UUID (expected xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)",
                    name, uuid
                ));
            }
        }
    }

    validate_images(&cfg.images);
}

pub fn validate_images(images: &ImagesConfig) {
    for (name, img) in &images.all_slots() {
        if img.is_empty() || img.contains('/') || img.contains("..") {
            fatal_error(&format!(
                "config: images.{} '{}' is empty or contains illegal path characters",
                name, img
            ));
        }
    }
}

pub fn read_config(path: &str) -> PivotConfig {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|e| fatal_error(&format!("Cannot read {}: {}", path, e)));
    toml::from_str(&content)
        .unwrap_or_else(|e| fatal_error(&format!("pivot.config parse error: {}", e)))
}