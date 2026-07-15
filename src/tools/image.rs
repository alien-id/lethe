use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::json;

const MAX_IMAGE_BASE64_BYTES: usize = 5_000_000;

#[derive(Clone, Debug)]
pub struct ImageTools {
    workspace_dir: PathBuf,
    /// Hosted jail: when set, images may only be read from inside the
    /// workspace (same rules as the sandboxed file tools).
    sandboxed: bool,
}

impl ImageTools {
    pub fn new(workspace_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            sandboxed: false,
        }
    }

    pub fn sandboxed(workspace_dir: impl Into<PathBuf>) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            sandboxed: true,
        }
    }

    pub fn view_image(&self, file_path: &str, _max_size: usize) -> String {
        let path = match self.resolve_path(file_path) {
            Ok(path) => path,
            Err(error) => return error_json(&error),
        };
        if !path.exists() {
            return error_json(&format!("File not found: {file_path}"));
        }
        if !path.is_file() {
            return error_json(&format!("Not a file: {file_path}"));
        }

        let Some(mime_type) = image_mime_type(&path) else {
            return error_json(
                "Not an image or unsupported format. Use: jpg, jpeg, png, gif, webp",
            );
        };

        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => return error_json(&format!("Failed to read image: {error}")),
        };
        let encoded = BASE64_STANDARD.encode(bytes);
        if encoded.len() > MAX_IMAGE_BASE64_BYTES {
            return error_json(&format!(
                "Image too large: {}MB encoded (max 5MB)",
                encoded.len() / 1_000_000
            ));
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        json!({
            "status": "ok",
            "message": format!("Viewing image: {}", path.display()),
            "path": path.display().to_string(),
            "_image_view": {
                "path": path.display().to_string(),
                "mime_type": mime_type,
                "data": encoded,
                "name": name,
            }
        })
        .to_string()
    }

    fn resolve_path(&self, file_path: &str) -> Result<PathBuf, String> {
        if self.sandboxed {
            return crate::tools::filesystem::resolve_jailed_path(file_path, &self.workspace_dir);
        }
        let path = PathBuf::from(file_path);
        Ok(if path.is_absolute() {
            path
        } else {
            self.workspace_dir.join(path)
        })
    }
}

fn image_mime_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn error_json(message: &str) -> String {
    json!({
        "status": "error",
        "message": message,
    })
    .to_string()
}

use serde_json::Value;

use crate::tools::registry::ToolRegistry;
use crate::tools::registry::args::{string_arg, usize_arg};
use crate::tools::spec::{ToolCategory, ToolDef, ToolExecutor, p_int, p_str_req};

fn exec_view_image(registry: &ToolRegistry<'_>, args: &Value) -> String {
    registry.image.view_image(
        &string_arg(args, "file_path"),
        usize_arg(args, "max_size", 1568),
    )
}

pub const TOOL_DEFS: &[ToolDef] = &[ToolDef {
    name: "view_image",
    description: "Attach a local image to the next model turn.",
    params: &[
        p_str_req("file_path", "Image path."),
        p_int("max_size", "Max image dimension hint."),
    ],
    category: ToolCategory::Initial,
    execute: ToolExecutor::Sync(exec_view_image),
}];

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn view_image_returns_payload_for_supported_image() {
        let tmp = tempdir().unwrap();
        let image = tmp.path().join("image.png");
        std::fs::write(&image, b"not a real png, but enough bytes").unwrap();
        let tools = ImageTools::new(tmp.path());

        let payload: Value = serde_json::from_str(&tools.view_image("image.png", 1568)).unwrap();

        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["_image_view"]["mime_type"], "image/png");
        assert!(payload["_image_view"]["data"].as_str().unwrap().len() > 10);
    }

    #[test]
    fn view_image_rejects_unsupported_extension() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"hello").unwrap();
        let tools = ImageTools::new(tmp.path());

        let payload: Value = serde_json::from_str(&tools.view_image("notes.txt", 1568)).unwrap();

        assert_eq!(payload["status"], "error");
        assert!(payload["message"].as_str().unwrap().contains("unsupported"));
    }
}
