//! Literal tags: the version string on every top-level envelope and the fixed provenance labels.
//!
//! A tag serializes as one exact string and refuses any other on decode, so a reader built for
//! `plant-view/1` fails loudly on `plant-view/2` instead of guessing.

/// Defines a zero-sized type that serializes as one fixed string and decodes only that string.
macro_rules! literal_tag {
    ($(#[$meta:meta])* $name:ident = $tag:literal, $what:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name;

        impl $name {
            /// The exact string this tag carries.
            pub const TAG: &'static str = $tag;
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(Self::TAG)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let got = String::deserialize(deserializer)?;
                if got == Self::TAG {
                    Ok(Self)
                } else {
                    Err(serde::de::Error::custom(format_args!(
                        concat!("unsupported ", $what, " {:?}, expected {:?}"),
                        got,
                        Self::TAG
                    )))
                }
            }
        }

        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "const": $tag })
            }
        }
    };
}

pub(crate) use literal_tag;

literal_tag!(
    /// Version of [`crate::PlantView`].
    PlantViewV1 = "plant-view/1", "version"
);
literal_tag!(
    /// Version of [`crate::ScopeDraft`].
    ScopeDraftV1 = "acan-scope-draft/1", "version"
);
literal_tag!(
    /// Version of [`crate::Scope`].
    ScopeV1 = "acan-scope/1", "version"
);
literal_tag!(
    /// Version of [`crate::ScopeConfirmation`].
    ScopeConfirmationV1 = "acan-scope-confirmation/1", "version"
);
literal_tag!(
    /// Schema version of every [`crate::LedgerEvent`] line in `.acan/rulings.log`.
    LedgerV1 = "acan-ledger/1", "version"
);
literal_tag!(
    /// Version of [`crate::ProbeCorpus`], the protected probe file.
    ProbeCorpusV1 = "acan-probes/1", "version"
);
literal_tag!(
    /// Version of [`crate::AppContract`], the protected `.acan/app.json`.
    AppContractV1 = "acan-app/1", "version"
);
literal_tag!(
    /// The runner contract a fixture and an app contract are written against.
    RunnerContractV1 = "acan-runner/1", "runner contract"
);
literal_tag!(
    /// Version of [`crate::Run`].
    RunV1 = "acan-run/1", "version"
);
literal_tag!(
    /// Version of [`crate::Observation`].
    ObservationV1 = "acan-observation/1", "version"
);
literal_tag!(
    /// Version of [`crate::DiscrepancyEpisode`].
    EpisodeV1 = "acan-episode/1", "version"
);
literal_tag!(
    /// Version of [`crate::Contest`].
    ContestV1 = "acan-contest/1", "version"
);
literal_tag!(
    /// Version of [`crate::Receipt`].
    ReceiptV1 = "acan-receipt/1", "version"
);
literal_tag!(
    /// Version of every MCP request, response and error envelope.
    McpV1 = "acan-mcp/1", "version"
);
literal_tag!(
    /// Version of [`crate::ContrastExample`], a retained ruling example.
    ContrastExampleV1 = "acan-contrast-example/1", "version"
);
literal_tag!(
    /// Version of [`crate::RepairRound`].
    RepairRoundV1 = "acan-repair-round/1", "version"
);
literal_tag!(
    /// Version of every twin-probe request, response and error envelope.
    TwinV1 = "acan-twin/1", "version"
);
literal_tag!(
    /// Version of [`crate::Card`] and [`crate::CardRequest`].
    CardV1 = "acan-card/1", "version"
);
literal_tag!(
    /// Version of [`crate::CaptionCatalog`]. Version 2 gives each template the classes and states
    /// it describes.
    CaptionCatalogV2 = "acan-caption-catalog/2", "version"
);
