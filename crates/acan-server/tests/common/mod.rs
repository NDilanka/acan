//! Helpers that the server's integration tests share.

use serde_json::{Value, json};

/// The split command for the order created by `posted`.
pub fn split_order(posted: &Value, brief: &str) -> Value {
    let order = posted
        .as_array()
        .expect("the command returns a batch")
        .iter()
        .map(|stamped| &stamped["event"])
        .find(|event| event["type"] == "order_posted")
        .expect("the order was posted");
    json!({
        "cmd": "split_order",
        "order": order["order"],
        "rev": 0,
        "tasks": [{
            "title": "the reader and the writer",
            "brief": brief,
            "scope": ["model"]
        }]
    })
}
