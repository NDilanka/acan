//! Helpers that the server's integration tests share.

use std::path::Path;
use std::str::FromStr;

use acan_core::{Actor, Event, MachineKey, OrderId, OrderRev, Seq, Stamped, TaskSpec};
use acan_server::EventStore;

/// Appends the split the job loop used to record for the order posted to `repo`, as the origin
/// accepts it. The server no longer splits orders, so a test records the split while the server is
/// stopped. The restart replays the log, and the order has one task, `t-1`, with `brief`. Returns
/// the order's id.
pub async fn record_split(repo: &Path, brief: &str) -> OrderId {
    let store = EventStore::open(&repo.join(".acan/world.db"))
        .await
        .expect("the event store opens");
    let log = store.load_from(Seq::new(0)).await.expect("the log loads");
    let order = log
        .iter()
        .find_map(|stamped| match &stamped.event {
            Event::OrderPosted { order, .. } => Some(*order),
            _ => None,
        })
        .expect("the order was posted");
    let last = log.last().expect("the log holds its start");
    let split = Stamped {
        seq: Seq::new(last.seq.get() + 1),
        at: last.at,
        actor: Actor::System,
        event: Event::OrderSplit {
            order,
            rev: OrderRev::new(0),
            tasks: vec![(
                "t-1".parse().expect("a task id"),
                TaskSpec {
                    title: "the reader and the writer".to_owned(),
                    brief: brief.to_owned(),
                    scope: vec![MachineKey::from_str("model").expect("a machine key")],
                },
            )],
        },
    };
    store.append(&[split]).await.expect("the split is stored");
    order
}
