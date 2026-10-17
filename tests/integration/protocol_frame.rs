use jensen_harness_protocol::{Frame, Notification, Request, RequestId, Response, classify};
use serde_json::json;

#[test]
fn classifies_a_request_by_method_and_id() {
    let raw = json!({"jsonrpc": "2.0", "id": 1, "method": "session.new", "params": {}});
    let Frame::Request(request) = classify(&raw).unwrap() else {
        panic!("expected a request");
    };
    assert_eq!(request.method, "session.new");
    assert_eq!(request.id, RequestId::Number(1));
}

#[test]
fn classifies_a_notification_by_method_without_id() {
    let raw = json!({"jsonrpc": "2.0", "method": "session.update", "params": {}});
    assert!(matches!(classify(&raw).unwrap(), Frame::Notification(_)));
}

#[test]
fn classifies_a_response_by_id_without_method() {
    let raw = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
    assert!(matches!(classify(&raw).unwrap(), Frame::Response(_)));
}

#[test]
fn round_trips_a_request_through_json() {
    let request = Request::new(RequestId::Text("a".to_string()), "session.new", None);
    let raw = serde_json::to_value(&request).unwrap();
    let Frame::Request(parsed) = classify(&raw).unwrap() else {
        panic!("expected a request");
    };
    assert_eq!(parsed.id, request.id);
}

#[test]
fn round_trips_a_notification_through_json() {
    let notification = Notification::new("session.update", None);
    let raw = serde_json::to_value(&notification).unwrap();
    assert!(matches!(classify(&raw).unwrap(), Frame::Notification(_)));
}

#[test]
fn round_trips_a_response_through_json() {
    let response = Response::ok(RequestId::Number(7), json!({"ok": true}));
    let raw = serde_json::to_value(&response).unwrap();
    assert!(matches!(classify(&raw).unwrap(), Frame::Response(_)));
}
