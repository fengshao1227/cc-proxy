/// Which upstream API protocol the server detected and will use for all requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamApiMode {
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
    /// OpenAI Chat Completions API (`/v1/chat/completions`).
    ChatCompletions,
}

impl UpstreamApiMode {
    /// Returns a short lowercase string identifier for logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
        }
    }
}

impl std::fmt::Display for UpstreamApiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
