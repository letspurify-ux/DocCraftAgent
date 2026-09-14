use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct DbConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub tls: bool,
    pub max_connections: u32,
}
impl Default for DbConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 3306,
            database: "doccraft_agent".into(),
            user: "root".into(),
            password: String::new(),
            tls: false,
            max_connections: 8,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context_limit: u32,
    pub model_context_limit: u32,
    pub max_output_tokens: u32,
    pub model_max_output: u32,
    pub safety_percent: u32,
    pub token_mode: String,
    pub token_count_url: String,
    pub output_parameter: String,
    pub reasoning: String,
    pub effort: String,
    pub reasoning_parameter: String,
    pub proxy_mode: String,
    pub proxy_url: String,
    pub proxy_user: String,
    pub proxy_password: String,
    pub ca_path: String,
    pub timeout_seconds: u64,
    pub retries: u32,
    pub concurrency: usize,
    pub rpm: u32,
    pub tpm: u32,
    pub input_price: f64,
    pub output_price: f64,
}
impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8000/v1".into(),
            api_key: String::new(),
            model: String::new(),
            context_limit: 200_000,
            model_context_limit: 200_000,
            max_output_tokens: 16384,
            model_max_output: 32768,
            safety_percent: 20,
            token_mode: "estimate".into(),
            token_count_url: String::new(),
            output_parameter: "max_completion_tokens".into(),
            reasoning: "off".into(),
            effort: "medium".into(),
            reasoning_parameter: "reasoning_effort".into(),
            proxy_mode: "none".into(),
            proxy_url: String::new(),
            proxy_user: String::new(),
            proxy_password: String::new(),
            ca_path: String::new(),
            timeout_seconds: 180,
            retries: 3,
            concurrency: 2,
            rpm: 30,
            tpm: 300_000,
            input_price: 0.0,
            output_price: 0.0,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Settings {
    pub db: DbConfig,
    pub llm: LlmConfig,
    pub source_roots: Vec<String>,
    pub output_roots: Vec<String>,
    pub max_jobs: usize,
    pub max_file_bytes: u64,
    pub retention_days: u32,
    pub cache_max_mb: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            db: DbConfig::default(),
            llm: LlmConfig::default(),
            source_roots: vec![],
            output_roots: vec![],
            max_jobs: 2,
            max_file_bytes: 20 * 1024 * 1024,
            retention_days: 30,
            cache_max_mb: 1024,
        }
    }
}
#[derive(Clone, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct TaskConfig {
    pub id: String,
    pub name: String,
    pub sources: Vec<String>,
    pub target: String,
    pub direction: String,
    pub language: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_iterations: u32,
    pub max_diagrams: Option<u32>,
    pub max_seconds: u64,
    pub max_tokens: u64,
    pub max_cost: f64,
    pub preview_outline: bool,
}
impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            sources: vec![],
            target: String::new(),
            direction: String::new(),
            language: "한국어".into(),
            include: vec![],
            exclude: vec![],
            max_iterations: 3,
            max_diagrams: None,
            max_seconds: 7200,
            max_tokens: 2_000_000,
            max_cost: 0.0,
            preview_outline: false,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub task: TaskConfig,
    pub settings: Settings,
    pub original_hash: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct SectionPlan {
    #[serde(default)]
    pub id: String,
    pub title: String,
    pub query: String,
    #[serde(default)]
    pub reader_question: String,
    #[serde(default)]
    pub handoff: String,
    #[serde(default)]
    pub diagrams: Option<Vec<String>>,
    /// Reading prerequisites, expressed as earlier zero-based section indices.
    #[serde(default)]
    pub depends_on: Vec<usize>,
    /// Source passages read before planning, retained for the section writer.
    #[serde(default)]
    pub evidence_ids: Vec<String>,
    #[serde(default)]
    pub owns_requirement_ids: Vec<String>,
    #[serde(default)]
    pub key_points: Vec<String>,
    #[serde(default)]
    pub out_of_scope: Vec<String>,
}
#[derive(Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct Outline {
    pub sections: Vec<SectionPlan>,
    #[serde(default)]
    pub reader_goal: String,
    #[serde(default)]
    pub storyline: String,
    #[serde(default)]
    pub terminology: Vec<String>,
    #[serde(default)]
    pub requirements: Vec<Requirement>,
    #[serde(default)]
    pub revision: u32,
}
#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct Requirement {
    pub id: String,
    pub question: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct OutlineReview {
    pub issues: Vec<OutlineIssue>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct OutlineIssue {
    pub severity: String,
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub section_ids: Vec<String>,
    #[serde(default)]
    pub requirement_ids: Vec<String>,
    #[serde(default)]
    pub query: String,
}
#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct Issue {
    pub severity: String,
    pub section: usize,
    pub message: String,
    #[serde(default)]
    pub query: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Review {
    pub issues: Vec<Issue>,
    #[serde(default)]
    pub outline_issues: Vec<OutlineIssue>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Section {
    pub title: String,
    pub markdown: String,
    pub evidence: Vec<Evidence>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub path: String,
    pub start: u32,
    pub end: u32,
    pub content: String,
}
#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct RunView {
    pub id: String,
    pub task_id: String,
    pub status: String,
    pub progress: serde_json::Value,
    pub error: Option<String>,
    pub tokens: u64,
    pub cost: f64,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct EventView {
    pub id: u64,
    pub run_id: String,
    pub kind: String,
    pub data: serde_json::Value,
}
