//! The order's state, and the transitions the scanner should find.

pub enum OrderState {
    Open,
    Paid,
    Shipped,
}

pub struct Order {
    pub state: OrderState,
}

impl Order {
    pub fn pay(&mut self) {
        match self.state {
            OrderState::Open => self.state = OrderState::Paid,
            _ => {}
        }
    }

    pub fn reopen(state: OrderState) -> OrderState {
        match state {
            OrderState::Shipped => OrderState::Open,
            other => other,
        }
    }
}
