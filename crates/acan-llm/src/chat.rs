use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::CallRecord;

/// One message in a conversation. Serializes to the OpenAI chat-completions shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    /// A user turn made of parts, such as text with images. It is sent with role `user` and an
    /// array `content`; a text-only turn stays [`Message::User`], whose `content` is a string.
    #[serde(rename = "user")]
    UserParts {
        content: Vec<ContentPart>,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    /// A user turn with text and image parts, in the order given.
    pub fn user_parts(content: Vec<ContentPart>) -> Self {
        Self::UserParts { content }
    }
}

/// One part of a [`Message::UserParts`] turn, in the OpenAI chat-completions shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// A PNG image sent inline as a `data:` URL.
    pub fn png(bytes: &[u8]) -> Self {
        Self::ImageUrl {
            image_url: ImageUrl {
                url: format!("data:image/png;base64,{}", BASE64.encode(bytes)),
            },
        }
    }

    /// The text of a text part.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::ImageUrl { .. } => None,
        }
    }
}

/// Where an image part's image is: a `data:` URL or a link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: ToolKind,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    #[default]
    Function,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON text as the model wrote it. The caller parses it.
    pub arguments: String,
}

/// A tool the model may call. `parameters` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A chat-completions request for one role. The provider and model come from routing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Set by `chat_json`. Set it directly to ask `chat` for schema-shaped output.
    pub response_format: Option<JsonSchemaFormat>,
}

impl ChatRequest {
    /// The text of the last user message, if there is one. For a turn made of parts, that is
    /// its first text part.
    pub fn last_user_text(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User { content } => Some(Some(content.as_str())),
                Message::UserParts { content } => {
                    Some(content.iter().find_map(ContentPart::as_text))
                }
                _ => None,
            })
            .flatten()
    }
}

/// A named JSON Schema the reply must match.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonSchemaFormat {
    pub name: String,
    pub schema: Value,
}

impl JsonSchemaFormat {
    /// Builds the format for `T` from its schemars schema.
    pub fn for_type<T: JsonSchema>() -> Self {
        let mut schema = serde_json::to_value(schemars::schema_for!(T))
            .expect("schemars schemas serialize to JSON");
        if let Value::Object(map) = &mut schema {
            map.remove("$schema");
        }
        Self {
            name: openai_name(&T::schema_name()),
            schema,
        }
    }
}

/// OpenAI accepts only letters, digits, dashes, and underscores, up to 64 characters.
fn openai_name(raw: &str) -> String {
    let name: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect();
    if name.is_empty() {
        "output".to_owned()
    } else {
        name
    }
}

/// The reply to one chat call, with its accounting record.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub call: CallRecord,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_text_only_user_message_keeps_its_string_content() {
        let encoded =
            serde_json::to_string(&Message::user("Plan the login fix.")).expect("encodes");
        assert_eq!(
            encoded,
            r#"{"role":"user","content":"Plan the login fix."}"#
        );
    }

    #[test]
    fn a_user_turn_with_parts_sends_an_array_of_text_and_image_parts() {
        let message = Message::user_parts(vec![
            ContentPart::text("BASELINE"),
            ContentPart::png(b"png"),
        ]);
        assert_eq!(
            serde_json::to_value(&message).expect("encodes"),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "BASELINE"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}}
                ]
            })
        );
    }

    #[test]
    fn the_last_user_text_of_a_turn_with_parts_is_its_first_text_part() {
        let request = ChatRequest {
            messages: vec![
                Message::user("older"),
                Message::user_parts(vec![
                    ContentPart::png(b"png"),
                    ContentPart::text("compare these"),
                    ContentPart::text("CANDIDATE"),
                ]),
            ],
            ..ChatRequest::default()
        };
        assert_eq!(request.last_user_text(), Some("compare these"));
    }

    #[test]
    fn a_last_user_turn_with_only_images_has_no_text() {
        let request = ChatRequest {
            messages: vec![
                Message::user("older"),
                Message::user_parts(vec![ContentPart::png(b"png")]),
            ],
            ..ChatRequest::default()
        };
        assert_eq!(request.last_user_text(), None);
    }
}
