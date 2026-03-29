use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::types::StreamEvent;

pub async fn process_sse_stream(
    response: reqwest::Response,
    tx: mpsc::Sender<StreamEvent>,
) -> Result<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut full_text = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        // Process complete SSE lines
        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
            buffer = buffer[newline_pos + 1..].to_string();

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    let _ = tx.send(StreamEvent::Done(full_text.clone())).await;
                    return Ok(());
                }

                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    let event_type = event
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");

                    match event_type {
                        "content_block_delta" => {
                            if let Some(delta) = event.get("delta") {
                                let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                match delta_type {
                                    "text_delta" => {
                                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                            full_text.push_str(text);
                                            let _ = tx.send(StreamEvent::Token(text.to_string())).await;
                                        }
                                    }
                                    "thinking_delta" | "signature_delta" => {
                                        // Adaptive thinking internals — skip silently
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "message_stop" => {
                            let _ = tx.send(StreamEvent::Done(full_text.clone())).await;
                            return Ok(());
                        }
                        "error" => {
                            let msg = event
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                                .unwrap_or("Unknown stream error");
                            let _ = tx.send(StreamEvent::Error(msg.to_string())).await;
                            return Ok(());
                        }
                        _ => {} // ping, message_start, content_block_start, etc.
                    }
                }
            }
        }
    }

    // Stream ended without message_stop
    if !full_text.is_empty() {
        let _ = tx.send(StreamEvent::Done(full_text)).await;
    }

    Ok(())
}
