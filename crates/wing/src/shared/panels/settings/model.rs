//! Presentation of the Gateway model union over the original sparse document.

use std::collections::HashSet;

use serde_json::{Value, json};
use wing_api_client::models::{SettingKind, SettingNode};

use super::{Root, SettingsDoc};
use crate::shared::doc_edit;

const MODEL_LIST: &str = "providers[].models";
const IMAGE_INPUT: &str = "providers[].models[].capabilities.vision";
const VISION_FIELD: &str = "capabilities.vision";

pub(super) fn object_variant(root: Root, list: &SettingNode) -> Option<usize> {
    if root != Root::Gateway || list.path != MODEL_LIST {
        return None;
    }
    let variants = list.variants.as_deref()?;
    if !variants.iter().any(|node| node.kind == SettingKind::Str) {
        return None;
    }
    variants.iter().position(|node| {
        node.kind == SettingKind::Object
            && node
                .children
                .iter()
                .any(|field| field.key == "name" && field.kind == SettingKind::Str)
            && node.children.iter().any(|field| {
                field.key == "capabilities"
                    && field.children.iter().any(|capability| {
                        capability.key == "vision" && capability.kind == SettingKind::Bool
                    })
            })
    })
}

pub(super) fn field_label(node: &SettingNode) -> &str {
    match node.path.as_str() {
        IMAGE_INPUT => "Image input",
        "providers[].models[].name" => "Model name",
        _ => node.display_label(),
    }
}

/// The virtual name borrows the short form directly. Other fields use the catalog defaults.
pub(super) fn field_value<'a>(doc: &'a SettingsDoc, root: Root, path: &str) -> Option<&'a Value> {
    doc.value(root, path).or_else(|| {
        let (item, field) = model_field(root, path)?;
        (field == "name")
            .then(|| doc.value(root, item).filter(|value| value.is_string()))
            .flatten()
    })
}

fn model_field(root: Root, path: &str) -> Option<(&str, &str)> {
    if root != Root::Gateway {
        return None;
    }
    let (provider, model) = path.strip_prefix("providers[")?.split_once("].models[")?;
    let (index, field) = model.split_once("].")?;
    provider.parse::<usize>().ok()?;
    index.parse::<usize>().ok()?;
    Some((&path[..path.len() - field.len() - 1], field))
}

/// Remember which objects came from a short form in this draft. Index changes follow list edits.
#[derive(Default)]
pub(super) struct ModelEdits {
    promoted: HashSet<String>,
}

impl ModelEdits {
    pub(super) fn clear(&mut self) {
        self.promoted.clear();
    }

    pub(super) fn remap(&mut self, root: Root, list: &str, map: &dyn Fn(usize) -> Option<usize>) {
        if root == Root::Gateway {
            self.promoted = self
                .promoted
                .iter()
                .filter_map(|path| doc_edit::remap_index(path, list, map))
                .collect();
        }
    }

    pub(super) fn write(
        &mut self,
        doc: &mut SettingsDoc,
        root: Root,
        path: &str,
        value: &Value,
    ) -> bool {
        let Some((item, field)) = model_field(root, path) else {
            return false;
        };
        if let Some(name) = doc.value(root, item).and_then(Value::as_str) {
            if field == "name" {
                doc.set_value(root, item, value.clone());
                doc.mark_dirty(root, path);
                return true;
            }
            doc.set_value(root, item, json!({"name": name}));
            self.promoted.insert(item.to_string());
        }
        if self.promoted.contains(item) && field == VISION_FIELD && value == &Value::Bool(false) {
            return self.reset(doc, root, path);
        }
        false
    }

    pub(super) fn reset(&mut self, doc: &mut SettingsDoc, root: Root, path: &str) -> bool {
        let Some((item, field)) = model_field(root, path) else {
            return false;
        };
        if doc.value(root, item).is_some_and(Value::is_string) {
            return true;
        }
        if field == "name" || !self.promoted.contains(item) {
            return false;
        }
        doc.remove_value(root, path);
        doc.clear_dirty(root, path);
        if let Some(name) = doc
            .value(root, item)
            .and_then(Value::as_object)
            .filter(|object| object.len() == 1)
            .and_then(|object| object.get("name"))
            .filter(|name| name.is_string())
            .cloned()
        {
            doc.set_value(root, item, name);
            self.promoted.remove(item);
        }
        true
    }
}
