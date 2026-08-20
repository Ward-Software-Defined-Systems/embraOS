//! Session history → neutral IR, with the inline-image ceiling.
//!
//! Policy (William, 2026-08-20): operator attachments replay INLINE as
//! base64 on every request — no third-party file store, no horizon. The
//! ceiling is the safety rail on the vision API's 32 MB request cap, not
//! a policy of its own: newest-first, at most [`MEDIA_HISTORY_MAX_IMAGES`]
//! images / [`MEDIA_HISTORY_MAX_BYTES`] raw bytes stay inline; anything
//! past it (or whose file is gone) degrades to a text placeholder that
//! names the path, so the model can `image_view` it on demand. Aging is
//! monotonic by turn distance, so each image crosses the boundary once —
//! one prompt-cache miss per aging event, never per turn.
//!
//! Assistant turns never get image blocks (the APIs reject them); their
//! refs are display/transcript state only.

use crate::brain::{AttachmentRef, Message};
use crate::provider::ir::{ApiMessage, Block, ImageData};

use super::store::{to_image_data, MediaStore};
use super::{MEDIA_HISTORY_MAX_BYTES, MEDIA_HISTORY_MAX_IMAGES};

/// Text a user turn gets when the operator sent images with no words.
/// Keeps every provider on the non-empty-text path (cache breakpoints,
/// enrichment gate, parts arrays all assume a text block exists).
pub const IMAGE_ONLY_PLACEHOLDER: &str = "(see attached image)";

/// Text-only history conversion — the pre-media shape, kept as the fast
/// path so sessions without attachments are untouched.
pub fn legacy_message_to_api(m: &Message) -> ApiMessage {
    let block = Block::Text(m.content.clone());
    match m.role.as_str() {
        "user" => ApiMessage::User { content: vec![block] },
        _ => ApiMessage::Assistant { content: vec![block] },
    }
}

pub fn not_inlined_placeholder(r: &AttachmentRef) -> String {
    format!(
        "[image: {} ({}×{}) — not inlined; path {}; use image_view to see it]",
        r.name, r.width, r.height, r.path
    )
}

/// Convert persisted history to IR, loading inline images from the store
/// under the ceiling.
pub async fn history_to_api(store: &MediaStore, history: &[Message]) -> Vec<ApiMessage> {
    if history.iter().all(|m| m.attachments.is_none()) {
        return history.iter().map(legacy_message_to_api).collect();
    }
    // Budget pass: newest user turn first, refs in turn order.
    let mut inline: Vec<Vec<bool>> = history
        .iter()
        .map(|m: &Message| vec![false; m.attachment_refs().len()])
        .collect();
    let mut count = 0usize;
    let mut bytes = 0u64;
    for (ti, m) in history.iter().enumerate().rev() {
        if m.role != "user" {
            continue;
        }
        for (ri, r) in m.attachment_refs().iter().enumerate() {
            if count < MEDIA_HISTORY_MAX_IMAGES && bytes + r.bytes <= MEDIA_HISTORY_MAX_BYTES {
                inline[ti][ri] = true;
                count += 1;
                bytes += r.bytes;
            }
        }
    }

    let mut out = Vec::with_capacity(history.len());
    for (ti, m) in history.iter().enumerate() {
        if m.role != "user" || m.attachments.is_none() {
            out.push(legacy_message_to_api(m));
            continue;
        }
        let mut images: Vec<ImageData> = Vec::new();
        let mut placeholders: Vec<String> = Vec::new();
        for (ri, r) in m.attachment_refs().iter().enumerate() {
            if inline[ti][ri] {
                match store.get(&r.id).await {
                    Ok((meta, data)) => images.push(to_image_data(&meta, &data)),
                    Err(e) => {
                        tracing::warn!(target: "media", id = %r.id, error = %e, "history image not loadable; placeholder");
                        placeholders.push(not_inlined_placeholder(r));
                    }
                }
            } else {
                placeholders.push(not_inlined_placeholder(r));
            }
        }
        let mut text = if m.content.trim().is_empty() {
            IMAGE_ONLY_PLACEHOLDER.to_string()
        } else {
            m.content.clone()
        };
        for p in placeholders {
            text.push('\n');
            text.push_str(&p);
        }
        out.push(if images.is_empty() {
            ApiMessage::user_text(text)
        } else {
            ApiMessage::user_with_images(images, text)
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ingest::{self, tests::png_fixture};
    use crate::media::store::{to_attachment_ref, MediaOrigin};

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("embra-replay-{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn stored(store: &MediaStore, w: u32) -> AttachmentRef {
        let n = ingest::normalize_blocking(png_fixture(w, 2)).unwrap();
        let meta = store.put(MediaOrigin::Attached, &format!("img{w}.png"), "s", n).await.unwrap();
        to_attachment_ref(&meta, store.dir())
    }

    fn count_images(msg: &ApiMessage) -> usize {
        msg.content().iter().filter(|b| matches!(b, Block::Image(_))).count()
    }

    #[tokio::test]
    async fn history_to_api_text_only_is_identical_to_legacy() {
        let dir = TempDir::new();
        let store = MediaStore::at(&dir.0);
        let history = vec![Message::user("hi"), Message::assistant("hello")];
        let out = history_to_api(&store, &history).await;
        assert_eq!(out.len(), 2);
        assert!(matches!(&out[0], ApiMessage::User { content } if matches!(&content[0], Block::Text(t) if t == "hi")));
        assert!(matches!(&out[1], ApiMessage::Assistant { content } if matches!(&content[0], Block::Text(t) if t == "hello")));
    }

    #[tokio::test]
    async fn history_to_api_attaches_user_images_and_never_assistant_images() {
        let dir = TempDir::new();
        let store = MediaStore::at(&dir.0);
        let a = stored(&store, 3).await;
        let g = stored(&store, 4).await;
        let history = vec![
            Message::user_with_attachments("what is this", vec![a.clone()]),
            Message::assistant_with_attachments("a picture; I generated one too", vec![g]),
            Message::user(""),
        ];
        let out = history_to_api(&store, &history).await;
        assert_eq!(count_images(&out[0]), 1);
        assert!(matches!(&out[0].content()[0], Block::Image(img) if img.name == "img3.png"));
        assert!(matches!(&out[0].content()[1], Block::Text(t) if t == "what is this"));
        assert_eq!(count_images(&out[1]), 0, "assistant refs are display-only");
        assert_eq!(out[1].content().len(), 1);
    }

    #[tokio::test]
    async fn history_to_api_image_only_turn_gets_placeholder_text() {
        let dir = TempDir::new();
        let store = MediaStore::at(&dir.0);
        let a = stored(&store, 3).await;
        let out = history_to_api(&store, &[Message::user_with_attachments("", vec![a])]).await;
        assert_eq!(count_images(&out[0]), 1);
        assert!(matches!(out[0].content().last(), Some(Block::Text(t)) if t == IMAGE_ONLY_PLACEHOLDER));
    }

    #[tokio::test]
    async fn history_to_api_missing_file_becomes_placeholder() {
        let dir = TempDir::new();
        let store = MediaStore::at(&dir.0);
        let mut a = stored(&store, 3).await;
        std::fs::remove_file(&a.path).unwrap();
        a.name = "gone.png".into();
        let out = history_to_api(&store, &[Message::user_with_attachments("look", vec![a])]).await;
        assert_eq!(count_images(&out[0]), 0);
        let Block::Text(t) = &out[0].content()[0] else { panic!() };
        assert!(t.starts_with("look\n[image: gone.png (3×2) — not inlined; path "), "{t}");
        assert!(t.ends_with("use image_view to see it]"));
    }

    #[tokio::test]
    async fn history_to_api_over_budget_becomes_placeholder_newest_first() {
        let dir = TempDir::new();
        let store = MediaStore::at(&dir.0);
        // Build MAX+2 single-image user turns; the two OLDEST must age out.
        let mut history = Vec::new();
        for i in 0..(MEDIA_HISTORY_MAX_IMAGES + 2) {
            let r = stored(&store, 3 + i as u32).await;
            history.push(Message::user_with_attachments(format!("turn {i}"), vec![r]));
            history.push(Message::assistant("ok"));
        }
        let out = history_to_api(&store, &history).await;
        let user_msgs: Vec<&ApiMessage> = out.iter().step_by(2).collect();
        assert_eq!(count_images(user_msgs[0]), 0, "oldest aged out");
        assert_eq!(count_images(user_msgs[1]), 0, "second oldest aged out");
        for m in &user_msgs[2..] {
            assert_eq!(count_images(m), 1);
        }
        assert!(matches!(user_msgs[0].content().last(), Some(Block::Text(t)) if t.contains("not inlined")));
        // Byte ceiling: a ref declaring a huge size is never inlined.
        let mut big = stored(&store, 3).await;
        big.bytes = MEDIA_HISTORY_MAX_BYTES + 1;
        let out = history_to_api(&store, &[Message::user_with_attachments("x", vec![big])]).await;
        assert_eq!(count_images(&out[0]), 0);
    }
}
