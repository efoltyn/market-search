use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    OpenRouter,
    OpenAI,
    Anthropic,
    Ollama,
    Mock,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::OpenAI => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Mock => "mock",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProviderKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "openrouter" => Ok(ProviderKind::OpenRouter),
            "openai" => Ok(ProviderKind::OpenAI),
            "anthropic" => Ok(ProviderKind::Anthropic),
            "ollama" => Ok(ProviderKind::Ollama),
            "mock" => Ok(ProviderKind::Mock),
            other => Err(format!("unknown provider '{other}'")),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Enforce Eli's tool-contract JSON schema (supported by some providers, e.g. OpenRouter).
    EliContractJsonSchema,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            images: Vec::new(),
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: Vec::new(),
            name: None,
        }
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images,
            name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            name: None,
        }
    }

    pub fn tool(content: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            images: Vec::new(),
            name: Some(name.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Optional provider-specific response format enforcement (e.g. JSON schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,

    #[serde(default)]
    pub stream: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Clone, Debug)]
pub enum ChatStreamEvent {
    Delta(String),
    Usage(Usage),
    Done,
}
