//! `file_offer` — hand the operator a workspace file as a browser download.
//!
//! The tool resolves the path through the download jail
//! (`media::offer`) and returns one line of text for the model plus a
//! display-only `FileRefMeta` the loop turns into a `FileRef` frame: the
//! web console renders a card with a download link, the terminal a file
//! line. Nothing of the file reaches the model.

use std::path::Path;

use embra_tool_macro::embra_tool;
use embra_tools_core::{DispatchError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::engineering::WORKSPACE_ROOT;
use super::registry::DispatchContext;
use crate::media::offer::{self, FILE_DOWNLOAD_MAX};

/// Longest caption shown on the card.
const NOTE_MAX: usize = 200;

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_offer",
    description = "Offer a workspace file to the operator as a browser download: a report you wrote, a dump, an export — any regular file under /embra/workspace up to 12 MiB (absolute or workspace-relative; a symlink must still resolve inside the workspace). The operator gets a file card with a download link in the web console and a file line in the terminal; nothing is sent to you. Write the file first (file_write) when it does not exist yet. note is an optional one-line caption shown on the card."
)]
#[serde(deny_unknown_fields)]
pub struct FileOfferArgs {
    /// The file to offer: absolute under /embra/workspace, or relative to it.
    pub path: String,
    /// Optional one-line caption shown with the download link.
    #[serde(default)]
    pub note: Option<String>,
}

impl FileOfferArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<ToolOutput, DispatchError> {
        Ok(file_offer_in(Path::new(WORKSPACE_ROOT), &self.path, self.note.as_deref().unwrap_or("")).await)
    }
}

/// Root-parameterized core (unit-tested under a temp root). A refusal is
/// `Ok` text, per house style; the resolver's words say why.
pub(crate) async fn file_offer_in(root: &Path, path: &str, note: &str) -> ToolOutput {
    match offer::resolve_offer_in(root, path, FILE_DOWNLOAD_MAX).await {
        Ok(f) => {
            let caption = super::sessions::truncate_str(note.trim(), NOTE_MAX).to_string();
            let text = format!(
                "Offered {} ({} KB) for download: {}",
                f.name,
                f.bytes / 1024,
                f.shown.display()
            );
            ToolOutput::text(text).with_file(offer::to_ref_meta(&f, &caption))
        }
        Err(e) => ToolOutput::text(format!("file_offer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "embra-file-offer-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn file_offer_names_the_file_and_carries_one_card() {
        let tmp = TempDir::new();
        let root = tmp.0.join("workspace");
        std::fs::create_dir_all(root.join("reports")).unwrap();
        std::fs::write(root.join("reports/weekly.md"), "x".repeat(3000)).unwrap();
        let out = file_offer_in(&root, "reports/weekly.md", "  the weekly report  ").await;
        assert_eq!(
            out.text,
            format!("Offered weekly.md (2 KB) for download: {}", root.join("reports/weekly.md").display())
        );
        assert_eq!(out.files.len(), 1);
        let f = &out.files[0];
        assert_eq!(f.name, "weekly.md");
        assert_eq!(f.byte_size, 3000);
        assert_eq!(f.media_type, "text/markdown");
        assert_eq!(f.caption, "the weekly report");
        assert_eq!(f.path, root.join("reports/weekly.md").display().to_string());
        assert!(out.images.is_empty());
    }

    #[tokio::test]
    async fn a_refused_offer_is_text_with_the_reason_and_no_card() {
        let tmp = TempDir::new();
        let root = tmp.0.join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let out = file_offer_in(&root, "../etc/passwd", "").await;
        assert!(out.text.starts_with("file_offer: Denied: "), "{}", out.text);
        assert!(out.files.is_empty());
        let out = file_offer_in(&root, "missing.md", "").await;
        assert!(out.text.ends_with(": not found"), "{}", out.text);
        assert!(out.files.is_empty());
    }

    #[test]
    fn file_offer_schema_is_plain_and_typed() {
        let schema = serde_json::to_value(schemars::schema_for!(FileOfferArgs)).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        assert_eq!(schema["additionalProperties"], false);
    }
}
