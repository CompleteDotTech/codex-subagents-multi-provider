use super::*;
use bytes::Bytes;
use codex_client::TransportError;
use futures::stream;
use http::HeaderMap;
use http::HeaderValue;
use http::StatusCode;
use serde_json::json;

async fn events(body: String, content_type: &'static str) -> Vec<Result<ResponseEvent, ApiError>> {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    let response = StreamResponse {
        status: StatusCode::OK,
        headers,
        bytes: Box::pin(stream::iter(vec![Ok::<_, TransportError>(Bytes::from(
            body,
        ))])),
    };
    let mut response = spawn_response_stream(response, Duration::from_secs(1), None, None);
    let mut events = Vec::new();
    while let Some(event) = response.next().await {
        events.push(event);
    }
    events
}

fn sse(values: Vec<Value>) -> String {
    values
        .into_iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect()
}

#[tokio::test]
async fn json_http_200_errors_preserve_quota_and_rate_categories() {
    for (code, quota) in [("insufficient_quota", true), ("rate_limit_exceeded", false)] {
        let events = events(
            json!({"error":{"code":code,"message":"bounded provider failure"}}).to_string(),
            "application/json; charset=utf-8",
        )
        .await;
        assert_eq!(events.len(), 1);
        if quota {
            assert!(matches!(events[0], Err(ApiError::QuotaExceeded)));
        } else {
            assert!(matches!(events[0], Err(ApiError::RateLimitExceeded { .. })));
        }
    }
}

#[tokio::test]
async fn sse_errors_cannot_be_overridden_by_a_later_success_event() {
    for failure in [
        json!({"type":"error","error":{"code":"insufficient_quota","message":"quota"}}),
        json!({"type":"error","code":"insufficient_quota","message":"quota"}),
        json!({"error":{"code":"insufficient_quota","message":"quota"}}),
    ] {
        let events = events(
            sse(vec![
                failure,
                json!({"type":"response.completed","response":{"id":"must-not-succeed"}}),
            ]),
            "text/event-stream",
        )
        .await;
        // The normal header parser emits its empty rate-limit snapshot before
        // SSE data. It must not be confused with successful response completion.
        assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
        assert!(matches!(events.last(), Some(Err(ApiError::QuotaExceeded))));
        assert!(events.iter().all(|event| matches!(
            event,
            Ok(ResponseEvent::RateLimits(_)) | Err(ApiError::QuotaExceeded)
        )));
    }
}

#[tokio::test]
async fn malformed_sse_and_unfinished_tool_calls_never_complete_successfully() {
    let malformed=events("data: {invalid\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"bad\"}}\n\n".into(),"text/event-stream").await;
    assert_eq!(malformed.iter().filter(|event| event.is_err()).count(), 1);
    assert!(
        matches!(malformed.last(),Some(Err(ApiError::Stream(message))) if message=="invalid JSON in Responses stream")
    );
    assert!(malformed.iter().all(|event| matches!(
        event,
        Ok(ResponseEvent::RateLimits(_)) | Err(ApiError::Stream(_))
    )));
    let unfinished=events(sse(vec![
        json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc1","call_id":"call1","name":"exec_command","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc1","delta":"{\"cmd\":"}),
        json!({"type":"response.completed","response":{"id":"bad"}}),
    ]),"text/event-stream").await;
    assert!(unfinished.iter().any(|event| matches!(event,Err(ApiError::Stream(message)) if message.contains("unfinished tool"))));
    assert!(
        !unfinished
            .iter()
            .any(|event| matches!(event, Ok(ResponseEvent::Completed { .. })))
    );
}

#[tokio::test]
async fn compatible_text_usage_and_function_completion_remain_streamed() {
    let events=events(sse(vec![
        json!({"type":"response.output_text.delta","delta":"hello"}),
        json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"c","name":"f","arguments":""}}),
        json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c","name":"f","arguments":"{}"}}),
        json!({"type":"response.completed","response":{"id":"ok","usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}),
    ]),"text/event-stream").await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event,Ok(ResponseEvent::OutputTextDelta(text)) if text=="hello"))
    );
    assert!(
        matches!(events.last(),Some(Ok(ResponseEvent::Completed{token_usage:Some(usage),..})) if usage.total_tokens==5)
    );
    assert!(events.iter().all(Result::is_ok));
}
