//! The machine models. Each kind has one file here. Every model is pure and deterministic, centred on the origin
//! with its base on z = 0.

mod assembler;
// `crate` is a reserved word, so the crate's file cannot be named `crate.rs`.
mod crate_box;
mod gate;
mod intake;
mod splitter;
mod tank;
mod valve;

use serde::{Deserialize, Serialize};

use crate::model::Model;

/// The machine kinds: sixteen named kinds and the assembler fallback.
///
/// The names are the wire names in the plant JSON. A name this build does not know reads as
/// [`Kind::Assembler`], so an older client draws a newer kind as the plain workbench.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Entry points: servers, app bootstrap, main. A tall unit with a side panel of lights and four stacked trays.
    Intake,
    /// Auth, sessions, permissions, guards. A tall auth post pair with a turnstile hub, a badge reader and a lintel lamp.
    Gate,
    /// Data access, persistence, caching, migrations. A relational database: a squat cylinder stack with banded seams, a gauge and pipe stubs.
    Tank,
    /// Middleware, proxies, gateways. A round hub with four chutes and a crown dial.
    Splitter,
    /// Rate limits, throttles, quotas. A small belt fitting: a body with a handwheel.
    Valve,
    /// Generated code, vendored output, tests, and machines with no files. A plain box.
    Crate,
    /// Routes, controllers, handlers, endpoints. An angled panel on a frame with a grid of jack sockets.
    Switchboard,
    /// Web frontend: components, pages, screens, styles. A tall slim cabinet with an angled glass screen.
    Kiosk,
    /// Models, schemas, types, DTOs, protobuf. A two-part casting mould with a base plate and a hinged top.
    Mould,
    /// Background jobs, queues, workers, cron. A long belt conveyor on two leg frames.
    Conveyor,
    /// Events, messaging, pub/sub, hooks. A loudspeaker horn on a post.
    Horn,
    /// Config, settings, environment, constants. A wall switch cabinet with a door open on two hinges.
    Switchbox,
    /// Infra, deploy, containers, IaC, CI. A gantry crane on two A-frame legs.
    Gantry,
    /// CLI, commands, scripts, tools. A control pedestal with a lever and two push buttons.
    Console,
    /// External API clients, SDKs, integrations. A dish antenna on a mast.
    Antenna,
    /// Shared utilities, helpers, common, lib. A shop tool rack with a sloped tray and a pegboard back.
    Toolrack,
    /// Services, business logic, and anything no other rule names: the fallback. A stack of boards with a
    /// gauge, a knob, two dials and a lamp. It is last so that `serde(other)` can read unknown names as it.
    #[default]
    #[serde(other)]
    Assembler,
}

impl Kind {
    /// Every kind, in declaration order.
    pub const ALL: [Kind; 17] = [
        Kind::Intake,
        Kind::Gate,
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
        Kind::Assembler,
    ];
}

/// A model's size in plan units. The model is centred on the origin, so its plan runs from `-w / 2` to `w / 2`
/// and from `-d / 2` to `d / 2`, and it stands `top` high.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Footprint {
    /// Width along x.
    pub w: f32,
    /// Depth along y.
    pub d: f32,
    /// Height of the model's top above the floor.
    pub top: f32,
}

/// The model for `kind`. The same kind always gives the same model.
#[must_use]
pub fn model(kind: Kind) -> Model {
    match kind {
        Kind::Intake => intake::build(),
        Kind::Gate => gate::build(),
        Kind::Assembler => assembler::build(),
        Kind::Tank => tank::build(),
        Kind::Splitter => splitter::build(),
        Kind::Valve => valve::build(),
        Kind::Crate => crate_box::build(),
        // The sixteen new kinds draw as the assembler until their figures land (kinds-expansion.md).
        Kind::Switchboard
        | Kind::Kiosk
        | Kind::Mould
        | Kind::Conveyor
        | Kind::Horn
        | Kind::Switchbox
        | Kind::Gantry
        | Kind::Console
        | Kind::Antenna
        | Kind::Toolrack => assembler::build(),
    }
}

/// The footprint of `kind`, from the kit's table.
#[must_use]
pub fn footprint(kind: Kind) -> Footprint {
    match kind {
        Kind::Intake => Footprint {
            w: 50.0,
            d: 96.0,
            top: 30.0,
        },
        Kind::Gate => Footprint {
            w: 44.0,
            d: 74.0,
            top: 40.0,
        },
        Kind::Assembler => Footprint {
            w: 96.0,
            d: 86.0,
            top: 66.0,
        },
        Kind::Tank => Footprint {
            w: 78.0,
            d: 62.0,
            top: 56.0,
        },
        Kind::Splitter => Footprint {
            w: 86.0,
            d: 86.0,
            top: 34.0,
        },
        Kind::Valve => Footprint {
            w: 18.0,
            d: 18.0,
            top: 16.0,
        },
        Kind::Crate => Footprint {
            w: 40.0,
            d: 40.0,
            top: 30.0,
        },
        // Until their figures land, the new kinds take the assembler's footprint.
        Kind::Switchboard
        | Kind::Kiosk
        | Kind::Mould
        | Kind::Conveyor
        | Kind::Horn
        | Kind::Switchbox
        | Kind::Gantry
        | Kind::Console
        | Kind::Antenna
        | Kind::Toolrack => footprint(Kind::Assembler),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::tests::{check_closed, check_outward};
    use crate::model::{Detail, Ink, Surface};

    /// The triangles a model draws at every level of detail.
    fn triangles(model: &Model) -> usize {
        model.meshes.iter().map(|mesh| mesh.indices.len() / 3).sum()
    }

    #[test]
    fn every_model_is_deterministic() {
        for kind in Kind::ALL {
            assert_eq!(model(kind), model(kind), "{kind:?}");
        }
    }

    #[test]
    fn every_mesh_is_a_union_of_closed_solids_with_outward_normals() {
        for kind in Kind::ALL {
            let built = model(kind);
            for (level, mesh) in built.meshes.iter().enumerate() {
                check_closed(mesh).unwrap_or_else(|e| panic!("{kind:?} level {level}: {e}"));
                check_outward(mesh).unwrap_or_else(|e| panic!("{kind:?} level {level}: {e}"));
                assert_eq!(
                    mesh.positions.len(),
                    mesh.normals.len(),
                    "{kind:?} level {level}"
                );
                assert_eq!(
                    mesh.positions.len(),
                    mesh.surfaces.len(),
                    "{kind:?} level {level}"
                );
            }
        }
    }

    #[test]
    fn bounds_contain_every_vertex_line_and_dot_and_fit_the_footprint() {
        for kind in Kind::ALL {
            let built = model(kind);
            let fp = footprint(kind);
            let bounds = built.bounds;
            let inside =
                |p: [f32; 3]| (0..3).all(|a| p[a] >= bounds.min[a] && p[a] <= bounds.max[a]);
            for mesh in &built.meshes {
                assert!(mesh.positions.iter().all(|&p| inside(p)), "{kind:?}");
            }
            for line in &built.lines {
                assert!(line.points.iter().all(|&p| inside(p)), "{kind:?}");
            }
            assert!(bounds.min[2] >= 0.0, "{kind:?} sits on the ground");
            assert!(
                bounds.max[2] <= fp.top + 1e-3,
                "{kind:?} top {} above footprint {}",
                bounds.max[2],
                fp.top
            );
            assert!(
                bounds.min[0] >= -fp.w / 2.0 - 1e-3 && bounds.max[0] <= fp.w / 2.0 + 1e-3,
                "{kind:?} x"
            );
            assert!(
                bounds.min[1] >= -fp.d / 2.0 - 1e-3 && bounds.max[1] <= fp.d / 2.0 + 1e-3,
                "{kind:?} y"
            );
        }
    }

    #[test]
    fn every_model_stays_within_the_budget_at_close() {
        for kind in Kind::ALL {
            let built = model(kind);
            let total = triangles(&built);
            println!(
                "{kind:?}: triangles always {}, near {}, close {}, total {total}; lines {}, dots {}",
                built.meshes[0].indices.len() / 3,
                built.meshes[1].indices.len() / 3,
                built.meshes[2].indices.len() / 3,
                built.lines.len(),
                built.dots.len(),
            );
            assert!(total <= 4000, "{kind:?} draws {total} triangles");
        }
    }

    #[test]
    fn wire_names_are_snake_case_and_unknown_names_read_as_assembler() {
        use serde::Deserialize;
        use serde::de::IntoDeserializer;
        let read = |name: &str| -> Result<Kind, serde::de::value::Error> {
            Kind::deserialize(name.into_deserializer())
        };
        assert_eq!(read("switchboard").unwrap(), Kind::Switchboard);
        assert_eq!(read("toolrack").unwrap(), Kind::Toolrack);
        assert_eq!(read("crate").unwrap(), Kind::Crate);
        assert_eq!(read("kind_from_the_future").unwrap(), Kind::Assembler);
        assert_eq!(Kind::default(), Kind::Assembler);
    }

    /// Only agents are saturated, and amber is reserved for waiting on the user.
    fn saturated(ink: Ink) -> bool {
        matches!(
            ink,
            Ink::Red | Ink::Green | Ink::Blue | Ink::Lit | Ink::Agent(_)
        )
    }

    #[test]
    fn no_model_draws_a_saturated_colour_at_any_detail() {
        for kind in Kind::ALL {
            let built = model(kind);
            for level in [Detail::Always, Detail::Near, Detail::Close] {
                for surface in &built.meshes[level as usize].surfaces {
                    let is_saturated = match *surface {
                        Surface::Paper | Surface::Glass => false,
                        Surface::Lit => true,
                        Surface::Ink(ink) => saturated(ink),
                    };
                    assert!(!is_saturated, "{kind:?} {level:?} surface {surface:?}");
                }
                for line in built.lines.iter().filter(|line| line.detail == level) {
                    assert!(
                        !saturated(line.tone),
                        "{kind:?} {level:?} line {:?}",
                        line.tone
                    );
                }
                for dot in built.dots.iter().filter(|dot| dot.detail == level) {
                    assert!(
                        !saturated(dot.tone),
                        "{kind:?} {level:?} dot {:?}",
                        dot.tone
                    );
                }
            }
        }
    }
}
