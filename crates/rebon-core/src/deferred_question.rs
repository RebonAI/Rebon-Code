//! 仅兼容读取 `AskUserQuestion` 曾经返回的 pending 占位记录。
//! 新调用始终等待回答或取消，此处不能重新生成或投递异步答案。

use serde_json::Value;

const PENDING_LEAD: &str = "Your question is on the user's screen; the answer has not arrived yet.";

/// 保留历史 pending 结果面向模型的原文，避免伪造用户答案。
pub(crate) fn pending_model_text(value: &Value) -> Option<&str> {
    if value.get("status").and_then(Value::as_str) != Some("pending") {
        return None;
    }
    value.get("message").and_then(Value::as_str)
}

/// 历史占位记录不代表用户已经回答或授权。
pub(crate) fn is_pending_model_text(text: &str) -> bool {
    text.starts_with(PENDING_LEAD)
}

/// 固定的历史输出样本，不能用于生成新调用的结果。
#[cfg(test)]
pub(crate) const LEGACY_PENDING_RESULT: &str = r#"{
    "status": "pending",
    "toolUseId": "toolu_9",
    "message": "Your question is on the user's screen; the answer has not arrived yet. It will come later as a user message wrapped in <question-answer tool_use_id=\"toolu_9\">. Until then, continue only with work that does not depend on the answer, and never guess it. If nothing independent is left, end your turn with a short note saying what you are waiting for."
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn historical_pending_results_are_still_recognized() {
        let value = serde_json::from_str(LEGACY_PENDING_RESULT).unwrap();
        let text = pending_model_text(&value).expect("historical pending text");
        assert!(is_pending_model_text(text));
        assert!(text.contains("<question-answer tool_use_id=\"toolu_9\">"));
    }

    #[test]
    fn an_answered_result_is_not_pending() {
        let value = json!({ "questions": [], "answers": { "Q?": "A" } });
        assert_eq!(pending_model_text(&value), None);
        assert!(!is_pending_model_text("User has answered your questions"));
    }
}
