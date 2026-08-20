use serde::{Deserialize, Serialize};

/// Legacy on-disk message shape — `SessionHistory` wraps `Vec<Message>`.
/// Used by sessions persistence and the gRPC conversation save path.
/// In-flight conversation now flows through `crate::provider::ir::ApiMessage`
/// (neutral) and `provider/anthropic/wire.rs::AnthropicWireMessage` (wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    /// Images on this turn, by reference into the MEDIA store. Serde-
    /// additive (`None` = absent on the wire, so pre-media history docs
    /// serialize byte-identically; `CURRENT_SESSION_FORMAT` stays 2 — this
    /// is an optional field, not typed-block persistence). User turns:
    /// operator attachments, replayed inline to the model under the
    /// replay ceiling (`media::replay`). Assistant turns: images the
    /// turn's tools produced/viewed — display + transcript only (the APIs
    /// reject image blocks on assistant turns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<AttachmentRef>>,
}

/// Persisted pointer to one stored image. `bytes` is the stored
/// (normalized) size — the replay budget uses it without touching disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub path: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            attachments: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            attachments: None,
        }
    }

    pub fn user_with_attachments(content: impl Into<String>, refs: Vec<AttachmentRef>) -> Self {
        let mut m = Self::user(content);
        m.attachments = if refs.is_empty() { None } else { Some(refs) };
        m
    }

    pub fn assistant_with_attachments(content: impl Into<String>, refs: Vec<AttachmentRef>) -> Self {
        let mut m = Self::assistant(content);
        m.attachments = if refs.is_empty() { None } else { Some(refs) };
        m
    }

    pub fn attachment_refs(&self) -> &[AttachmentRef] {
        self.attachments.as_deref().unwrap_or(&[])
    }
}

#[cfg(test)]
mod attachment_serde_tests {
    use super::*;

    #[test]
    fn message_without_attachments_serializes_byte_identically() {
        // The pre-media on-disk shape: exactly two keys, no `attachments`.
        let m = Message::user("hi");
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"role":"user","content":"hi"}"#);
        let m = Message::user_with_attachments("hi", vec![]);
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"role":"user","content":"hi"}"#);
    }

    #[test]
    fn legacy_history_doc_without_attachments_deserializes() {
        let m: Message = serde_json::from_str(r#"{"role":"assistant","content":"ok"}"#).unwrap();
        assert!(m.attachments.is_none());
        assert!(m.attachment_refs().is_empty());
        let m: Message = serde_json::from_str(
            r#"{"role":"user","content":"","attachments":[{"id":"att-20260820T153012Z-1a2b3c4d","name":"a.png","media_type":"image/png","width":2,"height":2,"bytes":70,"path":"/embra/workspace/MEDIA/att-20260820T153012Z-1a2b3c4d.png"}]}"#,
        )
        .unwrap();
        assert_eq!(m.attachment_refs().len(), 1);
        assert_eq!(m.attachment_refs()[0].name, "a.png");
    }
}
