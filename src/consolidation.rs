//! P1 写入整合（mem0 式）——LLM 判定新记忆与既有记忆的关系（ADD/UPDATE/NOOP）
//! + 实体/关系抽取（喂 P2 Kuzu 时序图谱）。
//!
//! 架构纪律：memoria 默认哑存储（不调 LLM）。本模块**仅在** `MEMORIA_CONSOLIDATION_URL`
//! 非空时启用；任何失败（超时/非 200/JSON 解析失败）一律返回 `None` → 写入路径退化为
//! 原生 ADD，绝不阻塞、绝不报错给调用方（与 embed_query 的 fail-open 契约一致）。

use serde_json::{json, Value};

/// 单条整合判定结果
#[derive(Debug, Clone)]
pub struct Decision {
    /// ADD = 新事实；UPDATE = 新事实取代旧（映射为 supersedes_id 走既有显式取代链）；NOOP = 重复不写
    pub action: String,
    /// UPDATE 的目标记忆 id（来自候选列表）
    pub target_id: Option<String>,
    /// LLM 判定理由（写审计/响应，便于追溯）
    pub reason: String,
    /// 抽取实体（P2 图谱）
    pub entities: Vec<ExtractedEntity>,
    /// 抽取关系（P2 图谱）
    pub edges: Vec<ExtractedEdge>,
}

#[derive(Debug, Clone)]
pub struct ExtractedEntity {
    pub name: String,
    pub etype: String,
}

#[derive(Debug, Clone)]
pub struct ExtractedEdge {
    pub subject: String,
    pub predicate: String,
    pub obj: String,
    pub valid_from: String,
    pub valid_to: String,
}

pub const CONSOLIDATION_SYSTEM_PROMPT: &str = r#"你是记忆库的写入整合判定器。给你「新记忆」和若干条「既有记忆候选」，判定新记忆与它们的关系，并抽取实体与关系。只输出一个 JSON 对象，不要任何解释或 markdown 代码块。

判定规则：
- NOOP：新记忆与某候选说的是同一件事（换措辞/同义重述/信息完全被候选覆盖）→ target_id 填该候选 id。
- UPDATE：新记忆是对某候选事实的更新/修正/状态变化（旧事实已过时，如"用A方案"→"改用B方案"）→ target_id 填该候选 id。
- ADD：新信息、或对候选的补充扩展（旧事实仍有效）→ target_id 填 null。
拿不准时选 ADD。target_id 只能来自候选列表，否则视为 ADD。

同时抽取新记忆中的实体与关系（没有就给空数组）：
- entities.etype 只能取：person/system/tool/concept/org/project/location/event/other
- edges.predicate 用小写下划线（如 uses/depends_on/part_of/works_at/located_in/references/supersedes/created_by）
- 涉及有效期时填 valid_from/valid_to（YYYY-MM-DD），否则空字符串

输出格式：
{"action":"ADD|UPDATE|NOOP","target_id":null,"reason":"≤30字","entities":[{"name":"...","etype":"..."}],"edges":[{"subject":"...","predicate":"...","obj":"...","valid_from":"","valid_to":""}]}"#;

/// 从 LLM 返回文本中稳健提取 JSON（容忍 ```json 围栏、前后闲话）
pub fn parse_decision(text: &str) -> Option<Decision> {
    let s = text.trim();
    let s = s.strip_prefix("```json").or_else(|| s.strip_prefix("```")).unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s).trim();
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end < start {
        return None;
    }
    let v: Value = serde_json::from_str(&s[start..=end]).ok()?;
    let action = v.get("action").and_then(|a| a.as_str())?.to_uppercase();
    if !matches!(action.as_str(), "ADD" | "UPDATE" | "NOOP") {
        return None;
    }
    let target_id = v
        .get("target_id")
        .and_then(|t| t.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "null");
    if action == "UPDATE" && target_id.is_none() {
        return None; // UPDATE 必须有目标，否则按解析失败处理（fail-open ADD）
    }
    let reason = v
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    let entities = v
        .get("entities")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let name = e.get("name").and_then(|n| n.as_str())?.trim().to_string();
                    if name.is_empty() || name.chars().count() > 120 {
                        return None;
                    }
                    let etype = e.get("etype").and_then(|t| t.as_str()).unwrap_or("other");
                    let etype = ["person", "system", "tool", "concept", "org", "project", "location", "event", "other"]
                        .iter()
                        .find(|x| **x == etype)
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| "other".to_string());
                    Some(ExtractedEntity { name, etype })
                })
                .collect()
        })
        .unwrap_or_default();
    let edges = v
        .get("edges")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let subject = e.get("subject").and_then(|s| s.as_str())?.trim().to_string();
                    let obj = e
                        .get("obj")
                        .or_else(|| e.get("object"))
                        .and_then(|o| o.as_str())?
                        .trim()
                        .to_string();
                    let predicate = e
                        .get("predicate")
                        .and_then(|p| p.as_str())
                        .unwrap_or("related_to")
                        .trim()
                        .to_lowercase()
                        .chars()
                        .take(60)
                        .collect();
                    if subject.is_empty() || obj.is_empty() || subject == obj {
                        return None;
                    }
                    Some(ExtractedEdge {
                        subject,
                        predicate,
                        obj,
                        valid_from: e.get("valid_from").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        valid_to: e.get("valid_to").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Decision { action, target_id, reason, entities, edges })
}

/// 调 OpenAI 兼容 chat/completions 判定。任何失败 → None（fail-open）。
pub async fn classify(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    api_key: &str,
    content: &str,
    candidates: &[(String, String)], // (memory_id, content 截断)
) -> Option<Decision> {
    if url.is_empty() || candidates.is_empty() {
        // 无候选 = 必然 ADD，无需问 LLM
        return if candidates.is_empty() {
            Some(Decision {
                action: "ADD".into(),
                target_id: None,
                reason: "无既有候选".into(),
                entities: vec![],
                edges: vec![],
            })
        } else {
            None
        };
    }
    let cand_json: Vec<Value> = candidates
        .iter()
        .take(5)
        .map(|(id, c)| json!({"id": id, "content": c.chars().take(300).collect::<String>()}))
        .collect();
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": CONSOLIDATION_SYSTEM_PROMPT},
            {"role": "user", "content": json!({
                "new_memory": content.chars().take(1200).collect::<String>(),
                "candidates": cand_json
            }).to_string()},
        ],
        "temperature": 0.1,
        "max_tokens": 600,
        "stream": false,
        // qwen3 系默认开思考模式（10-30s 级延迟），compatible-mode 下显式关闭
        "enable_thinking": false,
    });
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        let resp = client
            .post(url)
            .timeout(std::time::Duration::from_secs(20))
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(_) => continue,
        };
        if !resp.status().is_success() {
            continue;
        }
        let v: Value = resp.json().await.ok()?;
        let text = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())?;
        return parse_decision(text);
    }
    None
}

/// 抽取实体的确定性 id（同 ns 同名幂等 upsert）
pub fn auto_entity_id(ns: &str, name: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("{ns}|{name}").as_bytes());
    let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
    format!("auto-{}", &hex[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json() {
        let d = parse_decision(r#"{"action":"UPDATE","target_id":"abc123","reason":"事实已变更","entities":[{"name":"memoria","etype":"system"}],"edges":[]}"#).unwrap();
        assert_eq!(d.action, "UPDATE");
        assert_eq!(d.target_id.as_deref(), Some("abc123"));
        assert_eq!(d.entities.len(), 1);
        assert_eq!(d.entities[0].etype, "system");
    }

    #[test]
    fn parse_fenced_with_noise() {
        let d = parse_decision("好的，判定如下：\n```json\n{\"action\":\"NOOP\",\"target_id\":\"deadbeef\",\"reason\":\"重复\"}\n```").unwrap();
        assert_eq!(d.action, "NOOP");
        assert_eq!(d.entities.len(), 0);
    }

    #[test]
    fn update_without_target_rejected() {
        assert!(parse_decision(r#"{"action":"UPDATE","target_id":null}"#).is_none());
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_decision("模型抽风了").is_none());
        assert!(parse_decision(r#"{"action":"DESTROY"}"#).is_none());
    }

    #[test]
    fn entity_etype_whitelisted_and_edges_sanitized() {
        let d = parse_decision(r#"{"action":"ADD","entities":[{"name":"张三","etype":" alien"},{"name":"","etype":"person"}],"edges":[{"subject":"张三","predicate":"Uses","obj":"张三"},{"subject":"张三","predicate":"uses","obj":"memoria"}]}"#).unwrap();
        assert_eq!(d.entities.len(), 1);
        assert_eq!(d.entities[0].etype, "other"); // 非法 etype 落 other
        assert_eq!(d.edges.len(), 1); // 自环被滤
        assert_eq!(d.edges[0].predicate, "uses"); // 归一小写
    }
}
