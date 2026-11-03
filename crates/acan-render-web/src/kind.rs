//! Maps a machine's kind to the model that draws it, and gives each kind its plan footprint.
//! The scan decides each machine's kind (see acan-scan's `kind` module); the plant JSON carries it.
//! Pure Rust, so the table is unit tested on the host and serves live, fixture and stress plants.

pub use acan_iso::Kind;

/// Every kind, in the order their models are built. A kind's index here is its model's index.
pub const ALL: [Kind; 17] = [
    Kind::Intake,
    Kind::Gate,
    Kind::Assembler,
    Kind::Tank,
    Kind::Splitter,
    Kind::Valve,
    Kind::Crate,
    Kind::Switchboard,
    Kind::Kiosk,
    Kind::Mould,
    Kind::Conveyor,
    Kind::Horn,
    Kind::Switchbox,
    Kind::Gantry,
    Kind::Console,
    Kind::Antenna,
    Kind::Toolrack,
];

/// The index of `kind` in [`ALL`].
pub fn index(kind: Kind) -> usize {
    ALL.iter()
        .position(|&k| k == kind)
        .expect("ALL lists every kind")
}

/// A kind's plan footprint as `(width, depth)`, from acan-iso's table.
pub trait PlanFootprint {
    fn footprint(self) -> (f32, f32);
}

impl PlanFootprint for Kind {
    fn footprint(self) -> (f32, f32) {
        let footprint = acan_iso::footprint(self);
        (footprint.w, footprint.d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprints_match_the_brief() {
        assert_eq!(Kind::Assembler.footprint(), (96.0, 86.0));
        assert_eq!(Kind::Crate.footprint(), (40.0, 40.0));
    }

    #[test]
    fn all_lists_every_kind_once_and_indexes_it() {
        for kind in Kind::ALL {
            assert_eq!(ALL.iter().filter(|&&k| k == kind).count(), 1, "{kind:?}");
            assert_eq!(ALL[index(kind)], kind);
        }
        assert_eq!(ALL.len(), Kind::ALL.len());
    }
}
