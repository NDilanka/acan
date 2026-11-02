//! The plant view of an order's latest receipt, served by `GET /api/orders/{order}/plant-view`.
//!
//! Every fact comes from the receipt, the confirmed scope of its order, the order's plate and the
//! route table of the protected corpus the receipt ran against. That corpus is read at the
//! receipt's base commit, and the view is refused when it does not hold the number of routes the
//! receipt counted. The director's tour is filled in by [`crate::tour`].

use std::str::FromStr;

use acan_core::{MachineKey, OrderId, World};
use acan_intent::{MachineId, PlantView, ProbeCorpus, Receipt, RouteEntry, RouteTemplate, Scope};
use acan_llm::ModelClient;
use anyhow::anyhow;

use crate::STATE_DIR;
use crate::host::Host;
use crate::http::ApiError;
use crate::jobs;
use crate::model;
use crate::observe;
use crate::receipt::{MachineInput, PlateOrder, plant_view};
use crate::tour;

/// What a plant view answers when the order has no receipt for its confirmed scope yet.
const NO_RECEIPT: &str = "no receipt for this order yet";

/// The plant view of `order`'s latest receipt, with its tour filled in.
///
/// # Errors
///
/// Returns [`ApiError::NotFound`] when the order does not exist, has no confirmed scope, or has
/// no receipt for its confirmed scope. Returns [`ApiError::Internal`] when the receipt's corpus
/// cannot be read or does not match the receipt, or when the view cannot be built from the
/// receipt; the detail goes to the log.
pub(crate) async fn order_view<C: ModelClient + 'static>(
    host: &Host<C>,
    order: OrderId,
) -> Result<PlantView, ApiError> {
    let (receipt, scope, plate) = host.read(|world| latest(world, order))?;
    let workspace = host.workspace().ok_or_else(|| {
        ApiError::Internal(anyhow!(
            "the server has no git repository to read the protected corpus from"
        ))
    })?;
    let state = host.repo().join(STATE_DIR);
    let snapshot = jobs::receipt_snapshot(workspace.repo(), &state, &receipt.prepared)
        .await
        .map_err(ApiError::Internal)?;
    let (_app, corpus) =
        observe::read_protected(&snapshot).map_err(|err| ApiError::Internal(err.into()))?;
    let machines = model::plant_machines(&host.scan().index);
    let view = assemble(&receipt, &scope, &plate, &machines, &corpus)?;
    Ok(tour::fill(host, view).await)
}

/// The receipt the plant view answers for: the latest one whose scope revision is the order's
/// confirmed one, with the scope and the plate it was built against.
fn latest(world: &World, order: OrderId) -> Result<(Receipt, Scope, PlateOrder), ApiError> {
    let row = world.order(order).ok_or(ApiError::NotFound("order"))?;
    let lane = world.lane(order).ok_or(ApiError::NotFound(NO_RECEIPT))?;
    let confirmation = lane
        .confirmation
        .as_ref()
        .ok_or(ApiError::NotFound("no confirmed scope for this order"))?;
    let record = lane
        .runs
        .values()
        .rev()
        .filter_map(|run| run.receipt.as_ref())
        .find(|record| record.receipt.scope_rev == lane.scope.rev)
        .ok_or(ApiError::NotFound(NO_RECEIPT))?;
    let plate = PlateOrder {
        sentence: row.sentence.clone(),
        machine_id: machine_id(&row.machine)?,
        confirmed_at: Some(confirmation.at),
    };
    Ok((record.receipt.clone(), lane.scope.clone(), plate))
}

/// The plant view of `receipt` against its corpus. The machines are the plant's, and each one
/// lists the corpus routes whose static path passes through it.
///
/// # Errors
///
/// Returns [`ApiError::Internal`] when the corpus does not hold the receipt's route count, or the
/// receipt, scope, order or machines do not build a view.
pub(crate) fn assemble(
    receipt: &Receipt,
    scope: &Scope,
    plate: &PlateOrder,
    machines: &[MachineKey],
    corpus: &ProbeCorpus,
) -> Result<PlantView, ApiError> {
    if u32::try_from(corpus.routes.len()).ok() != Some(receipt.routes.total) {
        return Err(ApiError::Internal(anyhow!(
            "the protected corpus at {} holds {} routes, not the receipt's {}",
            receipt.base_commit(),
            corpus.routes.len(),
            receipt.routes.total,
        )));
    }
    let inputs = machines
        .iter()
        .map(|key| machine_input(key, &corpus.routes))
        .collect::<Result<Vec<_>, _>>()?;
    plant_view(receipt, scope, plate, &inputs).map_err(|err| ApiError::Internal(err.into()))
}

/// One plant machine with the corpus route templates whose static path passes through it.
fn machine_input(key: &MachineKey, routes: &[RouteEntry]) -> Result<MachineInput, ApiError> {
    let machine_id = machine_id(key)?;
    let mut templates: Vec<RouteTemplate> = Vec::new();
    for entry in routes {
        if entry
            .route_path
            .machines
            .iter()
            .any(|machine| *machine == machine_id)
            && !templates.contains(&entry.route_template)
        {
            templates.push(entry.route_template.clone());
        }
    }
    Ok(MachineInput {
        machine_id,
        routes: templates,
    })
}

fn machine_id(key: &MachineKey) -> Result<MachineId, ApiError> {
    MachineId::from_str(&key.to_string()).map_err(|err| ApiError::Internal(err.into()))
}

#[cfg(test)]
mod tests {
    use acan_intent::{ProbeCorpus, Receipt, Scope};
    use acan_llm::Stub;
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    use super::*;
    use crate::host::testing::host_with;

    const RECEIPT: &str = include_str!("../../acan-intent/fixtures/receipt.json");
    const SCOPE: &str = include_str!("../../acan-intent/fixtures/scope.json");
    const CORPUS: &str = include_str!("../../acan-intent/fixtures/probe-corpus.json");
    const PLAIN: &str = include_str!("../../acan-intent/fixtures/plant-view-v1-plain.json");

    fn decode<T: DeserializeOwned>(text: &str) -> T {
        serde_json::from_str(text).expect("the fixture decodes")
    }

    /// The order's plate and the plant's machines, as the plain fixture records them.
    fn plate_and_machines(plain: &Value) -> (PlateOrder, Vec<MachineKey>) {
        let order = &plain["order"];
        let plate = PlateOrder {
            sentence: order["sentence"]
                .as_str()
                .expect("the order has a sentence")
                .to_owned(),
            machine_id: serde_json::from_value(order["machine_id"].clone())
                .expect("the order has a machine"),
            confirmed_at: serde_json::from_value(order["confirmed_at"].clone())
                .expect("the order has a confirmation time"),
        };
        let machines = plain["machines"]
            .as_array()
            .expect("the plain view lists its machines")
            .iter()
            .map(|machine| {
                serde_json::from_value(machine["machine_id"].clone()).expect("a machine key")
            })
            .collect();
        (plate, machines)
    }

    #[test]
    fn assemble_builds_the_fixture_receipt_against_its_corpus_with_no_tour_yet() {
        let plain: Value = decode(PLAIN);
        let (plate, machines) = plate_and_machines(&plain);
        let receipt: Receipt = decode(RECEIPT);
        let scope: Scope = decode(SCOPE);
        let corpus: ProbeCorpus = decode(CORPUS);

        let view = assemble(&receipt, &scope, &plate, &machines, &corpus)
            .expect("the fixture receipt builds a view");

        assert_eq!(
            view.receipt_hash,
            receipt.receipt_hash().expect("the receipt hashes")
        );
        assert_eq!(view.items.len(), receipt.items.len());
        assert!(view.tour.is_none(), "the tour is filled after the view");
    }

    #[test]
    fn assemble_refuses_a_corpus_that_does_not_hold_the_receipts_routes() {
        let plain: Value = decode(PLAIN);
        let (plate, machines) = plate_and_machines(&plain);
        let receipt: Receipt = decode(RECEIPT);
        let scope: Scope = decode(SCOPE);
        let mut corpus: ProbeCorpus = decode(CORPUS);
        corpus.routes.pop();

        let err = assemble(&receipt, &scope, &plate, &machines, &corpus)
            .expect_err("a corpus short of the receipt's routes is refused");

        assert!(matches!(err, ApiError::Internal(_)), "{err:?}");
    }

    #[tokio::test]
    async fn an_order_the_world_does_not_have_is_not_found() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let host = host_with(dir.path(), Stub::default()).await;
        let order = "o-1".parse().expect("an order id");

        let err = order_view(&host, order)
            .await
            .expect_err("no order is in the world");

        assert!(matches!(err, ApiError::NotFound("order")), "{err:?}");
    }
}
