use axum::extract::Path;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::AppState;

const AGENTS: [(&str, &str, &str, &str); 4] = [
    ("solve", "HelpCircle", "blue", "Problem Solved"),
    ("question", "FileText", "purple", "Question Generated"),
    ("research", "Search", "emerald", "Research Report"),
    ("co_writer", "PenTool", "amber", "Co-Writer"),
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/agent-config/agents", get(list_agents))
        .route("/agent-config/agents/{agent_type}", get(get_agent))
}

async fn list_agents() -> Json<Value> {
    Json(json!(AGENTS
        .into_iter()
        .map(|(id, icon, color, label_key)| {
            (
                id.to_string(),
                json!({
                    "icon": icon,
                    "color": color,
                    "label_key": label_key
                }),
            )
        })
        .collect::<serde_json::Map<String, Value>>()))
}

async fn get_agent(Path(agent_type): Path<String>) -> Json<Value> {
    AGENTS
        .into_iter()
        .find(|(id, _, _, _)| *id == agent_type)
        .map(|(_, icon, color, label_key)| {
            Json(json!({
                "icon": icon,
                "color": color,
                "label_key": label_key
            }))
        })
        .unwrap_or_else(|| Json(json!({"error": format!("Agent type '{agent_type}' not found")})))
}
