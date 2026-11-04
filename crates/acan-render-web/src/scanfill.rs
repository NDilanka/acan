//! The first scan of a codebase, as the map draws it. Each machine starts as a ghost outline on the
//! ground, and fills in as its module finishes: its body drops onto the outline while the outline
//! fades. A page that already shows a settled map, or a rescan of one, builds every machine at
//! once, so a rescan updates the map where it stands. This file holds the pure parts: each
//! machine's phase and its ghost strokes. `App` keeps one [`ScanFill`], feeds it the scan's status
//! and steps, and draws what it reports.

use acan_render::Beam;
use acan_render::theme::TINT_GHOST;
use serde::Deserialize;

/// How long a machine takes to fill in, in milliseconds.
pub const FILL_MS: f32 = 450.0;

/// How far above its place a body stands at the start of its fill, in plan units. It drops onto
/// the place as the fill runs.
pub const DROP: f32 = 2.0;

/// The outline's opacity while its machine has not filled.
pub const GHOST_ALPHA: f32 = 0.8;

/// The outline's stroke width in device px, before the shader's thinnest-stroke floor.
pub const GHOST_PX: f32 = 1.75;

/// How far the outline lies outside its footprint, in plan units.
pub const GHOST_OFFSET: f32 = 3.0;

/// Where a scan is, as a status reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// The codebase is known, but its scan waits for the first page. The plant has no machines
    /// yet, so the map is blank until the scan starts.
    Waiting,
    /// The machines are known, and their modules are not all filled yet.
    Skeleton,
    /// Modules are filling in.
    Fill,
    /// Every module is filled, and the state machines are being resolved.
    States,
    /// The code index is committed.
    Done,
    /// The scan failed.
    Failed,
}

impl Stage {
    /// Whether the scan has ended, so its map is settled.
    #[must_use]
    pub fn ended(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// A scan's status as the page reads it: its stage and the modules that have filled. A plant's
/// `scan` summary has no list, so it reads as none filled.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Status {
    pub stage: Stage,
    #[serde(default)]
    pub filled: Vec<Filled>,
}

/// One filled module, by the name of the machine it fills.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Filled {
    pub name: String,
}

/// Reads a status from a `scan_status` frame or from a plant's `scan` summary. Other fields are
/// passed over.
///
/// # Errors
///
/// Returns the JSON error when the text is not a status.
pub fn parse_status(json: &str) -> Result<Status, serde_json::Error> {
    serde_json::from_str(json)
}

/// Where one machine is in its fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// Its outline only: its module has not filled.
    Outline,
    /// Filling in, this many milliseconds in.
    Filling(f32),
    /// Built: its body stands in its place, with no outline.
    Built,
}

/// Each machine's phase on the map on screen, by its index in the plant.
#[derive(Debug, Default)]
pub struct ScanFill {
    phases: Vec<Phase>,
    /// Whether the page has shown a settled map. A page that has never shown one animates its
    /// first scan, and no other scan.
    settled: bool,
}

impl ScanFill {
    /// Puts a plant of `machines` on screen, with every machine built until a status says otherwise.
    pub fn load(&mut self, machines: usize) {
        self.phases = vec![Phase::Built; machines];
    }

    /// Folds a status into the phases. The machines `filled` names by index are built. A machine
    /// that has not filled is an outline, unless its fill is already running. An ended scan, or a
    /// map that was settled before, builds every machine at once.
    pub fn apply(&mut self, stage: Stage, filled: &[usize]) {
        self.settled |= stage.ended();
        let mut done = vec![false; self.phases.len()];
        for &index in filled {
            if let Some(slot) = done.get_mut(index) {
                *slot = true;
            }
        }
        for (index, phase) in self.phases.iter_mut().enumerate() {
            *phase = match *phase {
                Phase::Filling(elapsed) if !self.settled => Phase::Filling(elapsed),
                _ if self.settled || done[index] => Phase::Built,
                _ => Phase::Outline,
            };
        }
    }

    /// Starts machine `index` filling in. A settled map, or a page that asks for reduced motion,
    /// builds it at once. Returns whether the plant has that machine.
    pub fn fill(&mut self, index: usize, reduced: bool) -> bool {
        let Some(phase) = self.phases.get_mut(index) else {
            return false;
        };
        if self.settled || reduced {
            *phase = Phase::Built;
        } else if *phase == Phase::Outline {
            *phase = Phase::Filling(0.0);
        }
        true
    }

    /// Moves the fills on by `dt_ms`. Returns whether any fill was running at the start of the
    /// step, so the frame that completes a fill still uploads its last place.
    pub fn advance(&mut self, dt_ms: f32) -> bool {
        let mut moving = false;
        for phase in &mut self.phases {
            if let Phase::Filling(elapsed) = *phase {
                moving = true;
                let next = elapsed + dt_ms.max(0.0);
                *phase = if next >= FILL_MS {
                    Phase::Built
                } else {
                    Phase::Filling(next)
                };
            }
        }
        moving
    }

    /// Whether machine `index` is an outline, so its body is not on the map yet.
    #[must_use]
    pub fn hidden(&self, index: usize) -> bool {
        matches!(self.phases.get(index), Some(Phase::Outline))
    }

    /// How far above its place machine `index`'s body stands, in plan units.
    #[must_use]
    pub fn drop_of(&self, index: usize) -> f32 {
        match self.phases.get(index) {
            Some(Phase::Filling(elapsed)) => DROP * (1.0 - ease_out(elapsed / FILL_MS)),
            _ => 0.0,
        }
    }

    /// The opacity of machine `index`'s outline, or `None` when it has none to draw.
    #[must_use]
    pub fn ghost_alpha(&self, index: usize) -> Option<f32> {
        match self.phases.get(index)? {
            Phase::Outline => Some(GHOST_ALPHA),
            Phase::Filling(elapsed) => {
                let left = 1.0 - ease_out(elapsed / FILL_MS);
                (left > 0.0).then_some(GHOST_ALPHA * left)
            }
            Phase::Built => None,
        }
    }

    /// The outline strokes now: each drawn machine's footprint, as `[x, y, w, h]` in plan units by
    /// machine index, is a grey loop of four strokes on the ground.
    #[must_use]
    pub fn ghosts(&self, footprints: &[[f32; 4]], dpr: f32) -> Vec<Beam> {
        let tint = u8::try_from(TINT_GHOST).expect("tint slots fit in a byte");
        (0..self.phases.len())
            .filter_map(|index| Some((footprints.get(index)?, self.ghost_alpha(index)?)))
            .flat_map(|(footprint, alpha)| {
                let corners = outline_corners(*footprint);
                (0..4).map(move |i| Beam {
                    from: corners[i],
                    to: corners[(i + 1) % 4],
                    origin: [0.0; 3],
                    grow: [0.0; 2],
                    turn: 0.0,
                    tint,
                    alpha,
                    width_px: GHOST_PX * dpr,
                    spot: 0.0,
                })
            })
            .collect()
    }
}

/// The four corners of a footprint `[x, y, w, h]`, grown by [`GHOST_OFFSET`], on the ground.
fn outline_corners([x, y, w, h]: [f32; 4]) -> [[f32; 3]; 4] {
    let (x0, y0) = (x - GHOST_OFFSET, y - GHOST_OFFSET);
    let (x1, y1) = (x + w + GHOST_OFFSET, y + h + GHOST_OFFSET);
    [[x0, y0, 0.0], [x1, y0, 0.0], [x1, y1, 0.0], [x0, y1, 0.0]]
}

/// The ease of a fill: it starts quickly and settles, and reaches 1 at the end of its time.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: [[f32; 4]; 1] = [[0.0, 0.0, 10.0, 5.0]];

    #[test]
    fn a_first_map_shows_each_unfilled_machine_as_an_outline() {
        let mut fill = ScanFill::default();
        fill.load(3);
        fill.apply(Stage::Skeleton, &[1]);
        assert_eq!(fill.phase_of(0), Phase::Outline);
        assert_eq!(fill.phase_of(1), Phase::Built);
        assert_eq!(fill.phase_of(2), Phase::Outline);
        assert!(fill.hidden(0) && !fill.hidden(1));
    }

    #[test]
    fn a_fill_drops_the_body_and_fades_the_outline_then_builds() {
        let mut fill = ScanFill::default();
        fill.load(1);
        fill.apply(Stage::Skeleton, &[]);
        assert!(fill.fill(0, false));
        assert_eq!(fill.phase_of(0), Phase::Filling(0.0));
        assert_eq!(fill.drop_of(0), DROP);
        assert_eq!(fill.ghost_alpha(0), Some(GHOST_ALPHA));

        assert!(fill.advance(FILL_MS / 2.0), "the fill runs");
        let (drop, alpha) = (fill.drop_of(0), fill.ghost_alpha(0).unwrap_or(0.0));
        assert!(
            drop > 0.0 && drop < DROP,
            "the body is part way down: {drop}"
        );
        assert!(
            alpha > 0.0 && alpha < GHOST_ALPHA,
            "the outline is part way gone: {alpha}"
        );

        assert!(
            fill.advance(FILL_MS),
            "the frame that completes the fill still runs"
        );
        assert_eq!(fill.phase_of(0), Phase::Built);
        assert_eq!(fill.drop_of(0), 0.0);
        assert_eq!(fill.ghost_alpha(0), None);
        assert!(!fill.advance(16.0), "a built map has nothing moving");
    }

    #[test]
    fn reduced_motion_builds_a_fill_at_once() {
        let mut fill = ScanFill::default();
        fill.load(1);
        fill.apply(Stage::Skeleton, &[]);
        assert!(fill.fill(0, true));
        assert_eq!(fill.phase_of(0), Phase::Built);
        assert!(!fill.advance(16.0));
    }

    #[test]
    fn a_settled_map_never_animates_a_scan() {
        let mut fill = ScanFill::default();
        fill.load(2);
        fill.apply(Stage::Done, &[]);
        assert!(fill.fill(0, false));
        assert_eq!(fill.phase_of(0), Phase::Built);
        fill.apply(Stage::Skeleton, &[]);
        assert_eq!(
            fill.phase_of(0),
            Phase::Built,
            "a rescan keeps the map where it stands"
        );
        assert_eq!(fill.phase_of(1), Phase::Built);
    }

    #[test]
    fn a_status_keeps_a_fill_that_is_running() {
        let mut fill = ScanFill::default();
        fill.load(2);
        fill.apply(Stage::Skeleton, &[]);
        fill.fill(0, false);
        fill.apply(Stage::Fill, &[]);
        assert_eq!(fill.phase_of(0), Phase::Filling(0.0));
        assert_eq!(fill.phase_of(1), Phase::Outline);
    }

    #[test]
    fn a_machine_the_plant_lacks_cannot_fill() {
        let mut fill = ScanFill::default();
        fill.load(1);
        assert!(!fill.fill(4, false));
        assert_eq!(fill.ghost_alpha(4), None);
        assert_eq!(fill.drop_of(4), 0.0);
        assert!(!fill.hidden(4));
    }

    #[test]
    fn an_outline_is_one_loop_of_four_strokes_that_fades_with_its_fill() {
        let mut fill = ScanFill::default();
        fill.load(1);
        fill.apply(Stage::Skeleton, &[]);
        let outline = fill.ghosts(&ONE, 2.0);
        assert_eq!(outline.len(), 4);
        assert!(outline.iter().all(|beam| {
            u32::from(beam.tint) == TINT_GHOST
                && beam.alpha == GHOST_ALPHA
                && beam.width_px == GHOST_PX * 2.0
        }));
        assert_eq!(
            outline[0].to, outline[1].from,
            "the strokes meet at each corner"
        );
        assert_eq!(outline[3].to, outline[0].from, "the loop closes");

        fill.fill(0, false);
        fill.advance(FILL_MS / 2.0);
        let half = fill.ghosts(&ONE, 1.0);
        assert!(
            half.iter()
                .all(|beam| beam.alpha > 0.0 && beam.alpha < GHOST_ALPHA)
        );

        fill.advance(FILL_MS);
        assert!(fill.ghosts(&ONE, 1.0).is_empty());
    }

    #[test]
    fn a_status_reads_its_stage_and_the_names_it_filled() {
        let frame = r#"{"type":"scan_status","stage":"fill","modules_done":1,"modules_total":5,
            "filled":[{"machine":3,"name":"core","files":2,"symbols":9,"exported":4,"imports":1,
            "exports":["run"],"belts":[]}]}"#;
        let status = parse_status(frame).expect("the frame reads");
        assert_eq!(status.stage, Stage::Fill);
        assert_eq!(
            status.filled,
            vec![Filled {
                name: "core".into()
            }]
        );

        let summary = parse_status(r#"{"stage":"skeleton","modules_done":0,"modules_total":5}"#)
            .expect("the summary reads");
        assert_eq!(summary.stage, Stage::Skeleton);
        assert!(summary.filled.is_empty());

        let waiting = parse_status(r#"{"type":"scan_status","stage":"waiting"}"#)
            .expect("a waiting scan reads");
        assert_eq!(waiting.stage, Stage::Waiting);

        assert!(parse_status(r#"{"stage":"reticulating"}"#).is_err());
        assert!(Stage::Failed.ended() && Stage::Done.ended() && !Stage::States.ended());
        assert!(!Stage::Waiting.ended(), "a waiting scan has not ended");
    }

    #[test]
    fn a_waiting_codebase_draws_nothing_until_its_scan_starts() {
        let mut fill = ScanFill::default();
        fill.load(0);
        fill.apply(Stage::Waiting, &[]);
        assert!(
            fill.ghosts(&[], 1.0).is_empty(),
            "a blank map has no outlines"
        );

        fill.load(1);
        fill.apply(Stage::Skeleton, &[]);
        assert!(fill.hidden(0), "the scan's first machine is an outline");
    }

    impl ScanFill {
        fn phase_of(&self, index: usize) -> Phase {
            self.phases[index]
        }
    }
}
