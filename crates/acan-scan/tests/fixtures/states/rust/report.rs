// A decoy: a status-like enum that no code moves between states.
pub enum ReportStatus {
    Draft,
    Final,
}

pub enum Colour {
    Red,
    Blue,
}

pub fn draft() -> ReportStatus {
    ReportStatus::Draft
}

pub fn tint(colour: Colour) -> Colour {
    match colour {
        Colour::Red => Colour::Blue,
        Colour::Blue => Colour::Blue,
    }
}
